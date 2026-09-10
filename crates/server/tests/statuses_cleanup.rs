//! Statuses-cleanup (M38): the budgeted auto-deletion sweep deleting old
//! posts through the federating delete path, the policy exceptions, the
//! cursor rollback hooks on unfav/unpin, and the `/settings/statuses-cleanup`
//! web form.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{actions, delivery, statuses_cleanup as sweep};
use plamenu_db::status::{self, NewLocalStatus};
use plamenu_db::statuses_cleanup::{self, PolicyUpdate};
use plamenu_db::{PgPool, account, favourite, follow, id, job, pin};
use tower::ServiceExt;

const TWO_WEEKS_SECS: i64 = 1_209_600;

/// Lets a remote user follow the local account directly in the database.
async fn add_follower(pool: &PgPool, user: &RemoteUser, followed: i64) {
    let remote = plamenu::remote::store_remote_actor(pool, &user.actor)
        .await
        .unwrap();
    follow::create(pool, remote.id, followed, None)
        .await
        .unwrap();
}

/// Creates a local status, then backdates its snowflake id by `age_secs`
/// (keeping the sequence bits so parallel backdates stay unique).
async fn old_status(pool: &PgPool, account_id: i64, visibility: &str, age_secs: i64) -> i64 {
    let created = status::create_local(
        pool,
        NewLocalStatus::new(account_id, "<p>old</p>", visibility, None),
    )
    .await
    .unwrap();
    let now_ms =
        i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap();
    let old_id = id::id_at(now_ms - age_secs * 1_000) | (created.id & 0xFFFF);
    sqlx::query("UPDATE statuses SET id = $1 WHERE id = $2")
        .bind(old_id)
        .bind(created.id)
        .execute(pool)
        .await
        .unwrap();
    old_id
}

fn default_policy() -> PolicyUpdate {
    PolicyUpdate {
        enabled: true,
        min_status_age: 1_209_600,
        keep_direct: true,
        keep_pinned: true,
        keep_polls: false,
        keep_media: false,
        keep_self_fav: true,
        keep_self_bookmark: true,
        min_favs: None,
        min_reblogs: None,
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sweep_deletes_old_statuses_and_federates(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    add_follower(&pool, &bob, alice.id).await;

    let doomed = old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    let pinned = old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    pin::create(&pool, alice.id, pinned).await.unwrap();
    let fresh = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>fresh</p>", "public", None),
    )
    .await
    .unwrap()
    .id;
    statuses_cleanup::upsert(&pool, alice.id, default_policy())
        .await
        .unwrap();

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    let (deleted, cursor) = sweep::run_once(&state, 0).await;
    assert_eq!(deleted, 1);
    assert_eq!(cursor, alice.id);

    // The old plain status is gone; the pinned and fresh ones survive.
    assert!(status::find_by_id(&pool, doomed).await.unwrap().is_none());
    assert!(status::find_by_id(&pool, pinned).await.unwrap().is_some());
    assert!(status::find_by_id(&pool, fresh).await.unwrap().is_some());

    // The deletion federated: one Delete to the follower's inbox.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, "https://remote.example/inbox");
    assert_eq!(sent[0].activity["type"], "Delete");

    // The scan cursor advanced at least past the deleted status (the run's
    // second pass parks it at the window edge once nothing is left).
    let policy = statuses_cleanup::get(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert!(policy.last_inspected_id.unwrap() >= doomed);

    // A second sweep finds nothing new and parks the cursor at the window
    // edge instead of re-scanning the kept statuses forever.
    let (deleted, _) = sweep::run_once(&state, cursor).await;
    assert_eq!(deleted, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sweep_stops_at_the_run_budget(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    for _ in 0..53 {
        old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    }
    statuses_cleanup::upsert(&pool, alice.id, default_policy())
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    // The run cap is 50 deletions; the remainder waits for the next sweep.
    let (first, cursor) = sweep::run_once(&state, 0).await;
    assert_eq!(first, 50);
    let (second, _) = sweep::run_once(&state, cursor).await;
    assert_eq!(second, 3);
    assert_eq!(status::count_by_account(&pool, alice.id).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn disabled_policy_is_never_swept(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let kept = old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    statuses_cleanup::upsert(
        &pool,
        alice.id,
        PolicyUpdate {
            enabled: false,
            ..default_policy()
        },
    )
    .await
    .unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    let (deleted, _) = sweep::run_once(&state, 0).await;
    assert_eq!(deleted, 0);
    assert!(status::find_by_id(&pool, kept).await.unwrap().is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unfavouriting_own_status_rolls_the_cursor_back(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let own = old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    favourite::create(&pool, alice.id, own, None).await.unwrap();
    statuses_cleanup::upsert(&pool, alice.id, default_policy())
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    // The sweep keeps the self-faved status and parks the cursor past it.
    let (deleted, _) = sweep::run_once(&state, 0).await;
    assert_eq!(deleted, 0);
    let parked = statuses_cleanup::get(&pool, alice.id)
        .await
        .unwrap()
        .unwrap()
        .last_inspected_id
        .unwrap();
    assert!(parked >= own);

    // Un-favouriting it makes it eligible again: the cursor rolls back and
    // the next sweep deletes it.
    actions::unfavourite_status(&state, &alice, own)
        .await
        .unwrap();
    let rolled = statuses_cleanup::get(&pool, alice.id)
        .await
        .unwrap()
        .unwrap()
        .last_inspected_id
        .unwrap();
    assert_eq!(rolled, own);
    let (deleted, _) = sweep::run_once(&state, 0).await;
    assert_eq!(deleted, 1);
    assert!(status::find_by_id(&pool, own).await.unwrap().is_none());
}

// ---- The /settings/statuses-cleanup web form ----------------------------

async fn web_user(pool: &PgPool) -> account::Account {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password("pw").unwrap();
    plamenu_db::user::create(pool, account.id, Some("alice@example.com"), &hash)
        .await
        .unwrap();
    account
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String, Option<String>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), cookie)
}

async fn login(app: &Router) -> String {
    let body =
        serde_urlencoded::to_string([("identifier", "alice@example.com"), ("password", "pw")])
            .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await.2.expect("session cookie")
}

async fn get_page(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(app, request).await;
    (status, body)
}

fn csrf_of(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

async fn post_form(
    app: &Router,
    uri: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let (status, body, _) = send(app, request).await;
    (status, body)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cleanup_form_renders_and_saves(pool: PgPool) {
    let alice = web_user(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // Renders with the (unsaved) defaults.
    let (status, body) = get_page(&app, "/settings/statuses-cleanup", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Automated post deletion"), "{body}");
    let csrf = csrf_of(&body);

    // Saving stores the policy.
    let (status, _) = post_form(
        &app,
        "/web/settings/statuses-cleanup",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("enabled", "true"),
            ("min_status_age", "604800"),
            ("keep_pinned", "true"),
            ("keep_direct", "false"),
            ("keep_self_fav", "false"),
            ("keep_self_bookmark", "false"),
            ("keep_media", "true"),
            ("keep_polls", "false"),
            ("min_favs", "5"),
            ("min_reblogs", ""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let policy = statuses_cleanup::get(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert!(policy.enabled);
    assert_eq!(policy.min_status_age, 604_800);
    assert!(policy.keep_pinned && policy.keep_media);
    assert!(!policy.keep_direct && !policy.keep_self_fav && !policy.keep_self_bookmark);
    assert_eq!(policy.min_favs, Some(5));
    assert_eq!(policy.min_reblogs, None);

    // The saved values render back.
    let (_, body) = get_page(&app, "/settings/statuses-cleanup", &cookie).await;
    assert!(body.contains(r#"value="5""#), "{body}");

    // A min_status_age outside the fixed ladder is rejected.
    let (status, _) = post_form(
        &app,
        "/web/settings/statuses-cleanup",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("enabled", "true"),
            ("min_status_age", "60"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A sub-1 threshold is rejected.
    let (status, _) = post_form(
        &app,
        "/web/settings/statuses-cleanup",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("enabled", "true"),
            ("min_status_age", "604800"),
            ("min_favs", "0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cleanup_form_requires_a_session(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let app = common::test_app(pool.clone());
    let request = Request::builder()
        .uri("/settings/statuses-cleanup")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "redirects to /login");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delivery_backlog_defers_the_sweep(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let kept = old_status(&pool, alice.id, "public", 2 * TWO_WEEKS_SECS).await;
    statuses_cleanup::upsert(&pool, alice.id, default_policy())
        .await
        .unwrap();
    // Pile up more due deliveries than the load threshold allows.
    for i in 0..501 {
        job::enqueue(
            &pool,
            alice.id,
            &format!("https://busy.example/inbox/{i}"),
            &serde_json::json!({"type": "Create"}),
        )
        .await
        .unwrap();
    }

    let state = test_state_with(pool.clone(), Arc::default());
    let (deleted, _) = sweep::run_once(&state, 0).await;
    assert_eq!(deleted, 0);
    assert!(status::find_by_id(&pool, kept).await.unwrap().is_some());
}
