//! Instance actor (`/actor`) and authorized-fetch (secure mode) tests.

mod common;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_secure,
    test_app_secure_unsigned_profile, test_app_with,
};
use http_body_util::BodyExt;
use plamenu_db::{PgPool, account};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

const ACCEPT_AP: &str = "application/activity+json";

async fn get_with_accept(app: Router, path: &str, accept: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// Signs a GET of `path` as `signer` and sends it through the router.
async fn get_signed(app: Router, path: &str, signer: &RequestSigner) -> (StatusCode, Value) {
    let signed_headers = signer.sign_get("plamenu.test", path, ACCEPT_AP, SystemTime::now());
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "plamenu.test")
        .header("date", signed_headers.date)
        .header(header::ACCEPT, ACCEPT_AP)
        .header("signature", signed_headers.signature)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_actor_document_is_served_and_key_is_stable(pool: PgPool) {
    let (status, doc) = get_with_accept(test_app(pool.clone()), "/actor", ACCEPT_AP).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["id"], "https://plamenu.test/actor");
    assert_eq!(doc["type"], "Application");
    assert_eq!(doc["preferredUsername"], "plamenu.test");
    assert_eq!(doc["inbox"], "https://plamenu.test/actor/inbox");
    assert_eq!(doc["outbox"], "https://plamenu.test/actor/outbox");
    assert_eq!(
        doc["publicKey"]["id"],
        "https://plamenu.test/actor#main-key"
    );
    assert_eq!(doc["publicKey"]["owner"], "https://plamenu.test/actor");
    let pem = doc["publicKey"]["publicKeyPem"].as_str().unwrap();
    assert!(pem.contains("BEGIN PUBLIC KEY"));

    // FEP-521a: the Ed25519 Multikey rides `assertionMethod`.
    assert_eq!(
        doc["assertionMethod"][0]["id"],
        "https://plamenu.test/actor#ed25519-key"
    );
    assert_eq!(doc["assertionMethod"][0]["type"], "Multikey");
    assert_eq!(
        doc["assertionMethod"][0]["controller"],
        "https://plamenu.test/actor"
    );
    let multikey = doc["assertionMethod"][0]["publicKeyMultibase"]
        .as_str()
        .unwrap();
    assert!(multikey.starts_with("z6Mk"), "{multikey}");

    // The keys are generated once and persist across requests.
    let (_, again) = get_with_accept(test_app(pool.clone()), "/actor", ACCEPT_AP).await;
    assert_eq!(again["publicKey"]["publicKeyPem"].as_str().unwrap(), pem);
    assert_eq!(
        again["assertionMethod"][0]["publicKeyMultibase"]
            .as_str()
            .unwrap(),
        multikey
    );

    // Mastodon serves its instance actor regardless of the Accept header.
    let (status, _) = get_with_accept(test_app(pool), "/actor", "text/html").await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_actor_outbox_is_empty(pool: PgPool) {
    let (status, doc) = get_with_accept(test_app(pool), "/actor/outbox", ACCEPT_AP).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["type"], "OrderedCollection");
    assert_eq!(doc["id"], "https://plamenu.test/actor/outbox");
    assert_eq!(doc["totalItems"], 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_resolves_the_instance_actor_in_all_forms(pool: PgPool) {
    for resource in [
        "acct:plamenu.test@plamenu.test",
        "plamenu.test@plamenu.test",
        "plamenu.test",
        "https://plamenu.test",
        "https://plamenu.test/",
        "https://plamenu.test/actor",
    ] {
        // `:`, `@` and `/` are all legal raw inside a query value.
        let path = format!("/.well-known/webfinger?resource={resource}");
        let (status, jrd) = get_with_accept(test_app(pool.clone()), &path, "*/*").await;
        assert_eq!(status, StatusCode::OK, "{resource}");
        assert_eq!(jrd["subject"], "acct:plamenu.test@plamenu.test");
        assert_eq!(jrd["links"][0]["href"], "https://plamenu.test/actor");
    }

    // Regular user lookups are untouched.
    create_local_account(&pool, "alice", "Alice").await;
    let (status, jrd) = get_with_accept(
        test_app(pool),
        "/.well-known/webfinger?resource=acct:alice@plamenu.test",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(jrd["subject"], "acct:alice@plamenu.test");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn actor_inbox_processes_activities_like_the_shared_inbox(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let follow = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#follows/1", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice",
    });
    let bytes = serde_json::to_vec(&follow).unwrap();
    let signed = bob
        .signer()
        .sign_post("plamenu.test", "/actor/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/actor/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header("content-type", ACCEPT_AP)
        .body(Body::from(bytes))
        .unwrap();
    let response = test_app_with(pool.clone(), stub)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let stored_bob = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        plamenu_db::follow::find(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_rejects_unsigned_object_gets(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let app = || test_app_secure(pool.clone(), Arc::default());

    // The profile-bearing routes (statuses, outbox, collections and the social
    // graph) stay 401 without a signature. The bare actor document does not —
    // see `authorized_fetch_serves_key_only_actor_to_unsigned_callers`.
    for path in [
        "/users/alice/outbox",
        "/users/alice/followers",
        "/users/alice/following",
        "/users/alice/collections/featured",
        "/users/alice/statuses/1",
    ] {
        let (status, body) = get_with_accept(app(), path, ACCEPT_AP).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert!(body["error"].is_string(), "{path}");
    }

    // Discovery and the instance actor stay public — that is how two
    // secure-mode servers bootstrap each other's keys.
    for path in [
        "/actor",
        "/.well-known/webfinger?resource=acct:alice@plamenu.test",
        "/.well-known/nodeinfo",
        "/nodeinfo/2.0",
    ] {
        let (status, _) = get_with_accept(app(), path, ACCEPT_AP).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
}

/// B1: under secure mode an *unsigned* fetch of the actor document is served a
/// key-only "blanked" actor (id + type + key material, no profile) rather than
/// a 401, so peers that dereference us unsigned (e.g. default-config Lemmy) can
/// still read our public key to verify our deliveries.
#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_serves_key_only_actor_to_unsigned_callers(pool: PgPool) {
    // `create_local_account` gives alice a display name ("Alice") and a bio
    // ("test account"); both must be withheld from the unsigned key-only view.
    create_local_account(&pool, "alice", "Alice").await;

    let (status, doc) = get_with_accept(
        test_app_secure(pool.clone(), Arc::default()),
        "/users/alice",
        ACCEPT_AP,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Identity + key material a verifier needs are present …
    assert_eq!(doc["id"], "https://plamenu.test/users/alice");
    assert_eq!(doc["type"], "Person");
    assert_eq!(doc["preferredUsername"], "alice");
    assert_eq!(
        doc["publicKey"]["id"],
        "https://plamenu.test/users/alice#main-key"
    );
    assert!(
        doc["publicKey"]["publicKeyPem"]
            .as_str()
            .is_some_and(|pem| pem.contains("BEGIN PUBLIC KEY")),
        "key-only actor must still carry the public key"
    );
    assert_eq!(doc["inbox"], "https://plamenu.test/users/alice/inbox");
    // … but the profile is blanked: no display name, bio, or discoverability.
    assert!(doc["name"].is_null(), "display name must be withheld");
    assert!(doc["summary"].is_null(), "bio must be withheld");
    assert_eq!(doc["discoverable"], false);
    assert_eq!(doc["indexable"], false);
    assert!(doc["suspended"].is_null(), "live actor is not suspended");
}

/// With `authorized_fetch_unsigned = "profile"`, an *unsigned* actor fetch
/// under secure mode returns the profile document (display name, bio) — the
/// Lemmy-interop posture — while the object routes stay gated.
#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_unsigned_profile_serves_profile_to_unsigned_callers(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;

    let (status, doc) = get_with_accept(
        test_app_secure_unsigned_profile(pool.clone(), Arc::default()),
        "/users/alice",
        ACCEPT_AP,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["id"], "https://plamenu.test/users/alice");
    assert_eq!(
        doc["name"], "Alice",
        "public profile keeps the display name"
    );
    assert!(
        doc["publicKey"]["publicKeyPem"].is_string(),
        "and still carries the key"
    );

    // The firehose and social graph remain signature-gated even in this mode.
    let (followers, _) = get_with_accept(
        test_app_secure_unsigned_profile(pool, Arc::default()),
        "/users/alice/followers",
        ACCEPT_AP,
    )
    .await;
    assert_eq!(followers, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_accepts_signed_gets_and_caches_the_signer(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_secure(pool.clone(), stub.clone());

    // First signed GET: bob's key is unknown, so his actor is fetched.
    let (status, doc) = get_signed(app(), "/users/alice", &bob.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["id"], "https://plamenu.test/users/alice");
    assert_eq!(stub.fetches(), vec![bob.actor.id.clone()]);

    // Second signed GET: verified against the now-cached key, no re-fetch.
    let (status, _) = get_signed(app(), "/users/alice/followers", &bob.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stub.fetches().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_resolves_gotosocial_main_key_without_poisoning_fallback(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let mut sleyka = RemoteUser::new("social.example", "sleyka");
    sleyka.actor.public_key.id = format!("{}/main-key", sleyka.actor.id);
    let signer = sleyka.signer();
    let stub = StubFederation::with_actors([]);
    let key_id = sleyka.actor.public_key.id.clone();
    let actor_id = sleyka.actor.id.clone();
    let key = sleyka.actor.public_key.public_key_pem.clone();

    stub.objects.lock().unwrap().insert(
        key_id.clone(),
        json!({
            "@context": ["https://w3id.org/security/v1", "https://www.w3.org/ns/activitystreams"],
            "id": actor_id.clone(),
            "type": "Person",
            "preferredUsername": "sleyka",
            "publicKey": {
                "id": key_id.clone(),
                "owner": actor_id.clone(),
                "publicKeyPem": key.clone(),
            },
        }),
    );
    stub.objects.lock().unwrap().insert(
        sleyka.actor.id.clone(),
        json!({
            "id": sleyka.actor.id.clone(),
            "type": "Person",
            "preferredUsername": "sleyka",
            "inbox": sleyka.actor.inbox.clone(),
            "publicKey": {
                "id": sleyka.actor.public_key.id.clone(),
                "owner": sleyka.actor.public_key.owner.clone(),
                "publicKeyPem": sleyka.actor.public_key.public_key_pem.clone(),
            },
        }),
    );

    let (status, doc) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &signer,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["id"], "https://plamenu.test/users/alice");
    assert_eq!(
        stub.fetches(),
        vec![key_id, actor_id],
        "the key stub is followed directly instead of first failing actor parsing"
    );

    let (status, _) = get_signed(
        test_app_secure(pool, stub.clone()),
        "/users/alice/followers",
        &signer,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stub.fetches().len(), 2, "cached key avoids a second fetch");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_fetch_rejects_bad_signatures(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // A signature by a key that does not match the published one.
    let impostor_keys = plamenu_ap::keys::generate_keypair().unwrap();
    let impostor =
        RequestSigner::from_pkcs8_pem(&impostor_keys.private_pem, bob.actor.public_key.id.clone())
            .unwrap();
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &impostor,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A keyId whose actor cannot be dereferenced.
    let ghost_keys = plamenu_ap::keys::generate_keypair().unwrap();
    let ghost = RequestSigner::from_pkcs8_pem(
        &ghost_keys.private_pem,
        "https://gone.example/users/ghost#main-key".to_owned(),
    )
    .unwrap();
    let (status, _) = get_signed(test_app_secure(pool, stub), "/users/alice", &ghost).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
