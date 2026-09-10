//! Server self-destruct (Mastodon's `tootctl self-destruct`): the 410 gate
//! over the whole surface with its sign-in/export allowlist, and the
//! broadcast worker that queues `Delete(Actor)` for every local account to
//! every known inbox.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, create_local_account, test_state_with};
use plamenu::{build_router, remote, self_destruct};
use plamenu_db::{PgPool, account, instance_settings, job};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[sqlx::test(migrations = "../db/migrations")]
async fn gate_serves_410_except_wind_down_allowlist(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let app = build_router(test_state_with(pool.clone(), std::sync::Arc::default()));

    // Normal operation: everything answers as usual.
    let (status, _) = get(&app, "/api/v1/instance").await;
    assert_eq!(status, StatusCode::OK);

    instance_settings::begin_self_destruct(&pool).await.unwrap();
    // A fresh router so the settings cache has no pre-destruct entry to serve.
    let app = build_router(test_state_with(pool.clone(), std::sync::Arc::default()));

    // API and OAuth surfaces: Mastodon's JSON 410.
    for uri in [
        "/api/v1/instance",
        "/api/v1/timelines/public",
        "/oauth/authorize",
    ] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::GONE, "expected 410 for {uri}");
        assert_eq!(body, r#"{"error":"Gone"}"#, "body for {uri}");
    }

    // The ActivityPub surface 410s too — the strongest "gone" signal for
    // remotes, and inbox POSTs are refused so peers stop delivering.
    for uri in [
        "/users/alice",
        "/.well-known/webfinger?resource=acct:alice@plamenu.test",
    ] {
        let (status, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::GONE, "expected 410 for {uri}");
    }
    let inbox_post = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header(header::CONTENT_TYPE, "application/activity+json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.clone().oneshot(inbox_post).await.unwrap();
    assert_eq!(response.status(), StatusCode::GONE);

    // Web pages get the human-readable wind-down notice.
    let (status, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::GONE);
    assert!(body.contains("permanently going offline"), "got: {body}");

    // The wind-down allowlist: sign-in, password reset, the export page, the
    // full-archive request/download routes the page promises, and the assets
    // they need stay reachable (finding #43). The archive routes 405/redirect
    // here without auth — the point is only that the gate itself does not 410
    // them, which `archive.rs` covers end to end with a real session.
    for uri in [
        "/login",
        "/auth/password/new",
        "/settings/export",
        "/web/settings/archive",
        "/settings/archive/1/download",
        "/assets/app.css",
        "/manifest.webmanifest",
        "/pwa/icon-192.png",
        "/sw.js",
        "/offline",
        "/favicon.ico",
        "/health",
        "/ready",
    ] {
        let (status, _) = get(&app, uri).await;
        assert_ne!(status, StatusCode::GONE, "expected {uri} to stay reachable");
    }

    // But the rest of the settings surface — imports and unrelated mutations —
    // stays gated: only the two archive routes are exempt, not the family.
    for uri in [
        "/settings/import/1",
        "/settings/profile",
        "/settings/account",
    ] {
        let (status, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::GONE, "expected {uri} to stay gated");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn worker_broadcasts_delete_actor_to_every_known_inbox(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    // An account that was already suspended (e.g. self-deleted) has had its
    // Delete federated at suspension time — the worker must skip it.
    let ghost = create_local_account(&pool, "ghost", "Ghost").await;
    account::suspend(&pool, ghost.id, "local").await.unwrap();

    // Three remote accounts on two servers: two share one shared inbox, the
    // third (no shared inbox) contributes its personal inbox. None of them
    // follow anyone — the broadcast audience is every known inbox.
    for user in [
        RemoteUser::new("masto.example", "carl"),
        RemoteUser::new("masto.example", "dana"),
    ] {
        remote::store_remote_actor(&pool, &user.actor)
            .await
            .unwrap();
    }
    let mut loner = RemoteUser::new("other.example", "erin");
    loner.actor.endpoints = None;
    remote::store_remote_actor(&pool, &loner.actor)
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), std::sync::Arc::default());

    let progress = self_destruct::progress(&state).await.unwrap();
    assert_eq!(progress.pending_accounts, 2);
    assert_eq!(progress.pending_deliveries, 0);

    // One pass covers both accounts: 2 accounts × 2 distinct inboxes.
    assert_eq!(self_destruct::run_once(&state).await.unwrap(), 2);
    let jobs = plamenu_db::job::claim_due(&pool, 100).await.unwrap();
    assert_eq!(jobs.len(), 4);
    for delivery_job in &jobs {
        assert_eq!(delivery_job.activity["type"], "Delete");
        let actor_uri = delivery_job.activity["actor"].as_str().unwrap();
        assert_eq!(delivery_job.activity["object"]["id"], actor_uri);
        assert_eq!(delivery_job.activity["object"]["type"], "Tombstone");
        assert!(
            actor_uri.ends_with("/users/alice") || actor_uri.ends_with("/users/bob"),
            "unexpected actor {actor_uri}"
        );
    }
    let mut inboxes: Vec<_> = jobs
        .iter()
        .filter(|j| {
            j.activity["actor"]
                .as_str()
                .unwrap()
                .ends_with("/users/alice")
        })
        .map(|j| j.inbox_url.clone())
        .collect();
    inboxes.sort();
    assert_eq!(
        inboxes,
        [
            "https://masto.example/inbox",
            "https://other.example/users/erin/inbox"
        ]
    );
    // Each delivery is signed by the deleted account itself.
    assert!(
        jobs.iter()
            .all(|j| { j.account_id == Some(alice.id) || j.account_id == Some(bob.id) })
    );

    // Broadcast accounts are suspended (tombstoned) as their notices queue.
    let alice = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    assert!(alice.suspended());

    // The pass is idempotent: nothing is left to broadcast.
    assert_eq!(self_destruct::run_once(&state).await.unwrap(), 0);
    let progress = self_destruct::progress(&state).await.unwrap();
    assert_eq!(progress.pending_accounts, 0);
    assert_eq!(progress.pending_deliveries, 4);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn worker_backs_off_while_the_queue_is_saturated(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    remote::store_remote_actor(&pool, &RemoteUser::new("masto.example", "carl").actor)
        .await
        .unwrap();
    // Saturate the queue to exactly MAX_ENQUEUED (10 000) in one batched insert.
    // Exactly-full must already back off — the boundary is `>=`, not `>`
    // (finding #42) — so the earlier off-by-one that admitted a pass at exactly
    // the cap is gone.
    let noop = serde_json::json!({"type": "Like"});
    let filler: Vec<String> = (0..10_000)
        .map(|i| format!("https://masto.example/inbox/{i}"))
        .collect();
    job::enqueue_many(&pool, alice.id, &filler, &noop, false)
        .await
        .unwrap();
    assert_eq!(job::pending_count(&pool).await.unwrap(), 10_000);

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    assert_eq!(self_destruct::run_once(&state).await.unwrap(), 0);
    let alice = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    assert!(!alice.suspended(), "backpressure must not suspend accounts");
}

/// Finding #42: one pass used to queue up to `ACCOUNTS_PER_PASS × inboxes` jobs
/// after a single top-of-pass capacity check, so it could blow far past the
/// `MAX_ENQUEUED` (10 000) backpressure cap. Each account's chunk is now reserved
/// before it starts, so a pass stops the moment the next account would overshoot.
#[sqlx::test(migrations = "../db/migrations")]
async fn worker_reserves_queue_capacity_per_account(pool: PgPool) {
    // Three local accounts await broadcast; two known remote inboxes → each
    // account's Delete fan-out is a 2-job chunk.
    let a = create_local_account(&pool, "a", "A").await;
    create_local_account(&pool, "b", "B").await;
    create_local_account(&pool, "c", "C").await;
    remote::store_remote_actor(&pool, &RemoteUser::new("masto.example", "carl").actor)
        .await
        .unwrap();
    let mut loner = RemoteUser::new("other.example", "erin");
    loner.actor.endpoints = None;
    remote::store_remote_actor(&pool, &loner.actor)
        .await
        .unwrap();

    // Pre-fill the queue to three below the cap (MAX_ENQUEUED − 3 = 9 997) with
    // unrelated jobs. Only one account's 2-job chunk fits before the queue would
    // exceed the cap, so the reservation must stop the pass after exactly one.
    let noop = serde_json::json!({"type": "Like"});
    let filler: Vec<String> = (0..9_997)
        .map(|i| format!("https://noop.example/inbox/{i}"))
        .collect();
    job::enqueue_many(&pool, a.id, &filler, &noop, false)
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    assert_eq!(
        self_destruct::run_once(&state).await.unwrap(),
        1,
        "only one account's chunk fits under the cap",
    );
    // The queue never exceeded MAX_ENQUEUED: 9 997 filler + one 2-job chunk.
    assert_eq!(job::pending_count(&pool).await.unwrap(), 9_999);
    // Two accounts still await their broadcast for the next pass.
    let progress = self_destruct::progress(&state).await.unwrap();
    assert_eq!(progress.pending_accounts, 2);
}
