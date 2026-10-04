//! Immutable local actor IDs and handle-only rename behavior.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    StubFederation, TEST_DOMAIN, create_immutable_local_account, deliver_all_due, test_app,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::{PgPool, account};
use serde_json::Value;
use tower::ServiceExt;

#[sqlx::test(migrations = "../db/migrations")]
async fn local_parent_resolution_reads_identity_and_status_together(pool: PgPool) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::layer::SubscriberExt;

    let legacy = common::create_local_account(&pool, "legacy", "Legacy").await;
    let numeric = create_immutable_local_account(&pool, "numeric", "Numeric").await;
    let state = test_state_with(pool.clone(), Arc::default());
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(common::QueryCounter(counter.clone())),
    );
    let mut counts = Vec::new();
    for actor in [&legacy, &numeric] {
        let post = plamenu_db::status::create_local(
            &pool,
            plamenu_db::status::NewLocalStatus::new(actor.id, "<p>parent</p>", "private", None),
        )
        .await
        .unwrap();
        let uri = plamenu::entities::status_uri_for_account(TEST_DOMAIN, &post, actor);
        counter.store(0, Ordering::Relaxed);
        let resolved = plamenu::ingest::resolve_status_ref(&state, &uri)
            .await
            .unwrap()
            .unwrap();
        counts.push(counter.load(Ordering::Relaxed));
        assert_eq!(resolved.id, post.id);
        assert_eq!(
            resolved.visibility, "private",
            "the caller applies the thread ceiling"
        );
        if actor.uri.is_none() {
            let folded = format!("https://{TEST_DOMAIN}/users/LEGACY/statuses/{}", post.id);
            assert_eq!(
                plamenu::ingest::resolve_status_ref(&state, &folded)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                post.id,
            );
        }
        let wrong = format!("https://{TEST_DOMAIN}/users/other/statuses/{}", post.id);
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &wrong)
                .await
                .unwrap()
                .is_none()
        );
        // Deletion retains a placeholder only while a live reply depends on it.
        plamenu_db::status::create_local(
            &pool,
            plamenu_db::status::NewLocalStatus::new(
                actor.id,
                "<p>reply</p>",
                "private",
                Some(post.id),
            ),
        )
        .await
        .unwrap();
        plamenu_db::status::stub_local(&pool, post.id, actor.id)
            .await
            .unwrap();
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &uri)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        counts,
        [2, 2],
        "remote URI probe plus one local identity/status snapshot"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_parent_resolution_preserves_canonical_uri_precedence_and_activation(pool: PgPool) {
    let alice = common::create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let old = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "<p>old</p>", "public", None),
    )
    .await
    .unwrap();
    // A stored legacy identity takes precedence over an upgrade fixture whose
    // current handle matches that URI. Set the persisted historical identity
    // directly: the handle-rename API intentionally rejects legacy actors.
    let old_actor_uri = format!("https://{TEST_DOMAIN}/users/original_alice");
    sqlx::query("UPDATE accounts SET uri = $1 WHERE id = $2")
        .bind(&old_actor_uri)
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let reused = common::create_local_account(&pool, "original_alice", "New Alice").await;
    let new = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(reused.id, "<p>new</p>", "public", None),
    )
    .await
    .unwrap();
    let canonical = format!("{old_actor_uri}/statuses/{}", old.id);
    assert_eq!(
        plamenu::ingest::resolve_status_ref(&state, &canonical)
            .await
            .unwrap()
            .unwrap()
            .id,
        old.id
    );
    for alias in [
        format!("{old_actor_uri}/statuses/{}", new.id),
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", old.id),
    ] {
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &alias)
                .await
                .unwrap()
                .is_none()
        );
    }
    let immutable = create_immutable_local_account(&pool, "numeric_owner", "Numeric").await;
    let numeric_post = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(immutable.id, "<p>numeric</p>", "public", None),
    )
    .await
    .unwrap();
    let numeric_uri =
        plamenu::entities::status_uri_for_account(TEST_DOMAIN, &numeric_post, &immutable);
    account::rename_local(&pool, immutable.id, "numeric_renamed")
        .await
        .unwrap();
    common::create_local_account(&pool, "numeric_owner", "Reused handle").await;
    assert_eq!(
        plamenu::ingest::resolve_status_ref(&state, &numeric_uri)
            .await
            .unwrap()
            .unwrap()
            .id,
        numeric_post.id,
    );
    for handle in ["numeric_owner", "numeric_renamed"] {
        let alias = format!(
            "https://{TEST_DOMAIN}/users/{handle}/statuses/{}",
            numeric_post.id
        );
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &alias)
                .await
                .unwrap()
                .is_none()
        );
    }
    plamenu_db::user::create(&pool, alice.id, None, "unused-test-password-hash")
        .await
        .unwrap();
    for state_sql in [
        "UPDATE users SET approved = false, confirmed_at = now() WHERE account_id = $1",
        "UPDATE users SET approved = true, confirmed_at = NULL WHERE account_id = $1",
    ] {
        sqlx::query(state_sql)
            .bind(alice.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &canonical)
                .await
                .unwrap()
                .is_none()
        );
        let shadowed = format!("{old_actor_uri}/statuses/{}", new.id);
        assert!(
            plamenu::ingest::resolve_status_ref(&state, &shadowed)
                .await
                .unwrap()
                .is_none(),
            "a pending canonical actor still blocks fallback to a reused handle"
        );
    }
}

async fn get(
    pool: &PgPool,
    path: &str,
    accept: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().uri(path);
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let response = test_app(pool.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rename_changes_discovery_and_human_urls_but_no_federated_id(pool: PgPool) {
    let created = create_immutable_local_account(&pool, "alice", "Alice").await;
    let actor_id = created.uri.clone().unwrap();
    assert_eq!(
        actor_id,
        format!("https://{TEST_DOMAIN}/ap/accounts/{}", created.id)
    );
    assert!(!actor_id.contains("alice"));
    let before = LocalUserUrls::for_account(TEST_DOMAIN, &created.username, created.uri.as_deref());
    // Queue an activity in the builder-era username layout before the rename.
    // Delivery must resolve the persisted actor/key at execution time.
    let queued =
        plamenu_ap::activity::like(TEST_DOMAIN, "alice", 77, "https://remote.example/objects/1");
    plamenu_db::job::enqueue(&pool, created.id, "https://remote.example/inbox", &queued)
        .await
        .unwrap();

    let actor_path = format!("/ap/accounts/{}", created.id);
    let (status, _, body) = get(&pool, &actor_path, Some("application/activity+json")).await;
    assert_eq!(status, StatusCode::OK);
    let actor: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(actor["id"], actor_id);
    assert_eq!(actor["inbox"], before.inbox);
    assert_eq!(actor["publicKey"]["id"], before.key_id);

    let (_, _, body) = get(
        &pool,
        "/.well-known/webfinger?resource=acct:alice@plamenu.test",
        None,
    )
    .await;
    let jrd: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        jrd["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| { link["rel"] == "self" && link["href"] == actor_id })
    );

    account::rename_local(&pool, created.id, "alice_new")
        .await
        .unwrap();
    let renamed = account::find_by_id(&pool, created.id)
        .await
        .unwrap()
        .unwrap();
    let after = LocalUserUrls::for_account(TEST_DOMAIN, &renamed.username, renamed.uri.as_deref());
    assert_eq!(before.id, after.id);
    assert_eq!(before.inbox, after.inbox);
    assert_eq!(before.outbox, after.outbox);
    assert_eq!(before.followers, after.followers);
    assert_eq!(before.following, after.following);
    assert_eq!(before.featured, after.featured);
    assert_eq!(before.key_id, after.key_id);
    assert_eq!(before.ed25519_key_id, after.ed25519_key_id);

    let (_, _, body) = get(
        &pool,
        "/.well-known/webfinger?resource=acct:alice_new@plamenu.test",
        None,
    )
    .await;
    let jrd: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        jrd["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| { link["rel"] == "self" && link["href"] == actor_id })
    );

    let (status, headers, _) = get(&pool, "/@alice/media?max_id=12", None).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        headers.get(header::LOCATION).unwrap(),
        "/@alice_new/media?max_id=12"
    );
    let (status, _, body) = get(&pool, &actor_path, Some("application/activity+json")).await;
    assert_eq!(status, StatusCode::OK);
    let actor_after: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(actor_after["id"], actor_id);
    assert_eq!(actor_after["preferredUsername"], "alice_new");
    assert_eq!(actor_after["inbox"], before.inbox);
    assert_eq!(actor_after["publicKey"]["id"], before.key_id);

    let federation = std::sync::Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), federation.clone());
    assert_eq!(deliver_all_due(&state).await, 1);
    let deliveries = federation.deliveries.lock().unwrap();
    let delivered = &deliveries[0].activity;
    assert_eq!(delivered["actor"], actor_id);
    assert!(delivered["id"].as_str().unwrap().starts_with(&actor_id));
    assert!(
        delivered["proof"]["verificationMethod"]
            .as_str()
            .unwrap()
            .starts_with(&actor_id),
        "proof and transport signing resolve the canonical post-rename key"
    );
}
