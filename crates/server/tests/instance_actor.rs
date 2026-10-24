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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn instance_actor_outbox_is_empty(pool: PgPool) {
    let (status, doc) = get_with_accept(test_app(pool), "/actor/outbox", ACCEPT_AP).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["type"], "OrderedCollection");
    assert_eq!(doc["id"], "https://plamenu.test/actor/outbox");
    assert_eq!(doc["totalItems"], 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

fn bookwyrm_actor_document(remote: &RemoteUser) -> Value {
    json!({
        "id": remote.actor.id,
        "type": "Person",
        "preferredUsername": remote.actor.preferred_username,
        "inbox": remote.actor.inbox,
        "publicKey": {
            "id": remote.actor.public_key.id,
            "owner": remote.actor.public_key.owner,
            "publicKeyPem": remote.actor.public_key.public_key_pem,
        },
    })
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn authorized_fetch_resolves_bookwyrm_slash_key_and_caches_canonical_actor(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let mut remote = RemoteUser::new("books.example", "lena");
    remote.actor.public_key.id = format!("{}/#main-key", remote.actor.id);
    let stub = StubFederation::with_actors([]);
    let document = bookwyrm_actor_document(&remote);
    let alias = format!("{}/", remote.actor.id);
    stub.objects.lock().unwrap().extend([
        (alias.clone(), document.clone()),
        (remote.actor.id.clone(), document),
    ]);
    let app = || test_app_secure(pool.clone(), stub.clone());
    let (status, doc) = get_signed(app(), "/users/alice", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(stub.fetches(), vec![alias.clone(), remote.actor.id.clone()]);
    assert!(account::find_by_uri(&pool, &alias).await.unwrap().is_none());
    assert!(
        account::find_by_uri(&pool, &remote.actor.id)
            .await
            .unwrap()
            .is_some()
    );
    let (status, _) = get_signed(app(), "/users/alice/followers", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stub.fetches().len(), 2, "canonical actor key is cached");
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn authorized_fetch_slash_alias_requires_canonical_key_ownership_and_signature(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    for mode in [
        "unadvertised",
        "wrong_owner",
        "bad_signature",
        "missing_canonical",
    ] {
        let mut remote = RemoteUser::new("books.example", mode);
        remote.actor.public_key.id = format!("{}/#main-key", remote.actor.id);
        let signer = remote.signer();
        let mut document = bookwyrm_actor_document(&remote);
        match mode {
            "unadvertised" => {
                document["publicKey"]["id"] = json!(format!("{}#other", remote.actor.id));
            }
            "wrong_owner" => {
                document["publicKey"]["owner"] = json!("https://other.example/user/lena");
            }
            "bad_signature" => {
                document["publicKey"]["publicKeyPem"] =
                    json!(plamenu_ap::keys::generate_keypair().unwrap().public_pem);
            }
            _ => {}
        }
        let stub = StubFederation::with_actors([]);
        stub.objects
            .lock()
            .unwrap()
            .insert(format!("{}/", remote.actor.id), document.clone());
        if mode != "missing_canonical" {
            stub.objects
                .lock()
                .unwrap()
                .insert(remote.actor.id.clone(), document);
        }
        let (status, doc) =
            get_signed(test_app_secure(pool.clone(), stub), "/users/alice", &signer).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{mode}: {doc}");
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

// PROPOSAL ONLY: append to crates/server/tests/instance_actor.rs after review.
// No tests/builds have been run. Reuses imports/helpers already in that file.

fn internal_fetch_fixture() -> (RemoteUser, Value) {
    let mut remote = RemoteUser::new("fetch.example", "internal.fetch");
    remote.actor.id = "https://fetch.example/internal/fetch".into();
    remote.actor.public_key.id = format!("{}#main-key", remote.actor.id);
    remote.actor.public_key.owner.clone_from(&remote.actor.id);
    let document = json!({
        "id": remote.actor.id,
        "type": "Application",
        "preferredUsername": "internal.fetch",
        "inbox": null,
        "outbox": null,
        "publicKey": {
            "id": remote.actor.public_key.id,
            "owner": remote.actor.id,
            "publicKeyPem": remote.keys.public_pem,
        },
    });
    (remote, document)
}

fn internal_fetch_stub(remote: &RemoteUser, document: Value) -> Arc<StubFederation> {
    let stub = StubFederation::with_actors([]);
    stub.objects
        .lock()
        .unwrap()
        .insert(remote.actor.id.clone(), document);
    stub
}

async fn assert_no_fetch_identity_written(pool: &PgPool, remote: &RemoteUser) {
    assert!(
        account::find_by_uri(pool, &remote.actor.id)
            .await
            .unwrap()
            .is_none()
    );
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM actor_keys WHERE controller_uri = $1")
        .bind(&remote.actor.id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        keys, 0,
        "fetch-only authentication must not persist actor keys"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_authenticates_null_and_missing_inbox_without_account_writes(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, mut document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document.clone());
    let app = || test_app_secure(pool.clone(), stub.clone());
    let (status, actor) = get_signed(app(), "/users/alice", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actor["id"], "https://plamenu.test/users/alice");
    assert_no_fetch_identity_written(&pool, &remote).await;

    document.as_object_mut().unwrap().remove("inbox");
    document.as_object_mut().unwrap().remove("outbox");
    stub.objects
        .lock()
        .unwrap()
        .insert(remote.actor.id.clone(), document);
    let (status, _) = get_signed(app(), "/users/alice", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.fetches().len(),
        2,
        "fetch-only keys refetch on each request"
    );
    assert_no_fetch_identity_written(&pool, &remote).await;

    let (status, _) = get_signed(
        app(),
        "/users/alice/followers_synchronization",
        &remote.signer(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no account/audience identity"
    );
    assert_eq!(
        stub.fetches().len(),
        3,
        "outer secure gate fetches once; account-only handler rejects before cache/fetch"
    );
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_rejects_malformed_foreign_and_unowned_key_documents(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    let mut variants = Vec::new();
    for (field, value) in [
        ("id", json!("https://foreign.example/internal/fetch")),
        ("type", json!("Person")),
        ("preferredUsername", json!("ordinary.user")),
        ("inbox", json!({"id": "https://fetch.example/inbox"})),
    ] {
        let mut malformed = document.clone();
        malformed[field] = value;
        variants.push(malformed);
    }
    for (field, value) in [
        (
            "id",
            json!("https://foreign.example/internal/fetch#main-key"),
        ),
        (
            "id",
            json!("https://fetch.example/internal/fetch#unselected-key"),
        ),
        ("owner", json!("https://foreign.example/internal/fetch")),
        ("publicKeyPem", json!("malformed PEM")),
    ] {
        let mut malformed = document.clone();
        malformed["publicKey"][field] = value;
        variants.push(malformed);
    }
    let mut missing = document.clone();
    missing.as_object_mut().unwrap().remove("publicKey");
    variants.push(missing);
    for malformed in variants {
        let stub = internal_fetch_stub(&remote, malformed);
        let (status, _) = get_signed(
            test_app_secure(pool.clone(), stub),
            "/users/alice",
            &remote.signer(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_no_fetch_identity_written(&pool, &remote).await;
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_rotation_uses_current_key_and_rejects_old_signature(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (old, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&old, document);
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &old.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (new, rotated) = internal_fetch_fixture();
    stub.objects
        .lock()
        .unwrap()
        .insert(new.actor.id.clone(), rotated);
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &new.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &old.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(stub.fetches().len(), 3);
    assert_no_fetch_identity_written(&pool, &new).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_rejects_unsupported_cavage_algorithm_label(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let signed =
        remote
            .signer()
            .sign_get("plamenu.test", "/users/alice", ACCEPT_AP, SystemTime::now());
    let request = Request::builder()
        .method("GET")
        .uri("/users/alice")
        .header("host", "plamenu.test")
        .header("date", signed.date)
        .header(header::ACCEPT, ACCEPT_AP)
        .header(
            "signature",
            signed.signature.replace("rsa-sha256", "ed25519"),
        )
        .body(Body::empty())
        .unwrap();
    let response = test_app_secure(pool.clone(), stub.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        stub.fetches().is_empty(),
        "reject label before network work"
    );
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_domain_block_denies_before_fetch(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "fetch.example",
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice",
        &remote.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(stub.fetches().is_empty());
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[test]
fn normal_delivery_actor_still_rejects_null_or_absent_inbox() {
    let (_, mut document) = internal_fetch_fixture();
    assert!(serde_json::from_value::<plamenu_ap::actor::RemoteActor>(document.clone()).is_err());
    document.as_object_mut().unwrap().remove("inbox");
    assert!(serde_json::from_value::<plamenu_ap::actor::RemoteActor>(document).is_err());
}

#[tokio::test]
async fn real_internal_fetch_transport_denies_literal_private_address() {
    let client =
        plamenu_federation::FederationClient::new("signer-security-regression", false).unwrap();
    let result = client
        .fetch_object("https://127.0.0.1/internal/fetch")
        .await;
    assert!(matches!(
        result,
        Err(plamenu_federation::FederationError::PrivateAddress(_))
    ));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_outbox_preserves_origin_domain_block(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "public control", "public", None),
    )
    .await
    .unwrap();
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let app = || test_app_secure(pool.clone(), stub.clone());
    let (status, page) = get_signed(app(), "/users/alice/outbox?page=true", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["orderedItems"].as_array().unwrap().len(), 1);
    plamenu_db::account_domain_block::create(&pool, alice.id, "fetch.example")
        .await
        .unwrap();
    let (status, page) = get_signed(app(), "/users/alice/outbox?page=true", &remote.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page["orderedItems"].as_array().unwrap().is_empty());
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_outbox_revalidation_failure_is_fail_closed(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "must remain blocked", "public", None),
    )
    .await
    .unwrap();
    plamenu_db::account_domain_block::create(&pool, alice.id, "fetch.example")
        .await
        .unwrap();
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *stub.object_fetch_gate.lock().unwrap() = Some(gate.clone());
    let app = test_app_secure(pool.clone(), stub.clone());
    let signer = remote.signer();
    let request =
        tokio::spawn(
            async move { get_signed(app, "/users/alice/outbox?page=true", &signer).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while stub.fetches().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // First fetch already captured a valid document; only the origin-policy
    // revalidation loses it. It must not fall back to an anonymous outbox.
    stub.objects.lock().unwrap().remove(&remote.actor.id);
    *stub.object_fetch_gate.lock().unwrap() = None;
    gate.add_permits(1);
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.get("orderedItems").is_none());
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_local_origin_variants_reject_before_transport(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    for origin in ["https://PLAMENU.TEST", "https://plamenu.test:443"] {
        let (mut remote, _) = internal_fetch_fixture();
        remote.actor.id = format!("{origin}/internal/fetch");
        remote.actor.public_key.id = format!("{}#main-key", remote.actor.id);
        remote.actor.public_key.owner.clone_from(&remote.actor.id);
        let stub = StubFederation::with_actors([]);
        let (status, _) = get_signed(
            test_app_secure(pool.clone(), stub.clone()),
            "/users/alice",
            &remote.signer(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            stub.fetches().is_empty(),
            "normalized self host must not be dereferenced"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_cannot_read_private_context_history(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let state = common::test_state_containers(pool.clone(), stub);
    let (root, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "private control",
            visibility: "private",
            media_ids: &[],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let conversation = plamenu_db::conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();
    let (status, _) = get_signed(
        plamenu::build_router(state),
        &format!("/contexts/{conversation}/history"),
        &remote.signer(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "GET authentication never grants private audience membership"
    );
    assert_no_fetch_identity_written(&pool, &remote).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_accepts_supported_cavage_labels_and_omission(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    for label in [Some("rsa-sha256"), Some("hs2019"), None] {
        let signed =
            remote
                .signer()
                .sign_get("plamenu.test", "/users/alice", ACCEPT_AP, SystemTime::now());
        let signature = match label {
            Some(label) => signed.signature.replace("rsa-sha256", label),
            None => signed.signature.replace("algorithm=\"rsa-sha256\",", ""),
        };
        let request = Request::builder()
            .method("GET")
            .uri("/users/alice")
            .header("host", "plamenu.test")
            .header("date", signed.date)
            .header(header::ACCEPT, ACCEPT_AP)
            .header("signature", signature)
            .body(Body::empty())
            .unwrap();
        let response = test_app_secure(pool.clone(), stub.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "supported Cavage label {label:?}"
        );
        assert_no_fetch_identity_written(&pool, &remote).await;
    }
}

#[tokio::test]
async fn real_internal_fetch_transport_private_dns_never_reaches_connector() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let destination = format!(
        "https://localhost:{}/internal/fetch",
        listener.local_addr().unwrap().port(),
    );
    // A hostname passes literal syntax checks, then the actual SafeResolver
    // must reject its private DNS answers before attempting TLS/connection.
    let answers: Vec<_> = tokio::net::lookup_host(("localhost", 0))
        .await
        .unwrap()
        .collect();
    assert!(!answers.is_empty());
    assert!(
        answers
            .iter()
            .all(|answer| plamenu_federation::guard::is_private_ip(answer.ip()))
    );
    let client =
        plamenu_federation::FederationClient::new("signer-private-dns-regression", false).unwrap();
    assert!(client.fetch_object(&destination).await.is_err());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_preexisting_account_cannot_grant_private_audience(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    // Model a prior-version account record with a real-looking inbox/key.
    let previous = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let stub = internal_fetch_stub(&remote, document);
    let state = common::test_state_containers(pool.clone(), stub.clone());
    let (root, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "private cached-account control",
            visibility: "private",
            media_ids: &[],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    plamenu_db::mention::attach(&pool, root.id, previous.id, false)
        .await
        .unwrap();
    let conversation = plamenu_db::conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();
    let (status, _) = get_signed(
        plamenu::build_router(state),
        &format!("/contexts/{conversation}/history"),
        &remote.signer(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "fetch-only principal must bypass cached Account membership"
    );
    let (status, _) = get_signed(
        test_app_secure(pool.clone(), stub.clone()),
        "/users/alice/followers_synchronization",
        &remote.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let current = account::find_by_uri(&pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.id, previous.id);
    assert_eq!(
        current.inbox_url, previous.inbox_url,
        "auth-only requests never promote or refresh this account"
    );
    assert_eq!(
        stub.fetches(),
        vec![remote.actor.id.clone()],
        "outer secure gate fetches once; account-only handler rejects before cache/fetch"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_rejects_malformed_rfc9421_input_before_fetch_or_writes(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (remote, document) = internal_fetch_fixture();
    let stub = internal_fetch_stub(&remote, document);
    let created = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key_id = &remote.actor.public_key.id;
    let cases = [
        ("sig1=bogus".to_owned(), "sig1=:AA==:"),
        (
            format!("sig1=(\"@method\" \"@target-uri\");created={created};alg=\"rsa-v1_5-sha256\""),
            "sig1=:AA==:",
        ),
        (
            format!(
                "sig1=(\"@method\" \"@target-uri\");created={created};keyid=\"{key_id}\";alg=\"rsa-v1_5-sha256\""
            ),
            "sig2=:AA==:",
        ),
        (
            format!(
                "sig1=(\"@method\" \"@target-uri\");created=1;keyid=\"{key_id}\";alg=\"rsa-v1_5-sha256\""
            ),
            "sig1=:AA==:",
        ),
    ];
    for (input, signature) in cases {
        let request = Request::builder()
            .method("GET")
            .uri("/users/alice")
            .header("host", "plamenu.test")
            .header(header::ACCEPT, ACCEPT_AP)
            .header("signature-input", input)
            .header("signature", signature)
            .body(Body::empty())
            .unwrap();
        let response = test_app_secure(pool.clone(), stub.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(stub.fetches().is_empty());
        assert_no_fetch_identity_written(&pool, &remote).await;
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn internal_fetch_query_and_userinfo_actor_uris_reject_before_transport(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    for actor_id in [
        "https://fetch.example/internal/fetch?ignored=1",
        "https://userinfo@fetch.example/internal/fetch",
    ] {
        let (mut remote, _) = internal_fetch_fixture();
        remote.actor.id = actor_id.into();
        remote.actor.public_key.id = format!("{actor_id}#main-key");
        remote.actor.public_key.owner = actor_id.into();
        let stub = StubFederation::with_actors([]);
        let (status, _) = get_signed(
            test_app_secure(pool.clone(), stub.clone()),
            "/users/alice",
            &remote.signer(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(stub.fetches().is_empty());
    }
}
