//! Inbox integration tests: real HTTP signatures over the full router, with
//! the network stubbed out.

mod common;

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_immutable_local_account, create_local_account,
    test_app_with, test_state_with,
};
use plamenu::build_router;
use plamenu::{delivery, remote};
use plamenu_ap::actor::RemotePublicKeys;
use plamenu_db::account::{self, RemoteAccountData};
use plamenu_db::{PgPool, featured_tag, follow, job, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

const ALICE_URI: &str = "https://plamenu.test/users/alice";

fn follow_activity(bob: &RemoteUser) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/1", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": ALICE_URI,
    })
}

/// Signs `body` as a POST to `path` and sends it through the router.
async fn post_signed_at(
    app: Router,
    path: &str,
    body: &Value,
    signer: &RequestSigner,
    now: SystemTime,
) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, path, &bytes, now);
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    post_signed_at(app, path, body, signer, SystemTime::now()).await
}

async fn post_signed_with_sync_header(
    app: Router,
    path: &str,
    body: &Value,
    signer: &RequestSigner,
    synchronization: &str,
) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, path, &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("collection-synchronization", synchronization)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

async fn post_unsigned(app: Router, path: &str, body: &Value) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/activity+json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

fn followers_digest<'a>(uris: impl Iterator<Item = &'a str>) -> String {
    let mut digest = [0u8; 32];
    for uri in uris {
        let hashed = Sha256::digest(uri.as_bytes());
        for (left, right) in digest.iter_mut().zip(hashed) {
            *left ^= right;
        }
    }
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    hex
}

fn harmless_like(bob: &RemoteUser) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activities/like-for-sync", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice/statuses/404",
    })
}

async fn follower_ids(pool: &PgPool) -> Option<(i64, i64)> {
    let bob = account::find_by_uri(pool, "https://remote.example/users/bob")
        .await
        .unwrap()?;
    let alice = account::find_local_by_username(pool, "alice")
        .await
        .unwrap()?;
    Some((bob.id, alice.id))
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_follow_creates_follower_and_sends_accept(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let follow = follow_activity(&bob);

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &follow,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The remote account was dereferenced once and stored.
    assert_eq!(stub.fetches(), [bob.actor.id.as_str()]);
    let (bob_id, alice_id) = follower_ids(&pool).await.unwrap();
    assert!(follow::exists(&pool, bob_id, alice_id).await.unwrap());
    assert_eq!(follow::count_followers(&pool, alice_id).await.unwrap(), 1);

    // The Accept was queued; the delivery worker sends it signed as alice.
    let state = test_state_with(pool.clone(), stub.clone());
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let deliveries = stub.deliveries();
    assert_eq!(deliveries.len(), 1);
    let accept = &deliveries[0];
    assert_eq!(accept.inbox_url, bob.actor.inbox);
    assert_eq!(accept.key_id, format!("{ALICE_URI}#main-key"));
    assert!(format!("{accept:?}").contains("[REDACTED]"));
    assert_eq!(accept.activity["type"], "Accept");
    assert_eq!(accept.activity["actor"], ALICE_URI);
    assert_eq!(accept.activity["object"], follow);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn shared_inbox_cannot_target_a_pending_local_actor(pool: PgPool) {
    let pending = create_immutable_local_account(&pool, "pending", "Pending").await;
    let password_hash = plamenu::auth::hash_password("correct horse battery").unwrap();
    user::create(
        &pool,
        pending.id,
        Some("pending@example.com"),
        &password_hash,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(pending.id)
        .execute(&pool)
        .await
        .unwrap();

    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/pending", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": pending.uri,
    });
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &activity,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let bob_account = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, bob_account.id, pending.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
    assert!(stub.deliveries().is_empty());

    // A peer that cached the actor before this boundary existed may still
    // address a Note to it. The public Note may be ingested, but the pending
    // identity must not become a mention recipient or receive a notification.
    let note_uri = format!("{}/statuses/pending-target", bob.actor.id);
    let target_uri = pending.uri.as_deref().unwrap();
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hello cached actor</p>",
            "to": [target_uri, "https://www.w3.org/ns/activitystreams#Public"],
            "tag": [{"type": "Mention", "href": target_uri, "name": "@pending"}],
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/inbox",
            &create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let mentioned = plamenu_db::mention::for_statuses(&pool, &[stored.id], false)
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert!(mentioned.is_empty());
    let notifications = plamenu_db::notification::list(
        &pool,
        pending.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(notifications.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn immutable_actor_inbox_and_follow_target_resolve_by_numeric_id(pool: PgPool) {
    let alice = create_immutable_local_account(&pool, "immutable", "Immutable").await;
    let actor_uri = alice.uri.clone().unwrap();
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/immutable", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": actor_uri,
    });
    let path = format!("/ap/accounts/{}/inbox", alice.id);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &path,
            &follow,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED
    );
    let bob = account::find_by_uri(&pool, "https://remote.example/users/bob")
        .await
        .unwrap()
        .unwrap();
    assert!(follow::exists(&pool, bob.id, alice.id).await.unwrap());

    let state = test_state_with(pool.clone(), stub.clone());
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let accept = stub.deliveries();
    assert_eq!(accept[0].activity["actor"], alice.uri.unwrap());
    assert!(accept[0].key_id.starts_with(&format!("{actor_uri}#")));
}

/// pub-relay signs with `keyId=<actor id>` while its actor document declares
/// `<actor id>#main-key` (seen live from relay.dresden.network). The bare
/// keyId must still resolve to the actor and, once the actor is cached, must
/// not force a refetch on every delivery.
#[sqlx::test(migrations = "../db/migrations")]
async fn bare_actor_keyid_is_accepted_and_cached(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let signer =
        RequestSigner::from_pkcs8_pem(&bob.keys.private_pem, bob.actor.id.clone()).unwrap();

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &follow_activity(&bob),
        &signer,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(stub.fetches(), [bob.actor.id.as_str()]);

    // A later delivery finds the stored account by the keyId's actor URI
    // (the stored key id carries the fragment) instead of refetching.
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &harmless_like(&bob),
        &signer,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(stub.fetches(), [bob.actor.id.as_str()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rsa_multikey_only_actor_authenticates_a_real_inbox_request(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let mut bob = RemoteUser::new("remote.example", "bob");
    let key_id = format!("{}#rsa-multikey", bob.actor.id);
    let public = plamenu_ap::multikey::encode_rsa_public(&bob.keys.public_pem).unwrap();
    bob.actor.assertion_method = vec![json!({
        "id": key_id.clone(),
        "type": "Multikey",
        "controller": bob.actor.id,
        "publicKeyMultibase": public,
    })];
    bob.actor.public_key = RemotePublicKeys::default();
    let signer = RequestSigner::from_pkcs8_pem(&bob.keys.private_pem, key_id).unwrap();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub),
            "/users/alice/inbox",
            &follow_activity(&bob),
            &signer,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let bob = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(follow::exists(&pool, bob.id, alice.id).await.unwrap());
}

/// An account row claiming another actor's `publicKey.id` — a hostile actor
/// document, or (as happened live) hand-seeded QA data — must not shadow the
/// real owner in the signer cache: with the impostor cached first, a signed
/// delivery from the true owner still authenticates as them and dispatches
/// instead of being silently dropped as an unproven forward.
#[sqlx::test(migrations = "../db/migrations")]
async fn foreign_key_id_claim_does_not_shadow_the_owner(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mut mallory = RemoteUser::new("evil.example", "mallory").actor.clone();
    // Key material is public, so the claim really verifies: id and PEM both.
    mallory.public_key.id = format!("{}#main-key", bob.actor.id);
    mallory.public_key.public_key_pem = bob.actor.public_key.public_key_pem.clone();
    plamenu::remote::store_remote_actor(&pool, &mallory)
        .await
        .unwrap();
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &follow_activity(&bob),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // The cross-origin claim was ignored: bob was dereferenced and the
    // follow attributed to him, not swallowed via the forwarded-copy path.
    assert_eq!(stub.fetches(), [bob.actor.id.as_str()]);
    let (follower_id, alice_id) = follower_ids(&pool).await.unwrap();
    assert!(follow::exists(&pool, follower_id, alice_id).await.unwrap());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_synchronization_reconciles_local_followers_after_digest_match(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let eve = create_local_account(&pool, "eve", "Eve").await;
    let mallory = create_local_account(&pool, "mallory", "Mallory").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();

    follow::create(
        &pool,
        alice.id,
        bob_account.id,
        Some("https://plamenu.test/users/alice#follows/1"),
    )
    .await
    .unwrap();
    follow::create_outgoing(
        &pool,
        carol.id,
        bob_account.id,
        "https://plamenu.test/users/carol#follows/2",
    )
    .await
    .unwrap();
    follow::create(
        &pool,
        mallory.id,
        bob_account.id,
        Some("https://plamenu.test/users/mallory#follows/3"),
    )
    .await
    .unwrap();

    let collection_url = "https://remote.example/followers-for-plamenu";
    let expected_items = [
        "https://plamenu.test/users/carol",
        "https://plamenu.test/users/eve",
        "https://plamenu.test/users/mallory",
    ];
    let expected_digest = followers_digest(expected_items.into_iter());
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        collection_url.to_owned(),
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": collection_url,
            "type": "Collection",
            "items": expected_items,
        }),
    );
    let header = format!(
        "collectionId=\"{}\", digest=\"{}\", url=\"{}\"",
        bob.actor.followers_url().unwrap(),
        expected_digest,
        collection_url
    );

    let status = post_signed_with_sync_header(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &harmless_like(&bob),
        &bob.signer(),
        &header,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    assert!(
        follow::find(&pool, alice.id, bob_account.id)
            .await
            .unwrap()
            .is_none(),
        "stale local follower was removed"
    );
    assert!(
        !follow::find(&pool, carol.id, bob_account.id)
            .await
            .unwrap()
            .unwrap()
            .pending,
        "missed Accept(Follow) was repaired"
    );
    assert!(
        follow::find(&pool, mallory.id, bob_account.id)
            .await
            .unwrap()
            .is_some(),
        "expected accepted follower remains"
    );
    assert!(
        follow::find(&pool, eve.id, bob_account.id)
            .await
            .unwrap()
            .is_none(),
        "unexpected local account is not inserted as a follower"
    );

    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 2);
    let sent = stub.deliveries();
    let actors: Vec<&str> = sent
        .iter()
        .map(|delivery| delivery.activity["actor"].as_str().unwrap())
        .collect();
    assert!(actors.contains(&"https://plamenu.test/users/alice"));
    assert!(actors.contains(&"https://plamenu.test/users/eve"));
    assert!(sent.iter().all(|delivery| {
        delivery.inbox_url == bob.actor.inbox && delivery.activity["type"] == "Undo"
    }));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_synchronization_does_not_remove_when_digest_mismatches(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    follow::create(
        &pool,
        alice.id,
        bob_account.id,
        Some("https://plamenu.test/users/alice#follows/1"),
    )
    .await
    .unwrap();

    let collection_url = "https://remote.example/empty-followers-for-plamenu";
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        collection_url.to_owned(),
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": collection_url,
            "type": "Collection",
            "items": [],
        }),
    );
    let header = format!(
        "collectionId=\"{}\", digest=\"{}\", url=\"{}\"",
        bob.actor.followers_url().unwrap(),
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        collection_url
    );

    let status = post_signed_with_sync_header(
        test_app_with(pool.clone(), stub),
        "/inbox",
        &harmless_like(&bob),
        &bob.signer(),
        &header,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        follow::find(&pool, alice.id, bob_account.id)
            .await
            .unwrap()
            .is_some(),
        "destructive removal waits for a matching fetched digest"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unsigned_follow_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let status = post_unsigned(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &follow_activity(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(follower_ids(&pool).await.is_none(), "no account stored");
    assert!(stub.deliveries().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_signed_with_wrong_key_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mallory = RemoteUser::new("remote.example", "mallory");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // Mallory signs with her own key but claims bob's keyId.
    let forged_signer =
        RequestSigner::from_pkcs8_pem(&mallory.keys.private_pem, bob.actor.public_key.id.clone())
            .unwrap();
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &follow_activity(&bob),
        &forged_signer,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(follow::count_followers(&pool, alice.id).await.unwrap(), 0);
    assert!(stub.deliveries().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn impersonating_another_actor_is_rejected(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);

    // Signed (validly!) by bob, but the activity claims carol as the actor.
    // Accepted like any delivery, but treated as an unverifiable forwarded
    // copy — a Follow cannot be confirmed at its origin, so no follow (by
    // either identity) may appear.
    let mut forged = follow_activity(&bob);
    forged["actor"] = json!(carol.actor.id);
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &forged,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for remote in [&bob, &carol] {
        if let Some(row) = account::find_by_uri(&pool, &remote.actor.id).await.unwrap() {
            assert!(
                follow::find(&pool, row.id, alice.id)
                    .await
                    .unwrap()
                    .is_none(),
                "the forged Follow must not create a relationship"
            );
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stale_signature_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let thirteen_hours_ago = SystemTime::now() - Duration::from_hours(13);
    let status = post_signed_at(
        test_app_with(pool, stub),
        "/inbox",
        &follow_activity(&bob),
        &bob.signer(),
        thirteen_hours_ago,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn tampered_body_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // Sign one body, send another.
    let signed_body = serde_json::to_vec(&follow_activity(&bob)).unwrap();
    let signed_headers =
        bob.signer()
            .sign_post(TEST_DOMAIN, "/inbox", &signed_body, SystemTime::now());
    let mut tampered = follow_activity(&bob);
    tampered["object"] = json!("https://plamenu.test/users/other");
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(serde_json::to_vec(&tampered).unwrap()))
        .unwrap();
    let response = test_app_with(pool, stub).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn key_rotation_triggers_refetch_and_succeeds(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");

    // The cache holds bob with an outdated key…
    let old_key = plamenu_ap::keys::generate_keypair().unwrap();
    account::upsert_remote(
        &pool,
        RemoteAccountData {
            username: "bob",
            domain: "remote.example",
            uri: &bob.actor.id,
            display_name: "",
            note: "",
            inbox_url: &bob.actor.inbox,
            shared_inbox_url: "",
            public_key_pem: &old_key.public_pem,
            public_key_id: &bob.actor.public_key.id,
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
            discoverable: false,
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
    .unwrap();

    // …while bob signs with his current key, served by the stub.
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &follow_activity(&bob),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(stub.fetches().len(), 1, "exactly one refetch");

    let (bob_id, alice_id) = follower_ids(&pool).await.unwrap();
    assert!(follow::exists(&pool, bob_id, alice_id).await.unwrap());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn undo_removes_follower(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let follow = follow_activity(&bob);

    let app = || test_app_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(app(), "/inbox", &follow, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let (bob_id, alice_id) = follower_ids(&pool).await.unwrap();
    assert!(follow::exists(&pool, bob_id, alice_id).await.unwrap());
    let notifications = |pool: PgPool| async move {
        plamenu_db::notification::list(
            &pool,
            alice_id,
            None,
            None,
            None,
            plamenu_db::notification::NotificationFilter::default(),
            10,
        )
        .await
        .unwrap()
    };
    assert_eq!(notifications(pool.clone()).await.len(), 1);

    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/1/undo", bob.actor.id),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": follow,
    });
    assert_eq!(
        post_signed(app(), "/inbox", &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(!follow::exists(&pool, bob_id, alice_id).await.unwrap());
    // The withdrawn follow takes its notification with it, like Mastodon.
    assert!(notifications(pool.clone()).await.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_note_stores_status_idempotently(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hello fediverse</p>",
            "published": "2026-06-01T12:00:00Z",
            "url": "https://remote.example/@bob/1",
        },
    });

    let app = || test_app_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "<p>hello fediverse</p>");
    assert_eq!(
        stored.url.as_deref(),
        Some("https://remote.example/@bob/1"),
        "the note's published web url is captured, distinct from its AP id"
    );

    // Redelivery must not duplicate.
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let again = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, stored.id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn iri_create_matches_inline_delivery_and_uses_inbox_recipient(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let inline_uri = format!("{}/statuses/inline-create", bob.actor.id);
    let referenced_uri = format!("{}/statuses/referenced-create", bob.actor.id);
    let note = |uri: &str| {
        json!({
            "id": uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>equivalent delivery</p>",
            "published": "2026-06-01T12:00:00Z",
            "to": [ALICE_URI, "https://www.w3.org/ns/activitystreams#Public"],
            "tag": [{"type": "Mention", "href": ALICE_URI, "name": "@alice"}],
        })
    };
    let inline_object = note(&inline_uri);
    let referenced_object = note(&referenced_uri);
    stub.objects
        .lock()
        .unwrap()
        .insert(referenced_uri.clone(), referenced_object);

    let inline_create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{inline_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": inline_object,
    });
    let referenced_create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{referenced_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": referenced_uri,
    });
    let app = || test_app_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(app(), "/users/alice/inbox", &inline_create, &bob.signer()).await,
        StatusCode::ACCEPTED,
    );
    assert_eq!(
        post_signed(
            app(),
            "/users/alice/inbox",
            &referenced_create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );

    let inline = status::find_by_uri(&pool, &inline_uri)
        .await
        .unwrap()
        .expect("the inline object is ingested");
    let referenced = status::find_by_uri(&pool, &referenced_uri)
        .await
        .unwrap()
        .expect("the IRI-valued object is dereferenced and ingested");
    assert_eq!(referenced.account_id, inline.account_id);
    assert_eq!(referenced.content, inline.content);
    assert_eq!(referenced.visibility, inline.visibility);
    assert!(
        plamenu_db::mention::exists(&pool, inline.id, alice.id)
            .await
            .unwrap()
    );
    assert!(
        plamenu_db::mention::exists(&pool, referenced.id, alice.id)
            .await
            .unwrap()
    );
    assert_eq!(
        stub.account_fetches(),
        [(referenced_uri.clone(), alice.id)],
        "the per-user inbox recipient must sign the object dereference"
    );

    let notifications = || async {
        plamenu_db::notification::list(
            &pool,
            alice.id,
            None,
            None,
            None,
            plamenu_db::notification::NotificationFilter {
                include_filtered: true,
                ..Default::default()
            },
            10,
        )
        .await
        .unwrap()
    };
    assert_eq!(notifications().await.len(), 2);

    // Dereferencing a replay is harmless: it neither duplicates storage nor
    // repeats any one-shot delivery effects.
    assert_eq!(
        post_signed(
            app(),
            "/users/alice/inbox",
            &referenced_create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    assert_eq!(
        status::find_by_uri(&pool, &referenced_uri)
            .await
            .unwrap()
            .unwrap()
            .id,
        referenced.id
    );
    assert_eq!(notifications().await.len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn iri_create_shared_inbox_uses_addressed_recipient(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let note_uri = format!("{}/statuses/shared-reference", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        note_uri.clone(),
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>shared inbox</p>",
            "to": [ALICE_URI],
        }),
    );
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": [ALICE_URI],
        "object": note_uri,
    });

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/inbox",
            &create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        stub.account_fetches(),
        [(note_uri.clone(), alice.id)],
        "top-level Create addressing must select the local fetch signer"
    );

    // The audit's exact public/shared-inbox shape has no recipient principal;
    // it falls back to the ordinary instance signer and still ingests.
    let public_uri = format!("{}/statuses/public-reference", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        public_uri.clone(),
        json!({
            "id": public_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>public reference</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let public_create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{public_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": public_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/inbox",
            &public_create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    assert!(
        status::find_by_uri(&pool, &public_uri)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        stub.account_fetches(),
        [(note_uri, alice.id)],
        "a public Create with no local audience must use the instance signer"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn iri_create_rejects_cross_origin_and_foreign_attribution(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let cross_origin_uri = "https://unrelated.example/statuses/forged";
    stub.objects.lock().unwrap().insert(
        cross_origin_uri.to_owned(),
        json!({
            "id": cross_origin_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>wrong origin</p>",
        }),
    );
    let cross_origin = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activities/cross-origin", bob.actor.id),
        "type": "Create",
        "actor": bob.actor.id,
        "object": cross_origin_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/users/alice/inbox",
            &cross_origin,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    assert!(
        !stub.fetches().iter().any(|uri| uri == cross_origin_uri),
        "a foreign origin must be rejected before dereferencing"
    );

    let foreign_author_uri = format!("{}/statuses/foreign-author", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        foreign_author_uri.clone(),
        json!({
            "id": foreign_author_uri,
            "type": "Note",
            "attributedTo": "https://remote.example/users/carol",
            "content": "<p>not Bob's words</p>",
        }),
    );
    let foreign_author = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{foreign_author_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": foreign_author_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/users/alice/inbox",
            &foreign_author,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    assert!(
        status::find_by_uri(&pool, &foreign_author_uri)
            .await
            .unwrap()
            .is_none(),
        "the fetched object must be attributed to the authenticated sender"
    );
}

/// A mention-less reply can still address its parent author in `to`. It
/// threads and grants silent audience access, but—like Mastodon—does not turn
/// that delivery address into a mention notification. An edit stays silent,
/// as does a `cc`-only bystander.
#[sqlx::test(migrations = "../db/migrations")]
async fn to_addressed_reply_without_mention_tag_stays_silent(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let (parent, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "root post",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let parent_uri = format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", parent.id);

    let note_uri = format!("{}/statuses/77", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>a quiet reply</p>",
            "inReplyTo": parent_uri,
            "to": [ALICE_URI, "https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{}/followers", bob.actor.id)],
        },
    });
    let app = || test_app_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("the reply is ingested");
    assert_eq!(stored.in_reply_to_id, Some(parent.id));
    assert!(
        plamenu_db::mention::exists(&pool, stored.id, alice.id)
            .await
            .unwrap(),
        "the parent author remains a silent audience recipient",
    );
    assert!(
        plamenu_db::mention::for_statuses(&pool, &[stored.id], true)
            .await
            .unwrap()
            .is_empty(),
        "the address must not become an active mention",
    );
    let notifications = |pool: PgPool| async move {
        plamenu_db::notification::list(
            &pool,
            alice.id,
            None,
            None,
            None,
            plamenu_db::notification::NotificationFilter::default(),
            10,
        )
        .await
        .unwrap()
    };
    let items = notifications(pool.clone()).await;
    assert!(
        items.is_empty(),
        "a tagless addressed reply must not notify: {items:?}",
    );

    // An edit re-runs audience processing but must not re-notify.
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity/update"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>a quiet reply, revised</p>",
            "inReplyTo": parent_uri,
            "to": [ALICE_URI, "https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{}/followers", bob.actor.id)],
            "updated": "2026-07-19T12:00:00Z",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        notifications(pool.clone()).await.len(),
        0,
        "the edit must not manufacture a notification"
    );

    // A local user only copied in `cc` is stored as a silent mention row but
    // never notified.
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let cc_note_uri = format!("{}/statuses/78", bob.actor.id);
    let cc_only = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{cc_note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": cc_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>ambient</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("https://{TEST_DOMAIN}/users/carol")],
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &cc_only, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let carol_items = plamenu_db::notification::list(
        &pool,
        carol.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(
        carol_items.is_empty(),
        "cc alone must not notify: {carol_items:?}"
    );
}

/// Incise-style personalized fan-out puts the receiving follower in a public
/// top-level Note's `to` without a Mention tag. That is delivery/access
/// addressing, not a reply or a mention: retain the silent audience row but do
/// not manufacture a "mentioned you" notification.
#[sqlx::test(migrations = "../db/migrations")]
async fn to_addressed_non_reply_is_silent_and_does_not_notify(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let note_uri = format!("{}/statuses/personalized-fanout", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>ordinary follower delivery</p>",
            "to": [ALICE_URI, "https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{}/followers", bob.actor.id)],
        },
    });

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub),
            "/users/alice/inbox",
            &create,
            &bob.signer(),
        )
        .await,
        StatusCode::ACCEPTED,
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("the addressed Note is ingested");
    assert!(
        plamenu_db::mention::exists(&pool, stored.id, alice.id)
            .await
            .unwrap(),
        "the inbox owner remains a silent audience recipient",
    );
    assert!(
        plamenu_db::mention::for_statuses(&pool, &[stored.id], true)
            .await
            .unwrap()
            .is_empty(),
        "a silent audience recipient must not appear as an active mention",
    );
    let notifications = plamenu_db::notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(
        notifications.is_empty(),
        "an ordinary to-addressed post must not notify: {notifications:?}",
    );
}

/// Inbound Note `to`/`cc`/`audience` resolution is set-based. A
/// Note addressed to many distinct non-existent local users issues the same
/// number of queries as one addressed to a single user — proof the former
/// per-username `find_local_by_username` loop is gone and a hostile signed
/// delivery packing thousands of local URLs cannot drive a serial query /
/// mention / notification fan-out.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_note_audience_resolution_query_count_is_flat(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    // One shared state so the sender actor and any lazy caches persist across
    // deliveries (each `build_router` clone shares the same `Arc`'d state).
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || build_router(state.clone());

    // Delivers a public Note addressed to `n` distinct, non-existent local users
    // in `to`, so no mention or notification is written and the counter sees
    // only the resolution work.
    let deliver = async |n: usize, seq: usize| {
        let mut to: Vec<String> = (0..n)
            .map(|i| format!("https://{TEST_DOMAIN}/users/ghost{i}"))
            .collect();
        to.push("https://www.w3.org/ns/activitystreams#Public".to_owned());
        let note_uri = format!("{}/statuses/aud{seq}", bob.actor.id);
        let create = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{note_uri}/activity"),
            "type": "Create",
            "actor": bob.actor.id,
            "object": {
                "id": note_uri,
                "type": "Note",
                "attributedTo": bob.actor.id,
                "content": "<p>addressed to nobody we host</p>",
                "to": to,
            },
        });
        assert_eq!(
            post_signed(app(), "/inbox", &create, &bob.signer()).await,
            StatusCode::ACCEPTED,
        );
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(common::QueryCounter(counter.clone())),
    );

    // Warm up: store the sending actor and warm any lazy caches so the two
    // measured deliveries differ only in how many users they address.
    deliver(1, 0).await;

    counter.store(0, Ordering::Relaxed);
    deliver(1, 1).await;
    let one = counter.swap(0, Ordering::Relaxed);

    deliver(50, 2).await;
    let fifty = counter.load(Ordering::Relaxed);

    println!("inbound note audience: 1 recipient -> {one} queries, 50 -> {fifty}");
    assert_eq!(
        one, fifty,
        "50 distinct addressed local URLs must not add queries over addressing one",
    );
}

/// An edit rebuilds the whole mention set atomically. The status's
/// mention rows exactly reflect the new addressing/tags — recipients dropped by
/// the edit lose their rows, added ones gain them, and a real (tag) mention
/// outranks a silent audience one for the same account — with no stray rows left
/// from the previous version.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_note_edit_rebuilds_mention_set(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let user_url = |name: &str| format!("https://{TEST_DOMAIN}/users/{name}");

    // First version: dave is tag-mentioned (active), alice is addressed in `to`
    // (silent access + notify), carol is copied in `cc` (silent).
    let note_uri = format!("{}/statuses/rebuild", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hi @dave</p>",
            "to": [user_url("alice"), "https://www.w3.org/ns/activitystreams#Public"],
            "cc": [user_url("carol")],
            "tag": [{"type": "Mention", "href": user_url("dave"), "name": "@dave"}],
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED,
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();

    let mentioned = |active_only: bool| {
        let pool = pool.clone();
        async move {
            let mut ids: Vec<i64> =
                plamenu_db::mention::for_statuses(&pool, &[stored.id], active_only)
                    .await
                    .unwrap()
                    .remove(&stored.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.id)
                    .collect();
            ids.sort_unstable();
            ids
        }
    };
    let mut all = vec![alice.id, carol.id, dave.id];
    all.sort_unstable();
    assert_eq!(mentioned(false).await, all, "all three are mention rows");
    assert_eq!(mentioned(true).await, vec![dave.id], "only dave is active");

    // Edit: address only carol in `to` now (was `cc`), tag-mention alice, and
    // drop dave entirely.
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity/update"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>actually @alice</p>",
            "to": [user_url("carol"), "https://www.w3.org/ns/activitystreams#Public"],
            "tag": [{"type": "Mention", "href": user_url("alice"), "name": "@alice"}],
            "updated": "2026-07-20T12:00:00Z",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED,
    );
    // dave is gone; alice (now tag-mentioned) is active; carol is a silent `to`.
    let mut all_after = vec![alice.id, carol.id];
    all_after.sort_unstable();
    assert_eq!(
        mentioned(false).await,
        all_after,
        "the set is rebuilt: dave dropped, no stray rows",
    );
    assert_eq!(
        mentioned(true).await,
        vec![alice.id],
        "alice's tag mention outranks any silent audience row",
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_note_stores_content_warning_sensitive_and_language(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // The spoiler arrives as `summary`; markup is reduced to plain text.
    let note_uri = format!("{}/statuses/cw", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "summary": "spiders <b>everywhere</b>",
            "sensitive": true,
            "content": "<p>eek</p>",
            "contentMap": {"de": "<p>eek</p>"},
            "published": "2026-06-01T12:00:00Z",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.spoiler_text, "spiders everywhere");
    assert!(stored.sensitive);
    assert_eq!(stored.language.as_deref(), Some("de"));

    // `summaryMap` is the fallback when `summary` is absent; Mastodon also uses
    // it as the language source when there is no content/name language map.
    let mapped_uri = format!("{}/statuses/cw-map", bob.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{mapped_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": mapped_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "summaryMap": {"en": "mapped warning"},
            "content": "<p>hi</p>",
            "published": "2026-06-01T12:00:00Z",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &mapped_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.spoiler_text, "mapped warning");
    assert!(!stored.sensitive);
    assert_eq!(stored.language.as_deref(), Some("en"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_note_attributed_to_someone_else_is_ignored(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let note_uri = "https://remote.example/users/carol/statuses/9";
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": "https://remote.example/users/carol",
            "content": "<p>forged</p>",
        },
    });

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub),
            "/inbox",
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, note_uri)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delete_removes_own_status_only(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>to be deleted</p>",
        },
    });
    let delete = json!({
        "type": "Delete",
        "actor": bob.actor.id,
        "object": { "id": note_uri, "type": "Tombstone" },
    });

    let app = || test_app_with(pool.clone(), stub.clone());
    post_signed(app(), "/inbox", &create, &bob.signer()).await;
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        post_signed(app(), "/inbox", &delete, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delete_of_unknown_actor_is_accepted_without_fetching(pool: PgPool) {
    let stub: Arc<StubFederation> = Arc::default();
    let delete = json!({
        "type": "Delete",
        "actor": "https://gone.example/users/ghost",
        "object": "https://gone.example/users/ghost",
    });
    // Not even signed: the actor is gone, verification would be impossible.
    let status = post_unsigned(test_app_with(pool, stub.clone()), "/inbox", &delete).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        stub.fetches().is_empty(),
        "must not dereference deleted actors"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delete_of_known_actor_removes_account_and_follows(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let app = || test_app_with(pool.clone(), stub.clone());
    post_signed(app(), "/inbox", &follow_activity(&bob), &bob.signer()).await;
    let (_, alice_id) = follower_ids(&pool).await.unwrap();
    assert_eq!(follow::count_followers(&pool, alice_id).await.unwrap(), 1);

    let delete = json!({
        "type": "Delete",
        "actor": bob.actor.id,
        "object": bob.actor.id,
    });
    assert_eq!(
        post_signed(app(), "/inbox", &delete, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        account::find_by_uri(&pool, &bob.actor.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(follow::count_followers(&pool, alice_id).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_of_unknown_local_user_is_404(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status = post_signed(
        test_app_with(pool, stub),
        "/inbox",
        &follow_activity(&bob), // alice does not exist in this test
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn user_inbox_of_unknown_user_is_404(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status = post_signed(
        test_app_with(pool, stub),
        "/users/ghost/inbox",
        &follow_activity(&bob),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unhandled_activity_types_are_accepted_quietly(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let like = json!({
        "type": "Like",
        "actor": bob.actor.id,
        "object": format!("{ALICE_URI}/statuses/1"),
    });
    let status = post_signed(test_app_with(pool, stub), "/inbox", &like, &bob.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn non_json_body_is_400(pool: PgPool) {
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .body(Body::from("not json"))
        .unwrap();
    let response = test_app_with(pool, Arc::default())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---- Thread backfill -------------------------------------------------------

/// A public `Note` by `user`, optionally replying to `parent_uri`.
fn note_object(user: &RemoteUser, slug: &str, in_reply_to: Option<&str>) -> Value {
    let mut object = json!({
        "id": format!("{}/statuses/{slug}", user.actor.id),
        "type": "Note",
        "attributedTo": user.actor.id,
        "content": format!("<p>{slug}</p>"),
        "published": "2026-06-01T12:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(parent_uri) = in_reply_to {
        object["inReplyTo"] = json!(parent_uri);
    }
    object
}

fn create_activity(user: &RemoteUser, object: &Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activity", object["id"].as_str().unwrap()),
        "type": "Create",
        "actor": user.actor.id,
        "object": object,
    })
}

fn uri_of(object: &Value) -> String {
    object["id"].as_str().unwrap().to_owned()
}

async fn stored_status(pool: &PgPool, uri: &str) -> Option<status::Status> {
    status::find_by_uri(pool, uri).await.unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reply_to_unknown_status_backfills_ancestor_chain(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, alice.id, stored_bob.id, None)
        .await
        .unwrap();

    let root = note_object(&bob, "root", None);
    let mid = note_object(&bob, "mid", Some(&uri_of(&root)));
    let leaf = note_object(&bob, "leaf", Some(&uri_of(&mid)));
    stub.objects
        .lock()
        .unwrap()
        .extend([(uri_of(&root), root.clone()), (uri_of(&mid), mid.clone())]);

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &create_activity(&bob, &leaf),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The whole chain was fetched, stored and threaded.
    let stored_root = stored_status(&pool, &uri_of(&root)).await.unwrap();
    let stored_mid = stored_status(&pool, &uri_of(&mid)).await.unwrap();
    let stored_leaf = stored_status(&pool, &uri_of(&leaf)).await.unwrap();
    assert_eq!(stored_leaf.in_reply_to_id, Some(stored_mid.id));
    assert_eq!(stored_mid.in_reply_to_id, Some(stored_root.id));
    assert_eq!(stored_root.in_reply_to_id, None);
    assert_eq!(stored_root.content, "<p>root</p>");
    // Walked leaf-upward: the direct parent before the root.
    let object_fetches: Vec<String> = stub
        .fetches()
        .into_iter()
        .filter(|uri| uri.contains("/statuses/"))
        .collect();
    assert_eq!(object_fetches, [uri_of(&mid), uri_of(&root)]);
    assert_eq!(
        stub.account_fetches(),
        [(uri_of(&mid), alice.id), (uri_of(&root), alice.id),],
        "ancestor backfill must use the local follower's signing identity"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reply_to_known_status_fetches_nothing(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let parent = note_object(&bob, "parent", None);
    let reply = note_object(&bob, "reply", Some(&uri_of(&parent)));
    let signer = bob.signer();
    assert_eq!(
        post_signed(app(), "/inbox", &create_activity(&bob, &parent), &signer).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), "/inbox", &create_activity(&bob, &reply), &signer).await,
        StatusCode::ACCEPTED
    );

    let stored_parent = stored_status(&pool, &uri_of(&parent)).await.unwrap();
    let stored_reply = stored_status(&pool, &uri_of(&reply)).await.unwrap();
    assert_eq!(stored_reply.in_reply_to_id, Some(stored_parent.id));
    // Only bob's actor was dereferenced (signature verification), no objects.
    assert_eq!(stub.fetches(), [bob.actor.id.as_str()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn backfill_stops_at_the_ancestor_cap(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // a7 ← a6 ← … ← a1 ← leaf: two more ancestors than the cap of five.
    let mut ancestors = vec![note_object(&bob, "a7", None)];
    for n in (1..=6).rev() {
        let parent_uri = uri_of(ancestors.last().unwrap());
        ancestors.push(note_object(&bob, &format!("a{n}"), Some(&parent_uri)));
    }
    let leaf = note_object(&bob, "leaf", Some(&uri_of(ancestors.last().unwrap())));
    stub.objects
        .lock()
        .unwrap()
        .extend(ancestors.iter().map(|o| (uri_of(o), o.clone())));

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &create_activity(&bob, &leaf),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // a1–a5 came in threaded; a5 is the (flat) truncation point, a6/a7 never.
    let stored_leaf = stored_status(&pool, &uri_of(&leaf)).await.unwrap();
    let a1 = stored_status(&pool, "https://remote.example/users/bob/statuses/a1")
        .await
        .unwrap();
    assert_eq!(stored_leaf.in_reply_to_id, Some(a1.id));
    let a5 = stored_status(&pool, "https://remote.example/users/bob/statuses/a5")
        .await
        .unwrap();
    assert_eq!(a5.in_reply_to_id, None);
    for missing in ["a6", "a7"] {
        let uri = format!("https://remote.example/users/bob/statuses/{missing}");
        assert!(stored_status(&pool, &uri).await.is_none());
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reply_to_unfetchable_status_is_stored_flat(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // The parent is not in the stub's object map: the fetch 404s.
    let leaf = note_object(
        &bob,
        "leaf",
        Some("https://remote.example/users/bob/statuses/gone"),
    );
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &create_activity(&bob, &leaf),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let stored_leaf = stored_status(&pool, &uri_of(&leaf)).await.unwrap();
    assert_eq!(stored_leaf.in_reply_to_id, None);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn backfill_refuses_cross_host_ancestors(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // A note hosted on remote.example claiming an author hosted elsewhere.
    let forged_uri = "https://remote.example/users/bob/statuses/forged";
    let forged = json!({
        "id": forged_uri,
        "type": "Note",
        "attributedTo": "https://other.example/users/mallory",
        "content": "<p>forged</p>",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(forged_uri.to_owned(), forged);

    let leaf = note_object(&bob, "leaf", Some(forged_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &create_activity(&bob, &leaf),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    assert!(stored_status(&pool, forged_uri).await.is_none());
    let stored_leaf = stored_status(&pool, &uri_of(&leaf)).await.unwrap();
    assert_eq!(stored_leaf.in_reply_to_id, None);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn backfill_survives_reply_cycles(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // A malicious pair replying to each other: a ← b ← a ← …
    let cycle_a_uri = "https://remote.example/users/bob/statuses/cycle-a";
    let cycle_b_uri = "https://remote.example/users/bob/statuses/cycle-b";
    let cycle_a = note_object(&bob, "cycle-a", Some(cycle_b_uri));
    let cycle_b = note_object(&bob, "cycle-b", Some(cycle_a_uri));
    stub.objects.lock().unwrap().extend([
        (cycle_a_uri.to_owned(), cycle_a),
        (cycle_b_uri.to_owned(), cycle_b),
    ]);

    let leaf = note_object(&bob, "leaf", Some(cycle_a_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &create_activity(&bob, &leaf),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The cycle is broken at the revisited note: b stored flat, a under b.
    let stored_a = stored_status(&pool, cycle_a_uri).await.unwrap();
    let stored_b = stored_status(&pool, cycle_b_uri).await.unwrap();
    let stored_leaf = stored_status(&pool, &uri_of(&leaf)).await.unwrap();
    assert_eq!(stored_leaf.in_reply_to_id, Some(stored_a.id));
    assert_eq!(stored_a.in_reply_to_id, Some(stored_b.id));
    assert_eq!(stored_b.in_reply_to_id, None);
}

/// The full relay lifecycle: subscribe (instance-actor Follow to the relay
/// inbox), the relay's Accept flipping state, public fan-out including the
/// relay, and a relayed Announce ingesting the object instead of boosting.
#[sqlx::test(migrations = "../db/migrations")]
async fn relay_subscription_accept_and_relayed_announce(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let relay_user = RemoteUser::new("relay.example", "actor");
    let federation = StubFederation::with_users(&[&relay_user]);
    let state = test_state_with(pool.clone(), federation.clone());

    // Subscribe: pending + Follow(as:Public) signed by the instance actor.
    let relay =
        plamenu_db::relay::create(&pool, &relay_user.actor.inbox, Some(&relay_user.actor.id))
            .await
            .unwrap()
            .unwrap();
    assert!(plamenu::relays::enable(&state, relay.id).await.unwrap());
    delivery::run_due(&state).await;
    let sent = federation.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, relay_user.actor.inbox);
    assert_eq!(sent[0].activity["type"], "Follow");
    assert_eq!(
        sent[0].activity["object"],
        "https://www.w3.org/ns/activitystreams#Public"
    );
    assert_eq!(
        sent[0].activity["actor"],
        format!("https://{TEST_DOMAIN}/actor")
    );
    assert_eq!(
        sent[0].key_id,
        format!("https://{TEST_DOMAIN}/actor#main-key")
    );
    let pending = plamenu_db::relay::find(&pool, relay.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, "pending");
    let follow_activity_id = pending.follow_activity_id.clone().unwrap();

    // The relay accepts, echoing our Follow back as the object.
    let accept = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#accepts/1", relay_user.actor.id),
        "type": "Accept",
        "actor": relay_user.actor.id,
        "object": {
            "id": follow_activity_id,
            "type": "Follow",
            "actor": format!("https://{TEST_DOMAIN}/actor"),
            "object": "https://www.w3.org/ns/activitystreams#Public",
        },
    });
    let app = test_app_with(pool.clone(), federation.clone());
    let status = post_signed(app.clone(), "/inbox", &accept, &relay_user.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        plamenu_db::relay::find(&pool, relay.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "accepted"
    );

    // A public local status now also fans out to the relay inbox.
    let _ = alice;
    let before = federation.deliveries().len();
    plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "hello relay",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    delivery::run_due(&state).await;
    let after = federation.deliveries();
    assert!(
        after[before..]
            .iter()
            .any(|d| d.inbox_url == relay_user.actor.inbox && d.activity["type"] == "Create"),
        "expected the Create to reach the relay inbox"
    );

    // A relayed Announce ingests the object without creating a boost.
    let carol = RemoteUser::new("remote.example", "carol");
    federation
        .actors
        .lock()
        .unwrap()
        .insert(carol.actor.id.clone(), carol.actor.clone());
    let note_uri = format!("{}/statuses/9001", carol.actor.id);
    federation.objects.lock().unwrap().insert(
        note_uri.clone(),
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": carol.actor.id,
            "content": "<p>via relay</p>",
            "published": "2026-07-01T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let announce = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://relay.example/activities/1",
        "type": "Announce",
        "actor": relay_user.actor.id,
        "object": note_uri,
    });
    let status = post_signed(app.clone(), "/inbox", &announce, &relay_user.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let ingested = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("relayed note ingested");
    assert!(ingested.reblog_of_id.is_none());
    // No boost row was created for the relay actor's announce.
    assert!(
        status::find_by_uri(&pool, "https://relay.example/activities/1")
            .await
            .unwrap()
            .is_none()
    );

    // Unsubscribing sends Undo(Follow) and returns the relay to idle.
    let before = federation.deliveries().len();
    assert!(plamenu::relays::disable(&state, relay.id).await.unwrap());
    delivery::run_due(&state).await;
    let sent = federation.deliveries();
    let undo = &sent[before..][0];
    assert_eq!(undo.activity["type"], "Undo");
    assert_eq!(undo.activity["object"]["id"], follow_activity_id);
    assert_eq!(
        plamenu_db::relay::find(&pool, relay.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "idle"
    );
}

/// A relay's Reject marks the subscription rejected.
#[sqlx::test(migrations = "../db/migrations")]
async fn relay_reject_marks_subscription_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let relay_user = RemoteUser::new("relay.example", "actor");
    let federation = StubFederation::with_users(&[&relay_user]);
    let state = test_state_with(pool.clone(), federation.clone());

    // No actor URI: the pre-migration-0028 fallback, which matches a relay by
    // its *own* inbox. Kept exercised so upgraded rows keep working.
    let relay = plamenu_db::relay::create(&pool, &relay_user.actor.inbox, None)
        .await
        .unwrap()
        .unwrap();
    plamenu::relays::enable(&state, relay.id).await.unwrap();
    let follow_activity_id = plamenu_db::relay::find(&pool, relay.id)
        .await
        .unwrap()
        .unwrap()
        .follow_activity_id
        .unwrap();

    // Reject with the bare Follow id as the object.
    let reject = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#rejects/1", relay_user.actor.id),
        "type": "Reject",
        "actor": relay_user.actor.id,
        "object": follow_activity_id,
    });
    let app = test_app_with(pool.clone(), federation.clone());
    let status = post_signed(app, "/inbox", &reject, &relay_user.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        plamenu_db::relay::find(&pool, relay.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "rejected"
    );
}

/// Signs `body` as an RFC 9421 POST (`rsa-v1_5-sha256`) and sends it.
async fn post_signed_rfc9421(
    app: Router,
    path: &str,
    body: &Value,
    signer: &RequestSigner,
) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let target = format!("https://{TEST_DOMAIN}{path}");
    let produced = signer.sign_post_rfc9421(&target, &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", TEST_DOMAIN)
        .header("date", produced.date)
        .header("content-digest", produced.content_digest)
        .header("signature-input", produced.signature_input)
        .header("signature", produced.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// RFC 9421: an inbox delivery signed with `Signature-Input` /
/// `Signature` instead of draft-cavage verifies and processes end-to-end.
#[sqlx::test(migrations = "../db/migrations")]
async fn rfc9421_signed_follow_is_processed(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let status = post_signed_rfc9421(
        test_app_with(pool.clone(), stub.clone()),
        "/users/alice/inbox",
        &follow_activity(&bob),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let bob_row = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("bob was dereferenced and stored");
    assert!(
        follow::exists(&pool, bob_row.id, alice.id).await.unwrap(),
        "the RFC 9421-signed follow landed"
    );
}

/// An RFC 9421 signature by the wrong key must be rejected like any other
/// bad signature.
#[sqlx::test(migrations = "../db/migrations")]
async fn rfc9421_signed_with_wrong_key_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mallory = RemoteUser::new("remote.example", "mallory");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // Mallory's key material under bob's keyId.
    let forged =
        RequestSigner::from_pkcs8_pem(&mallory.keys.private_pem, bob.actor.public_key.id.clone())
            .unwrap();
    let status = post_signed_rfc9421(
        test_app_with(pool.clone(), stub),
        "/users/alice/inbox",
        &follow_activity(&bob),
        &forged,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An RFC 9421 delivery signed with the actor's FEP-521a Ed25519 key
/// (`alg="ed25519"`, keyid `#ed25519-key`) — the shape Mitra produces with
/// `rfc9421_enabled` — verifies against the stored Multikey.
#[sqlx::test(migrations = "../db/migrations")]
async fn rfc9421_ed25519_signed_follow_is_processed(pool: PgPool) {
    use base64::Engine;

    let alice = create_local_account(&pool, "alice", "Alice").await;
    let mut bob = RemoteUser::new("remote.example", "bob").with_ed25519();
    // No classic `publicKey`: this is a real FEP-521a-only authentication
    // path, not merely selection of a Multikey next to a legacy fallback.
    bob.actor.public_key = RemotePublicKeys::default();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let follow = follow_activity(&bob);

    let bytes = serde_json::to_vec(&follow).unwrap();
    let path = "/users/alice/inbox";
    let target = format!("https://{TEST_DOMAIN}{path}");
    let digest = plamenu_federation::rfc9421::content_digest(&bytes);
    let created = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key_id = bob.ed25519_key_id();
    let params = format!(
        "(\"@method\" \"@target-uri\" \"content-digest\");created={created};keyid=\"{key_id}\";alg=\"ed25519\"",
    );
    let base = format!(
        "\"@method\": POST\n\"@target-uri\": {target}\n\"content-digest\": {digest}\n\"@signature-params\": {params}",
    );
    let secret = plamenu_ap::multikey::decode_ed25519_private(
        &bob.ed25519.as_ref().unwrap().private_multibase,
    )
    .unwrap();
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    let signature = base64::engine::general_purpose::STANDARD
        .encode(ed25519_dalek::Signer::sign(&signing, base.as_bytes()).to_bytes());

    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", TEST_DOMAIN)
        .header("content-digest", digest)
        .header("signature-input", format!("sig1={params}"))
        .header("signature", format!("sig1=:{signature}:"))
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    let status = test_app_with(pool.clone(), stub)
        .oneshot(request)
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);

    let bob_row = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("bob was dereferenced via his ed25519 keyId");
    assert!(
        follow::exists(&pool, bob_row.id, alice.id).await.unwrap(),
        "the Ed25519 RFC 9421 follow landed"
    );
}

/// Ed25519 (FEP-521a) key rotation: the stored Multikey is stale, the
/// delivery is signed with the actor's new key — one actor refetch updates
/// the stored key and the signature verifies, mirroring the RSA
/// `key_rotation_triggers_refetch_and_succeeds` path.
#[sqlx::test(migrations = "../db/migrations")]
async fn ed25519_key_rotation_triggers_refetch_and_succeeds(pool: PgPool) {
    use base64::Engine;

    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob").with_ed25519();
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // The cache holds bob's retired key under its own immutable URI…
    let stale = plamenu_ap::keys::generate_ed25519_keypair();
    let mut stale_actor = bob.actor.clone();
    stale_actor.assertion_method = vec![serde_json::json!({
        "id": format!("{}#ed25519-old", stale_actor.id),
        "type": "Multikey",
        "controller": stale_actor.id,
        "publicKeyMultibase": stale.public_multibase,
    })];
    let stored = plamenu::remote::store_remote_actor(&pool, &stale_actor)
        .await
        .unwrap();

    // …while the delivery is signed with his current key.
    let follow = follow_activity(&bob);
    let bytes = serde_json::to_vec(&follow).unwrap();
    let path = "/users/alice/inbox";
    let target = format!("https://{TEST_DOMAIN}{path}");
    let digest = plamenu_federation::rfc9421::content_digest(&bytes);
    let created = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key_id = bob.ed25519_key_id();
    let params = format!(
        "(\"@method\" \"@target-uri\" \"content-digest\");created={created};keyid=\"{key_id}\";alg=\"ed25519\"",
    );
    let base = format!(
        "\"@method\": POST\n\"@target-uri\": {target}\n\"content-digest\": {digest}\n\"@signature-params\": {params}",
    );
    let secret = plamenu_ap::multikey::decode_ed25519_private(
        &bob.ed25519.as_ref().unwrap().private_multibase,
    )
    .unwrap();
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    let signature = base64::engine::general_purpose::STANDARD
        .encode(ed25519_dalek::Signer::sign(&signing, base.as_bytes()).to_bytes());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", TEST_DOMAIN)
        .header("content-digest", digest)
        .header("signature-input", format!("sig1={params}"))
        .header("signature", format!("sig1=:{signature}:"))
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    let status = test_app_with(pool.clone(), stub.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);

    // Exactly one refetch, and the stored key is the rotated one.
    assert_eq!(stub.fetches().len(), 1, "exactly one actor refetch");
    let keys = plamenu_db::actor_key::usable_for_account(&pool, stored.id)
        .await
        .unwrap();
    assert_eq!(
        keys.iter()
            .find(|key| key.key_uri == bob.ed25519_key_id())
            .unwrap()
            .public_key,
        bob.ed25519.as_ref().unwrap().public_multibase,
        "the refetch stored the new exact key URI"
    );
    assert!(
        plamenu_db::actor_key::by_uri(&pool, &format!("{}#ed25519-old", bob.actor.id))
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some(),
        "the absent overlap key was revoked rather than overwritten"
    );
    assert!(
        follow::exists(&pool, stored.id, alice.id).await.unwrap(),
        "the follow landed after the rotation refetch"
    );
}

// A remote actor can send signed self-`Update`s without bound.
// Each accepted one would otherwise spawn two detached collection-refresh tasks
// (featuredTags + outbox), growing the pending-task population without limit.
// The refresh coordinator coalesces them: the first Update admits two refreshes,
// later ones within the freshness window admit none.
#[sqlx::test(migrations = "../db/migrations")]
async fn flooded_actor_updates_spawn_a_bounded_number_of_refreshes(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    // Share one AppState across every delivery so the in-memory coordinator
    // persists: each `build_router` clone shares the same `Arc`'d state, unlike
    // a fresh `test_app_with` per request.
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || build_router(state.clone());

    let outbox_uri = format!("{}/outbox", bob.actor.id);
    let featured_tags_uri = format!("{}/collections/tags", bob.actor.id);
    let actor_object = json!({
        "id": bob.actor.id,
        "type": "Person",
        "preferredUsername": "bob",
        "inbox": bob.actor.inbox,
        "outbox": outbox_uri,
        "featuredTags": featured_tags_uri,
        "publicKey": {
            "id": bob.actor.public_key.id,
            "owner": bob.actor.id,
            "publicKeyPem": bob.actor.public_key.public_key_pem,
        },
    });

    let before = plamenu::remote::refresh_spawn_count();
    for n in 0..10 {
        let update = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{}#updates/{n}", bob.actor.id),
            "type": "Update",
            "actor": bob.actor.id,
            "object": actor_object,
        });
        assert_eq!(
            post_signed(app(), "/inbox", &update, &bob.signer()).await,
            StatusCode::ACCEPTED,
            "update {n} is accepted"
        );
    }
    let spawned = plamenu::remote::refresh_spawn_count() - before;
    assert_eq!(
        spawned, 2,
        "10 identical actor Updates admit exactly the first pair of refreshes \
         (featuredTags + outbox), not 20"
    );
}

/// Polls `cond` (which may hit the database) roughly every 10 ms up to ~5 s.
/// The refresh tasks run detached, so the assertions below wait for them to
/// reach the gated fetch, and then to finish reconciling once released.
async fn wait_for<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not met within the timeout");
}

// The refresh bound must also hold when the origin is *slow*.
// Without a pre-spawn admission cap, a flood of signed self-`Update`s behind a
// hanging origin would pile up detached fetch tasks — each pinning an
// `AppState` clone and an attacker-supplied collection URI — waiting on the
// 32-permit network semaphore. This gates the origin so every outbound object
// fetch hangs, floods identical Updates, and proves the fan-out stays bounded:
// exactly one outbound fetch per collection and one DB reconciliation each, no
// matter how deep the flood, with only the first pair of tasks ever pending.
#[sqlx::test(migrations = "../db/migrations")]
async fn slow_origin_actor_update_flood_keeps_refresh_fan_out_bounded(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let outbox_uri = format!("{}/outbox", bob.actor.id);
    let featured_tags_uri = format!("{}/collections/tags", bob.actor.id);
    // The two collections the refresh tasks fetch from the (about-to-be-gated)
    // origin: the outbox carries a status total, featuredTags one hashtag.
    stub.objects.lock().unwrap().insert(
        outbox_uri.clone(),
        json!({ "id": outbox_uri, "type": "OrderedCollection", "totalItems": 42 }),
    );
    stub.objects.lock().unwrap().insert(
        featured_tags_uri.clone(),
        json!({
            "id": featured_tags_uri,
            "type": "OrderedCollection",
            "orderedItems": [{
                "type": "Hashtag",
                "name": "#rust",
                "href": format!("{}/tags/rust", bob.actor.id),
            }],
        }),
    );
    // Hold every outbound object fetch open — a "slow origin" that never answers
    // until released below.
    stub.gate_object_fetches();

    let state = test_state_with(pool.clone(), stub.clone());
    let app = || build_router(state.clone());

    let actor_object = json!({
        "id": bob.actor.id,
        "type": "Person",
        "preferredUsername": "bob",
        "inbox": bob.actor.inbox,
        "outbox": outbox_uri,
        "featuredTags": featured_tags_uri,
        "publicKey": {
            "id": bob.actor.public_key.id,
            "owner": bob.actor.id,
            "publicKeyPem": bob.actor.public_key.public_key_pem,
        },
    });

    let before = plamenu::remote::refresh_spawn_count();
    // Flood: 20 identical signed self-Updates. Signature verification uses
    // `fetch_actor` (not gated) and the handler stores the inline actor without
    // a fetch, so each Update is accepted immediately even while the origin
    // hangs; only the spawned refresh tasks touch the gated `fetch_object`.
    for n in 0..20 {
        let update = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{}#updates/{n}", bob.actor.id),
            "type": "Update",
            "actor": bob.actor.id,
            "object": actor_object,
        });
        assert_eq!(
            post_signed(app(), "/inbox", &update, &bob.signer()).await,
            StatusCode::ACCEPTED,
            "update {n} is accepted while the origin hangs",
        );
    }

    // Coalescing/freshness admit only the first Update's featuredTags+outbox
    // pair; the other 19 find them in-flight or too-recent and admit nothing.
    // Because the gate holds those two tasks open, that admitted count is also
    // the *pending* task population: two, no matter how deep the flood.
    assert_eq!(
        plamenu::remote::refresh_spawn_count() - before,
        2,
        "a 20-deep flood behind a hung origin leaves exactly two refresh tasks pending",
    );

    // Wait until both admitted tasks reach the origin and hang there, then prove
    // that is *all* the outbound work: one fetch per collection, not one per
    // flooded Update.
    let collection_hits =
        |stub: &StubFederation, uri: &str| stub.fetches().iter().filter(|u| *u == uri).count();
    wait_for(|| async {
        collection_hits(&stub, &outbox_uri) >= 1 && collection_hits(&stub, &featured_tags_uri) >= 1
    })
    .await;
    assert_eq!(
        collection_hits(&stub, &outbox_uri),
        1,
        "the outbox is fetched once behind the gate, not once per Update",
    );
    assert_eq!(
        collection_hits(&stub, &featured_tags_uri),
        1,
        "featuredTags is fetched once behind the gate, not once per Update",
    );

    // Release the slow origin and let both tasks finish reconciling.
    stub.open_object_fetches();
    let bob_id = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("bob was stored by the first Update")
        .id;
    wait_for(|| async { !featured_tag::list(&pool, bob_id).await.unwrap().is_empty() }).await;

    // DB reconciliation ran exactly once: one featured tag, and the outbox
    // fetch (which precedes every status-count write) was never repeated.
    let featured = featured_tag::list(&pool, bob_id).await.unwrap();
    assert_eq!(
        featured.len(),
        1,
        "featuredTags reconciled once, to one tag"
    );
    assert_eq!(
        collection_hits(&stub, &outbox_uri),
        1,
        "no further outbox fetch after the gate opened — reconciliation ran once",
    );
    assert_eq!(
        collection_hits(&stub, &featured_tags_uri),
        1,
        "no further featuredTags fetch after the gate opened",
    );
}
