//! The profile directory: `GET /api/v1/directory`, its operator
//! toggle, the eligibility filtering, ordering, paging and the signed-in
//! viewer's block/mute/domain-block exclusions.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::{Account, RemoteAccountData};
use plamenu_db::instance_settings::SettingsUpdate;
use plamenu_db::status::NewLocalStatus;
use plamenu_db::{
    PgPool, account, account_domain_block, block, instance_settings, mute, oauth, status, user,
};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

/// A stored remote account on `domain`, without real key generation.
async fn remote_account(pool: &PgPool, domain: &str, username: &str) -> Account {
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
            shared_inbox_url: "",
            public_key_pem: "pub",
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

async fn make_discoverable(pool: &PgPool, account_id: i64) {
    sqlx::query!(
        "UPDATE accounts SET discoverable = true WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Opts an account out of discovery (fresh local accounts are discoverable by
/// default now — see migration 0127), so the exclusion cases test something.
async fn make_undiscoverable(pool: &PgPool, account_id: i64) {
    sqlx::query!(
        "UPDATE accounts SET discoverable = false WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A discoverable local account, the directory's baseline inhabitant.
async fn listed_account(pool: &PgPool, username: &str) -> Account {
    let account = create_local_account(pool, username, username).await;
    make_discoverable(pool, account.id).await;
    account
}

async fn set_directory_enabled(pool: &PgPool, enabled: bool) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        SettingsUpdate {
            profile_directory: enabled,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

async fn get(pool: &PgPool, uri: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response: axum::response::Response = test_app(pool.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn usernames(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("array body")
        .iter()
        .map(|entity| entity["acct"].as_str().unwrap())
        .collect()
}

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "directory-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write",
    )
    .await
    .unwrap();
    (account, token)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_gated_on_operator_setting(pool: PgPool) {
    // On by default (Mastodon's `profile_directory: true`), empty until
    // someone opts in.
    let (status, body) = get(&pool, "/api/v1/directory", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    set_directory_enabled(&pool, false).await;
    let (status, body) = get(&pool, "/api/v1/directory", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_lists_only_eligible_discoverable_accounts(pool: PgPool) {
    listed_account(&pool, "alice").await;
    // Explicitly opted out of discovery.
    let bob = create_local_account(&pool, "bob", "bob").await;
    make_undiscoverable(&pool, bob.id).await;
    // Opted in but moderated or migrated away.
    let carol = listed_account(&pool, "carol").await;
    account::suspend(&pool, carol.id, "local").await.unwrap();
    let dave = listed_account(&pool, "dave").await;
    account::silence(&pool, dave.id).await.unwrap();
    let eve = listed_account(&pool, "eve").await;
    account::set_moved_to(&pool, eve.id, Some("https://elsewhere.example/users/eve"))
        .await
        .unwrap();
    // Opted in but the user is still awaiting registration approval.
    let (frank, _) = user_with_token(&pool, "frank").await;
    make_discoverable(&pool, frank.id).await;
    sqlx::query!(
        "UPDATE users SET approved = false WHERE account_id = $1",
        frank.id
    )
    .execute(&pool)
    .await
    .unwrap();
    // Discoverable remote accounts are listed too.
    remote_account(&pool, "remote.example", "gina").await;

    let (status, body) = get(&pool, "/api/v1/directory", None).await;
    assert_eq!(status, StatusCode::OK);
    let mut listed = usernames(&body);
    listed.sort_unstable();
    assert_eq!(listed, ["alice", "gina@remote.example"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_orders_by_activity_or_newness(pool: PgPool) {
    let alice = listed_account(&pool, "alice").await;
    let bob = listed_account(&pool, "bob").await;
    // Never posted.
    listed_account(&pool, "carol").await;
    status::create_local(
        &pool,
        NewLocalStatus::new(bob.id, "<p>1</p>", "public", None),
    )
    .await
    .unwrap();
    let newest = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>2</p>", "public", None),
    )
    .await
    .unwrap();

    // Default (`active`): most recently posted first, never-posted last.
    let (status_code, body) = get(&pool, "/api/v1/directory", None).await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(usernames(&body), ["alice", "bob", "carol"]);

    // The Account entity dates the last post (a bare date, Mastodon-style).
    let at = newest.created_at;
    let expected = format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    );
    assert_eq!(body[0]["last_status_at"], json!(expected));
    assert_eq!(body[2]["last_status_at"], Value::Null);

    // `order=new`: newest accounts first, posting activity irrelevant.
    let (_, body) = get(&pool, "/api/v1/directory?order=new", None).await;
    assert_eq!(usernames(&body), ["carol", "bob", "alice"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_local_flag_and_offset_paging(pool: PgPool) {
    listed_account(&pool, "alice").await;
    listed_account(&pool, "bob").await;
    remote_account(&pool, "remote.example", "gina").await;

    let (_, body) = get(&pool, "/api/v1/directory?local=true&order=new", None).await;
    assert_eq!(usernames(&body), ["bob", "alice"]);

    let (_, body) = get(&pool, "/api/v1/directory?order=new&limit=1", None).await;
    assert_eq!(usernames(&body), ["gina@remote.example"]);
    let (_, body) = get(&pool, "/api/v1/directory?order=new&limit=2&offset=1", None).await;
    assert_eq!(usernames(&body), ["bob", "alice"]);
    let (_, body) = get(&pool, "/api/v1/directory?order=new&offset=3", None).await;
    assert_eq!(body, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_hides_blocked_muted_and_domain_blocked_for_the_viewer(pool: PgPool) {
    let (viewer, token) = user_with_token(&pool, "viewer").await;
    // The viewer is just the observer here, not a directory inhabitant.
    make_undiscoverable(&pool, viewer.id).await;
    let blocked = listed_account(&pool, "blocked").await;
    let blocker = listed_account(&pool, "blocker").await;
    let muted = listed_account(&pool, "muted").await;
    let once_muted = listed_account(&pool, "once-muted").await;
    listed_account(&pool, "bystander").await;
    remote_account(&pool, "blocked.example", "hidden").await;

    block::create(&pool, viewer.id, blocked.id, None)
        .await
        .unwrap();
    block::create(&pool, blocker.id, viewer.id, None)
        .await
        .unwrap();
    mute::upsert(&pool, viewer.id, muted.id, true, None)
        .await
        .unwrap();
    // An expired mute no longer hides anyone.
    mute::upsert(
        &pool,
        viewer.id,
        once_muted.id,
        true,
        Some(OffsetDateTime::now_utc() - Duration::hours(1)),
    )
    .await
    .unwrap();
    account_domain_block::create(&pool, viewer.id, "blocked.example")
        .await
        .unwrap();

    // Anonymous readers see everyone.
    let (_, body) = get(&pool, "/api/v1/directory", None).await;
    assert_eq!(usernames(&body).len(), 6);

    let (status, body) = get(&pool, "/api/v1/directory", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let mut listed = usernames(&body);
    listed.sort_unstable();
    assert_eq!(listed, ["bystander", "once-muted"]);
}
