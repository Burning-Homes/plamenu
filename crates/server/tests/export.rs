//! Integration tests for CSV data export: each of the six download
//! endpoints reproduces Mastodon's export files byte-for-byte, with local
//! accounts rendered as `username@{local_domain}` and remote ones as their
//! stored `username@domain`. Ordering follows Mastodon (newest relationship
//! first); comma-bearing list titles are quoted.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu_db::account::{Account, RemoteAccountData};
use plamenu_db::status::{NewLocalStatus, NewRemoteStatus};
use plamenu_db::{
    PgPool, account, account_domain_block, block, bookmark, follow, list, mute, status, user,
};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

// ---- HTTP harness ------------------------------------------------------

struct Resp {
    status: StatusCode,
    location: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    set_cookie: Option<String>,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let header = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let status = response.status();
    let location = header(header::LOCATION);
    let content_type = header(header::CONTENT_TYPE);
    let content_disposition = header(header::CONTENT_DISPOSITION);
    let set_cookie = header(header::SET_COOKIE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        content_type,
        content_disposition,
        set_cookie,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

/// The `name=value` head of a `Set-Cookie`, ready to send back as `Cookie`.
fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

/// Signs `alice` in and returns the session cookie header.
async fn login(app: &Router) -> String {
    let body = serde_urlencoded::to_string([("email", EMAIL), ("password", PASSWORD)]).unwrap();
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    cookie_pair(
        resp.set_cookie
            .as_ref()
            .expect("login sets a session cookie"),
    )
    .to_owned()
}

// ---- Fixtures ----------------------------------------------------------

async fn seed_alice(pool: &PgPool) -> Account {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(EMAIL), &hash)
        .await
        .unwrap();
    account
}

async fn seed_remote(pool: &PgPool, username: &str, domain: &str) -> Account {
    let uri = format!("https://{domain}/users/{username}");
    let inbox = format!("{uri}/inbox");
    let shared_inbox = format!("https://{domain}/inbox");
    let key_id = format!("{uri}#main-key");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &inbox,
            shared_inbox_url: &shared_inbox,
            public_key_pem: "",
            public_key_id: &key_id,
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: true,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn following_and_lists_export(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = seed_remote(&pool, "carol", "remote.example").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;

    // Follows, oldest first — export orders newest first (follow id DESC).
    // The bob follow carries per-follow settings (M32); the language list
    // joins with ", " so the cell must come out CSV-quoted.
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    follow::update_settings(
        &pool,
        alice.id,
        bob.id,
        Some(false),
        Some(false),
        Some(true),
        Some(&["en".to_owned(), "de".to_owned()]),
    )
    .await
    .unwrap();
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();
    // A pending follow request must NOT appear in the export.
    follow::create_request(&pool, alice.id, dave.id, None)
        .await
        .unwrap();

    // Two lists; the first title carries a comma so it must be CSV-quoted.
    let friends = list::create(&pool, alice.id, "Friends, Family", "list", false)
        .await
        .unwrap();
    list::add_members(&pool, friends.id, alice.id, &[bob.id, carol.id])
        .await
        .unwrap()
        .unwrap();
    let work = list::create(&pool, alice.id, "Work", "list", false)
        .await
        .unwrap();
    list::add_members(&pool, work.id, alice.id, &[bob.id])
        .await
        .unwrap()
        .unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let follows = get(
        &app,
        "/settings/export/following_accounts.csv",
        Some(&cookie),
    )
    .await;
    assert_eq!(follows.status, StatusCode::OK);
    assert_eq!(
        follows.content_type.as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        follows.content_disposition.as_deref(),
        Some("attachment; filename=\"following_accounts.csv\"")
    );
    assert_eq!(
        follows.body,
        "Account address,Show boosts,Notify on new posts,Languages,Show replies\n\
         carol@remote.example,true,false,,true\n\
         bob@plamenu.test,false,true,\"en, de\",false\n"
    );

    let lists = get(&app, "/settings/export/lists.csv", Some(&cookie)).await;
    assert_eq!(lists.status, StatusCode::OK);
    assert_eq!(
        lists.body,
        "\"Friends, Family\",bob@plamenu.test\n\
         \"Friends, Family\",carol@remote.example\n\
         Work,bob@plamenu.test\n"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn blocks_mutes_domains_export(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = seed_remote(&pool, "carol", "remote.example").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let erin = seed_remote(&pool, "erin", "spam.example").await;

    block::create(&pool, alice.id, dave.id, None).await.unwrap();
    block::create(&pool, alice.id, erin.id, None).await.unwrap();

    // hide_notifications differs per row; an expired mute must be excluded.
    mute::upsert(&pool, alice.id, carol.id, false, None)
        .await
        .unwrap();
    mute::upsert(&pool, alice.id, bob.id, true, None)
        .await
        .unwrap();
    mute::upsert(
        &pool,
        alice.id,
        dave.id,
        true,
        Some(OffsetDateTime::now_utc() - Duration::hours(1)),
    )
    .await
    .unwrap();

    account_domain_block::create(&pool, alice.id, "spam.example")
        .await
        .unwrap();
    account_domain_block::create(&pool, alice.id, "ads.example")
        .await
        .unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let blocks = get(&app, "/settings/export/blocked_accounts.csv", Some(&cookie)).await;
    assert_eq!(blocks.status, StatusCode::OK);
    assert_eq!(blocks.body, "erin@spam.example\ndave@plamenu.test\n");

    let mutes = get(&app, "/settings/export/muted_accounts.csv", Some(&cookie)).await;
    assert_eq!(mutes.status, StatusCode::OK);
    assert_eq!(
        mutes.body,
        "Account address,Hide notifications\n\
         bob@plamenu.test,true\n\
         carol@remote.example,false\n"
    );

    let domains = get(&app, "/settings/export/blocked_domains.csv", Some(&cookie)).await;
    assert_eq!(domains.status, StatusCode::OK);
    assert_eq!(domains.body, "ads.example\nspam.example\n");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bookmarks_export(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = seed_remote(&pool, "carol", "remote.example").await;

    // A local status (URI derived from the author + id) and a remote one
    // (its stored URI is used verbatim).
    let local = status::create_local(
        &pool,
        NewLocalStatus::new(bob.id, "<p>hi</p>", "public", None),
    )
    .await
    .unwrap();
    let remote = status::upsert_remote(
        &pool,
        NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/notes/7",
            account_id: carol.id,
            content: "<p>yo</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    bookmark::create(&pool, alice.id, local.id).await.unwrap();
    bookmark::create(&pool, alice.id, remote.id).await.unwrap();

    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/settings/export/bookmarks.csv", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.body,
        format!(
            "https://remote.example/notes/7\n\
             https://plamenu.test/users/bob/statuses/{}\n",
            local.id
        )
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn export_requires_login(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool);

    let resp = get(&app, "/settings/export/following_accounts.csv", None).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.location.as_deref(), Some("/login"));
    assert!(resp.body.is_empty(), "no CSV leaks to anonymous callers");
}

/// A dataset larger than the streaming page size arrives
/// complete — the download pages through the whole set in keyset chunks, so a
/// large account is never silently truncated and no page boundary loses or
/// repeats a row.
#[sqlx::test(migrations = "../db/migrations")]
async fn large_export_streams_every_row_without_truncation(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // 2,500 rows — several 1,000-row pages — seeded in one SQL statement.
    sqlx::query(
        "INSERT INTO account_domain_blocks (id, account_id, domain)
         SELECT g, $1, 'blocked-' || g || '.example'
         FROM generate_series(1, 2500) AS g",
    )
    .bind(alice.id)
    .execute(&pool)
    .await
    .unwrap();
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/settings/export/blocked_domains.csv", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    let mut lines: Vec<&str> = resp.body.lines().collect();
    assert_eq!(lines.len(), 2500, "every row arrives, none truncated");
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(lines.len(), 2500, "no row is repeated at a page boundary");
}

/// The CSV downloads draw from the per-account web-maintenance
/// admission budget, so a buggy or malicious client cannot hammer the
/// dataset-walking queries without bound.
#[sqlx::test(migrations = "../db/migrations")]
async fn export_downloads_draw_from_the_maintenance_budget(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let budget = plamenu::rate_limit::WEB_MAINTENANCE_BUDGET;
    let mut last = None;
    for _ in 0..=budget {
        last = Some(
            get(&app, "/settings/export/blocked_domains.csv", Some(&cookie))
                .await
                .status,
        );
    }
    assert_eq!(
        last,
        Some(StatusCode::TOO_MANY_REQUESTS),
        "the request past the budget is refused"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_export_file_is_not_found(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/settings/export/passwords.csv", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn export_page_lists_downloads(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool);
    let cookie = login(&app).await;

    let resp = get(&app, "/settings/export", Some(&cookie)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.body
            .contains("/settings/export/following_accounts.csv")
    );
    assert!(resp.body.contains("/settings/export/bookmarks.csv"));
    assert!(resp.body.contains("Import &amp; export"));
}
