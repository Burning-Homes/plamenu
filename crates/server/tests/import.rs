//! Integration tests for CSV data import. Uploads parse and coerce
//! like Mastodon's `Form::Import` (own-header vs headerless files, boolean
//! casting, type mismatch flagging), confirmation schedules the import, and the
//! worker applies it through the normal follow/block/mute/list actions —
//! deleting the rows that succeed and leaving the failures, which the
//! `{type}_failures.csv` re-emits.

mod common;

use std::fmt::Write as _;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu::AppState;
use plamenu_db::account::{Account, RemoteAccountData};
use plamenu_db::{PgPool, account, block, bulk_import, follow, list, mute, user};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

// ---- HTTP harness ------------------------------------------------------

struct Resp {
    status: StatusCode,
    location: Option<String>,
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
    let set_cookie = header(header::SET_COOKIE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        set_cookie,
        // Fluent brackets every interpolated value in directional isolates
        // (U+2068 FSI / U+2069 PDI), invisible to a reader but not to
        // `contains`. Drop them so assertions read like the page does.
        body: String::from_utf8_lossy(&bytes).replace(['\u{2068}', '\u{2069}'], ""),
    }
}

async fn get(app: &Router, uri: &str, cookie: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post_form(app: &Router, uri: &str, cookie: &str, body: String) -> Resp {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

async fn login(app: &Router) -> String {
    let body = serde_urlencoded::to_string([("email", EMAIL), ("password", PASSWORD)]).unwrap();
    let resp = post_form(app, "/login", "", body).await;
    cookie_pair(resp.set_cookie.as_ref().expect("login sets a cookie")).to_owned()
}

/// The CSRF token embedded in a settings page, needed for state-changing POSTs.
fn csrf_of(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

/// Builds a `multipart/form-data` upload body for the import endpoint.
fn upload_body(
    import_type: &str,
    mode: &str,
    csrf: &str,
    filename: &str,
    csv: &str,
) -> (String, Vec<u8>) {
    let boundary = "----plamenuimporttest";
    let mut body = String::new();
    for (name, value) in [("csrf", csrf), ("type", import_type), ("mode", mode)] {
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        );
    }
    let _ = write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"data\"; filename=\"{filename}\"\r\n\
         Content-Type: text/csv\r\n\r\n"
    );
    let mut bytes = body.into_bytes();
    bytes.extend_from_slice(csv.as_bytes());
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), bytes)
}

/// Uploads a CSV; returns the response (a redirect to the review page on
/// success).
async fn upload(
    app: &Router,
    cookie: &str,
    import_type: &str,
    mode: &str,
    filename: &str,
    csv: &str,
) -> Resp {
    let page = get(app, "/settings/export", cookie).await;
    let csrf = csrf_of(&page.body);
    let (content_type, body) = upload_body(import_type, mode, &csrf, filename, csv);
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/web/settings/import")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

/// The import id from a successful upload's `/settings/import/{id}` redirect.
fn import_id(resp: &Resp) -> i64 {
    let location = resp.location.as_ref().expect("upload redirects to review");
    location
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .expect("numeric import id")
}

/// Confirms an import and runs the worker to completion.
async fn confirm_and_run(app: &Router, state: &AppState, cookie: &str, id: i64) {
    let review = get(app, &format!("/settings/import/{id}"), cookie).await;
    let csrf = csrf_of(&review.body);
    let body = serde_urlencoded::to_string([("csrf", csrf.as_str())]).unwrap();
    let confirmed = post_form(
        app,
        &format!("/web/settings/import/{id}/confirm"),
        cookie,
        body,
    )
    .await;
    assert_eq!(confirmed.status, StatusCode::SEE_OTHER);
    let claimed = Box::pin(plamenu::import_worker::run_due(state)).await;
    assert_eq!(claimed, 1, "the confirmed import is claimed and applied");
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
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: &format!("https://{domain}/inbox"),
            public_key_pem: "",
            public_key_id: &format!("{uri}#main-key"),
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

fn state_and_app(pool: PgPool) -> (AppState, Router) {
    let state = common::test_state_with(pool, std::sync::Arc::default());
    let app = plamenu::build_router(state.clone());
    (state, app)
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn upload_creates_unconfirmed_import_and_review_page(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob").await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               @bob@plamenu.test,true,false,\n";
    let resp = upload(
        &app,
        &cookie,
        "following",
        "merge",
        "following_accounts.csv",
        csv,
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let id = import_id(&resp);

    // Stored unconfirmed with the coerced row.
    let import = bulk_import::find_for_account(&pool, alice.id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.state, "unconfirmed");
    assert_eq!(import.total_items, 1);
    assert!(!import.likely_mismatched);

    // The review page offers a confirm button and the row count.
    let review = get(&app, &format!("/settings/import/{id}"), &cookie).await;
    assert_eq!(review.status, StatusCode::OK);
    assert!(review.body.contains("Confirm import"));
    assert!(review.body.contains("following_accounts.csv"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn confirm_runs_worker_and_applies_follows(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = seed_remote(&pool, "carol", "remote.example").await;
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // Two resolvable accounts and one that does not exist (a failure). Bob's
    // row carries per-follow settings (M32) the import must apply.
    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               bob@plamenu.test,false,true,\"en, de\"\n\
               carol@remote.example,true,false,\n\
               ghost@plamenu.test,true,false,\n";
    let resp = upload(
        &app,
        &cookie,
        "following",
        "merge",
        "following_accounts.csv",
        csv,
    )
    .await;
    let id = import_id(&resp);
    Box::pin(confirm_and_run(&app, &state, &cookie, id)).await;

    // Both resolvable follows landed; the ghost stayed a failure. Bob's
    // row settings were applied, carol's absent columns took the defaults.
    let bob_edge = follow::find(&pool, alice.id, bob.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!bob_edge.show_reblogs);
    assert!(bob_edge.notify);
    assert_eq!(
        bob_edge.languages.as_deref(),
        Some(["en".to_owned(), "de".to_owned()].as_slice())
    );
    let carol_edge = follow::find(&pool, alice.id, carol.id)
        .await
        .unwrap()
        .unwrap();
    assert!(carol_edge.show_reblogs);
    assert!(!carol_edge.notify);
    assert_eq!(carol_edge.languages, None);

    let import = bulk_import::find_for_account(&pool, alice.id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.state, "finished");
    assert_eq!(import.total_items, 3);
    assert_eq!(import.processed_items, 3);
    assert_eq!(import.imported_items, 2);
    assert_eq!(import.failure_count(), 1);

    // The failures CSV re-emits only the ghost row, in export shape.
    let failures = get(
        &app,
        &format!("/settings/import/{id}/failures.csv"),
        &cookie,
    )
    .await;
    assert_eq!(failures.status, StatusCode::OK);
    assert_eq!(
        failures.body,
        "Account address,Show boosts,Notify on new posts,Languages,Show replies\n\
         ghost@plamenu.test,true,false,,\n"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_suspended_account_import_applies_no_rows(pool: PgPool) {
    // A confirmed import claimed just before its owner self-deletes must not
    // recreate follows or federate from the tombstone.
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               bob@plamenu.test,true,false,\n";
    let resp = upload(
        &app,
        &cookie,
        "following",
        "merge",
        "following_accounts.csv",
        csv,
    )
    .await;
    let id = import_id(&resp);

    // Confirm (schedule) the import, then claim it — the worker is now holding
    // an `in_progress` import for alice.
    let review = get(&app, &format!("/settings/import/{id}"), &cookie).await;
    let csrf = csrf_of(&review.body);
    let confirm_body = serde_urlencoded::to_string([("csrf", csrf.as_str())]).unwrap();
    let confirmed = post_form(
        &app,
        &format!("/web/settings/import/{id}/confirm"),
        &cookie,
        confirm_body,
    )
    .await;
    assert_eq!(confirmed.status, StatusCode::SEE_OTHER);
    let claimed = bulk_import::claim_scheduled(&pool, 10).await.unwrap();
    assert_eq!(claimed.len(), 1);

    // Alice self-deletes (suspends) after the claim but before the run.
    account::suspend(&pool, alice.id, "local").await.unwrap();

    // Running the already-claimed import cancels it without following bob.
    Box::pin(plamenu::bulk_import::run(&state, &claimed[0]))
        .await
        .unwrap();
    assert!(
        follow::find(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_none(),
        "a suspended account's import must not create follows"
    );
    let import = bulk_import::find_for_account(&pool, alice.id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.state, "finished");
    assert_eq!(import.imported_items, 0);

    // And a still-scheduled import for a suspended account is never claimed.
    let none = Box::pin(plamenu::import_worker::run_due(&state)).await;
    assert_eq!(none, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn headerless_blocks_and_mute_defaults(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // A bare, headerless single-column blocks file still imports.
    let blocks = "@bob@plamenu.test\ncarol@plamenu.test\n";
    let resp = upload(
        &app,
        &cookie,
        "blocking",
        "merge",
        "blocked_accounts.csv",
        blocks,
    )
    .await;
    Box::pin(confirm_and_run(&app, &state, &cookie, import_id(&resp))).await;
    assert!(block::exists(&pool, alice.id, bob.id).await.unwrap());
    assert!(block::exists(&pool, alice.id, carol.id).await.unwrap());

    // A headerless mute file leaves hide_notifications at the default (on).
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let mutes = "dave@plamenu.test\n";
    let resp = upload(
        &app,
        &cookie,
        "muting",
        "merge",
        "muted_accounts.csv",
        mutes,
    )
    .await;
    Box::pin(confirm_and_run(&app, &state, &cookie, import_id(&resp))).await;
    let mute = mute::find_active(&pool, alice.id, dave.id)
        .await
        .unwrap()
        .expect("dave is muted");
    assert!(mute.hide_notifications);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn lists_import_creates_list_and_membership(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let csv = "Friends,bob@plamenu.test\n";
    let resp = upload(&app, &cookie, "lists", "merge", "lists.csv", csv).await;
    Box::pin(confirm_and_run(&app, &state, &cookie, import_id(&resp))).await;

    // The list was created, the follow was ensured, and bob was added.
    let lists = list::owned_by(&pool, alice.id).await.unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].title, "Friends");
    assert!(
        follow::find(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_some()
    );
    let members = list::members_page(&pool, lists[0].id, None, None, None)
        .await
        .unwrap();
    assert_eq!(members, vec![bob.id]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn overwrite_reconciles_removed_follows(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    // Alice already follows both.
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    follow::create(&pool, alice.id, dave.id, None)
        .await
        .unwrap();
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // Overwrite with a file that keeps only bob.
    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               bob@plamenu.test,true,false,\n";
    let resp = upload(
        &app,
        &cookie,
        "following",
        "overwrite",
        "following_accounts.csv",
        csv,
    )
    .await;
    Box::pin(confirm_and_run(&app, &state, &cookie, import_id(&resp))).await;

    assert!(
        follow::find(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        follow::find(&pool, alice.id, dave.id)
            .await
            .unwrap()
            .is_none(),
        "dave, absent from the overwrite file, is unfollowed"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mismatched_type_is_flagged(pool: PgPool) {
    seed_alice(&pool).await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // A mutes file (its header gives it away) uploaded as a following import.
    let csv = "Account address,Hide notifications\nbob@plamenu.test,true\n";
    let resp = upload(
        &app,
        &cookie,
        "following",
        "merge",
        "muted_accounts.csv",
        csv,
    )
    .await;
    let review = get(
        &app,
        &format!("/settings/import/{}", import_id(&resp)),
        &cookie,
    )
    .await;
    assert!(
        review
            .body
            .contains("looks like a different kind of export"),
        "the review page warns about the mismatch"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn over_row_limit_is_rejected(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let mut csv = String::new();
    for i in 0..=20_000 {
        let _ = writeln!(csv, "user{i}@spam.test");
    }
    let resp = upload(
        &app,
        &cookie,
        "blocking",
        "merge",
        "blocked_accounts.csv",
        &csv,
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(
        resp.location.as_deref(),
        Some("/settings/export?error=too_many")
    );
    // Nothing was stored.
    assert!(
        bulk_import::recent_for_account(&pool, alice.id, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn upload_is_refused_at_the_pending_import_cap(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // Fill the per-account in-flight quota with cheap zero-row imports.
    let cap = plamenu::bulk_import::MAX_UNFINISHED_IMPORTS_PER_ACCOUNT;
    for _ in 0..cap {
        let admission = bulk_import::create_with_rows_capped(
            &pool,
            alice.id,
            "blocking",
            false,
            "b.csv",
            false,
            &[],
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        assert!(
            matches!(admission, bulk_import::ImportAdmission::Admitted(_)),
            "seed import is created"
        );
    }
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // A further upload is refused, and no new import is stored.
    let csv = "spam@example.com\n";
    let resp = upload(
        &app,
        &cookie,
        "blocking",
        "merge",
        "blocked_accounts.csv",
        csv,
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(
        resp.location.as_deref(),
        Some("/settings/export?error=too_many_pending")
    );
    assert_eq!(
        bulk_import::recent_for_account(&pool, alice.id, 1000)
            .await
            .unwrap()
            .len(),
        usize::try_from(cap).unwrap()
    );

    // Discarding one frees a slot, so a fresh upload is accepted again.
    let existing = bulk_import::recent_for_account(&pool, alice.id, 1)
        .await
        .unwrap();
    assert!(
        bulk_import::delete_for_account(&pool, alice.id, existing[0].id)
            .await
            .unwrap()
    );
    let ok = upload(
        &app,
        &cookie,
        "blocking",
        "merge",
        "blocked_accounts.csv",
        csv,
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert!(
        ok.location
            .as_deref()
            .unwrap()
            .starts_with("/settings/import/"),
        "a freed slot admits the upload"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn imports_are_scoped_to_their_owner(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob").await;
    let bob_account = create_local_account(&pool, "mallory", "Mallory").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(&pool, bob_account.id, Some("mallory@example.com"), &hash)
        .await
        .unwrap();
    let (_state, app) = state_and_app(pool.clone());

    // Alice uploads an import.
    let alice_cookie = login(&app).await;
    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               bob@plamenu.test,true,false,\n";
    let resp = upload(&app, &alice_cookie, "following", "merge", "f.csv", csv).await;
    let id = import_id(&resp);
    let _ = alice;

    // Mallory cannot see or confirm it.
    let mallory_cookie = {
        let body =
            serde_urlencoded::to_string([("email", "mallory@example.com"), ("password", PASSWORD)])
                .unwrap();
        let resp = post_form(&app, "/login", "", body).await;
        cookie_pair(resp.set_cookie.as_ref().unwrap()).to_owned()
    };
    let review = get(&app, &format!("/settings/import/{id}"), &mallory_cookie).await;
    assert_eq!(review.status, StatusCode::NOT_FOUND);
    let body = serde_urlencoded::to_string([("csrf", "x")]).unwrap();
    let confirm = post_form(
        &app,
        &format!("/web/settings/import/{id}/confirm"),
        &mallory_cookie,
        body,
    )
    .await;
    // CSRF check fails first (403), so the import is untouched either way.
    assert_ne!(confirm.status, StatusCode::SEE_OTHER);
}

/// Finding #51: an authenticated client could POST maximum-size import uploads
/// as fast as it liked, each buffering a body and (up to the per-account cap)
/// persisting thousands of rows. A per-account admission budget, spent *before*
/// the multipart body is buffered, turns the flood away with a `429` once the
/// budget is exhausted — while the accepted uploads never exceed the account's
/// unfinished-import cap.
#[sqlx::test(migrations = "../db/migrations")]
async fn import_uploads_are_admission_limited_per_account(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob").await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // One page fetch for the CSRF token, reused across every upload.
    let page = get(&app, "/settings/export", &cookie).await;
    let csrf = csrf_of(&page.body);
    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               @bob@plamenu.test,true,false,\n";

    let post_upload = |csrf: String| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let (content_type, body) = upload_body("following", "merge", &csrf, "f.csv", csv);
            send(
                &app,
                Request::builder()
                    .method("POST")
                    .uri("/web/settings/import")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
        }
    };

    // The whole budget is admitted as redirects: the first uploads schedule
    // imports, and once the per-account unfinished cap is hit the rest redirect
    // with `too_many_pending` — but every request still spends the budget.
    let budget = plamenu::rate_limit::IMPORT_UPLOAD_BUDGET;
    for _ in 0..budget {
        let resp = post_upload(csrf.clone()).await;
        assert_eq!(
            resp.status,
            StatusCode::SEE_OTHER,
            "within budget: a redirect (scheduled or capped)",
        );
    }

    // The next upload is refused by the admission budget with a `429` — before
    // the body was even buffered.
    let over = post_upload(csrf.clone()).await;
    assert_eq!(over.status, StatusCode::TOO_MANY_REQUESTS);

    // The per-account cap held throughout: never more than the cap of unfinished
    // imports exists, well under the number of requests fired.
    let unfinished = bulk_import::count_unfinished_for_account(&pool, alice.id)
        .await
        .unwrap();
    assert_eq!(
        unfinished,
        plamenu::bulk_import::MAX_UNFINISHED_IMPORTS_PER_ACCOUNT
    );
}

/// The whole Import &amp; export page — both halves plus the archive section —
/// and the review page render in the signed-in account's stored locale. The
/// page is the only place the six import types and the archive states are
/// named for a reader, so a missed catalog lookup shows up here as English.
#[sqlx::test(migrations = "../db/migrations")]
async fn import_and_export_page_uses_stored_russian_locale(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    create_local_account(&pool, "bob", "Bob").await;
    let stored = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, stored.id, Some("ru"))
        .await
        .unwrap();
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/settings/export", &cookie).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"<html lang="ru" dir="ltr""#));
    assert!(
        page.body
            .contains("<title>Импорт и экспорт — Plamenu</title>")
    );
    // Import half: heading, type picker, the size hint and the mode radios.
    assert!(page.body.contains("Что импортировать"));
    assert!(page.body.contains("Список заблокированных доменов"));
    assert!(page.body.contains("До 20 МБ и 20 000 строк."));
    assert!(
        page.body
            .contains("Заменить — оставить только содержимое файла.")
    );
    // Export half and the archive section, including its retention sentence.
    assert!(page.body.contains("Скачайте свои данные"));
    assert!(page.body.contains("Заблокированные аккаунты"));
    assert!(page.body.contains("Запросить архив"));
    assert!(page.body.contains("Полный архив"));
    assert!(!page.body.contains(">Follows<"));
    assert!(!page.body.contains("Request archive"));

    // An uploaded file gets a recent-imports row: type, state and row count,
    // the last through a Russian plural.
    let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
               @bob@plamenu.test,true,false,\n";
    let uploaded = upload(
        &app,
        &cookie,
        "following",
        "merge",
        "following_accounts.csv",
        csv,
    )
    .await;
    let id = import_id(&uploaded);

    let listed = get(&app, "/settings/export", &cookie).await;
    assert!(listed.body.contains("Недавние импорты"));
    assert!(
        listed
            .body
            .contains("Список подписок · Ожидает подтверждения")
    );
    assert!(listed.body.contains("1 строка"));
    assert!(listed.body.contains("Просмотреть"));

    // The review page and its summary.
    let review = get(&app, &format!("/settings/import/{id}"), &cookie).await;
    assert_eq!(review.status, StatusCode::OK);
    assert!(
        review
            .body
            .contains("<title>Просмотр импорта — Plamenu</title>")
    );
    assert!(review.body.contains("Подтвердить импорт"));
    assert!(review.body.contains("Дополнение"));
    assert!(review.body.contains("following_accounts.csv"));
    assert!(!review.body.contains("Confirm import"));
}

/// A refused upload redirects with a stable `?error=` code; the page re-states
/// it from the catalog, and anything else in that parameter renders nothing.
#[sqlx::test(migrations = "../db/migrations")]
async fn upload_refusals_are_codes_restated_in_the_readers_language(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let stored = user::find_by_account_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    user::update_locale(&pool, stored.id, Some("ru"))
        .await
        .unwrap();
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    let refused = upload(&app, &cookie, "following", "merge", "empty.csv", "").await;
    assert_eq!(refused.status, StatusCode::SEE_OTHER);
    assert_eq!(
        refused.location.as_deref(),
        Some("/settings/export?error=empty"),
        "the refusal travels as a code, not a sentence"
    );

    let page = get(&app, "/settings/export?error=empty", &cookie).await;
    assert!(page.body.contains("В этом файле нет строк для импорта."));

    // An unknown code — the only thing an outside caller can put here — is
    // dropped rather than echoed into the alert.
    let injected = get(
        &app,
        "/settings/export?error=Your%20session%20expired%2C%20call%20555",
        &cookie,
    )
    .await;
    assert_eq!(injected.status, StatusCode::OK);
    assert!(!injected.body.contains("call 555"));
    assert!(!injected.body.contains("settings__error"));
}
