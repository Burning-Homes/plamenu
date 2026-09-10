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
