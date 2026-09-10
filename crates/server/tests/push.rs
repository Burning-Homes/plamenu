//! Web Push: subscription CRUD parity and the dispatch pipeline — the
//! fan-out trigger, the worker, and client-side decryption of both content
//! encodings against the recorded deliveries.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use common::{StubFederation, create_local_account, test_state_with};
use hkdf::Hkdf;
use http_body_util::BodyExt;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{AppState, build_router};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, notification, oauth, user, web_push};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

/// A local account with login credentials and an access token of `scopes`,
/// minted directly (the OAuth issuance flow is covered in `client_api.rs`).
/// Returns the account, its user row id and the bearer token.
async fn user_with_token(pool: &PgPool, username: &str, scopes: &str) -> (Account, i64, String) {
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
            name: "push-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write push",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(row.id), scopes)
        .await
        .unwrap();
    (account, row.id, token)
}

async fn api(
    app: Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(json) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// A browser-side subscription keypair: (p256dh, auth) as sent to the API,
/// plus the secret halves for decrypting deliveries.
fn client_keys() -> (String, String, p256::SecretKey, [u8; 16]) {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).unwrap();
    let secret = p256::SecretKey::from_slice(&bytes).unwrap();
    let mut auth = [0u8; 16];
    getrandom::fill(&mut auth).unwrap();
    let p256dh = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
    (p256dh, URL_SAFE_NO_PAD.encode(auth), secret, auth)
}

fn subscription_body(p256dh: &str, auth: &str, alerts: &Value) -> Value {
    json!({
        "subscription": {
            "endpoint": "https://push.example/wpush/abc123",
            "keys": { "p256dh": p256dh, "auth": auth },
        },
        "data": { "alerts": alerts },
    })
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], okm: &mut [u8]) {
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, okm)
        .unwrap();
}

fn aes128gcm_open(cek: &[u8; 16], nonce: &[u8; 12], ciphertext: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
    Aes128Gcm::new_from_slice(cek)
        .unwrap()
        .decrypt(
            &Nonce::try_from(&nonce[..]).expect("nonce is 12 bytes"),
            ciphertext,
        )
        .unwrap()
}

/// Decrypts a legacy `aesgcm` delivery from its body + headers.
fn decrypt_legacy(
    ua_secret: &p256::SecretKey,
    auth: &[u8],
    headers: &[(String, String)],
    body: &[u8],
) -> Value {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    };
    let salt = URL_SAFE_NO_PAD
        .decode(header("Encryption").strip_prefix("salt=").unwrap())
        .unwrap();
    let dh = header("Crypto-Key")
        .split(';')
        .find_map(|part| part.strip_prefix("dh="))
        .unwrap();
    let as_public = p256::PublicKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(dh).unwrap()).unwrap();

    let ua_public = ua_secret.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
    let mut ikm = [0u8; 32];
    hkdf(
        auth,
        shared.raw_secret_bytes(),
        b"Content-Encoding: auth\x00",
        &mut ikm,
    );
    let mut context = b"P-256\x00".to_vec();
    context.extend_from_slice(&65u16.to_be_bytes());
    context.extend_from_slice(ua_public.as_bytes());
    context.extend_from_slice(&65u16.to_be_bytes());
    context.extend_from_slice(as_public.to_sec1_point(false).as_bytes());
    let mut cek_info = b"Content-Encoding: aesgcm\x00".to_vec();
    cek_info.extend_from_slice(&context);
    let mut nonce_info = b"Content-Encoding: nonce\x00".to_vec();
    nonce_info.extend_from_slice(&context);
    let mut cek = [0u8; 16];
    hkdf(&salt, &ikm, &cek_info, &mut cek);
    let mut nonce = [0u8; 12];
    hkdf(&salt, &ikm, &nonce_info, &mut nonce);

    let padded = aes128gcm_open(&cek, &nonce, body);
    assert_eq!(&padded[..2], &[0, 0], "legacy padding prefix");
    serde_json::from_slice(&padded[2..]).unwrap()
}

/// Decrypts an RFC 8291 `aes128gcm` delivery from its self-describing body.
fn decrypt_standard(ua_secret: &p256::SecretKey, auth: &[u8], body: &[u8]) -> Value {
    let salt = &body[..16];
    let id_len = usize::from(body[20]);
    assert_eq!(id_len, 65, "keyid is the ephemeral public key");
    let as_public = p256::PublicKey::from_sec1_bytes(&body[21..21 + id_len]).unwrap();
    let ciphertext = &body[21 + id_len..];

    let ua_public = ua_secret.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
    let mut key_info = b"WebPush: info\x00".to_vec();
    key_info.extend_from_slice(ua_public.as_bytes());
    key_info.extend_from_slice(as_public.to_sec1_point(false).as_bytes());
    let mut ikm = [0u8; 32];
    hkdf(auth, shared.raw_secret_bytes(), &key_info, &mut ikm);
    let mut cek = [0u8; 16];
    hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\x00", &mut cek);
    let mut nonce = [0u8; 12];
    hkdf(salt, &ikm, b"Content-Encoding: nonce\x00", &mut nonce);

    let record = aes128gcm_open(&cek, &nonce, ciphertext);
    assert_eq!(record.last(), Some(&0x02), "last-record delimiter");
    serde_json::from_slice(&record[..record.len() - 1]).unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_show_update_delete_lifecycle(pool: PgPool) {
    let (_, _, token) = user_with_token(&pool, "alice", "read write push").await;
    let app = || build_router(test_state_with(pool.clone(), Arc::default()));
    let (p256dh, auth, _, _) = client_keys();

    // No subscription yet.
    let (status, body) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");

    // Create: alert values arrive as strings from form-driven clients and
    // are cast to booleans; policy defaults to "all".
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(subscription_body(
            &p256dh,
            &auth,
            &json!({"mention": "true", "favourite": false, "bogus_kind": true}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["id"].is_i64(), "id is a bare integer: {body}");
    assert_eq!(body["endpoint"], "https://push.example/wpush/abc123");
    assert_eq!(body["standard"], false);
    assert_eq!(body["alerts"], json!({"mention": true, "favourite": false}));
    assert_eq!(body["policy"], "all");
    let server_key = body["server_key"].as_str().unwrap().to_owned();
    assert!(!server_key.is_empty());

    // The instance document advertises the same VAPID key.
    let (_, instance) = api(app(), "GET", "/api/v2/instance", None, None).await;
    assert_eq!(
        instance["configuration"]["vapid"]["public_key"],
        Value::String(server_key.clone())
    );

    // Re-creating replaces (new row, same singleton slot).
    let first_id = body["id"].as_i64().unwrap();
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(subscription_body(&p256dh, &auth, &json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(body["id"].as_i64().unwrap(), first_id);
    assert_eq!(body["server_key"].as_str().unwrap(), server_key);

    // Update replaces data only.
    let (status, body) = api(
        app(),
        "PUT",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"data": {"policy": "followed", "alerts": {"follow": true}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["policy"], "followed");
    assert_eq!(body["alerts"], json!({"follow": true}));
    assert_eq!(body["endpoint"], "https://push.example/wpush/abc123");

    // Delete answers an empty object and is idempotent.
    let (status, body) = api(
        app(),
        "DELETE",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (status, _) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api(
        app(),
        "DELETE",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // PUT without a subscription is a 404, like Mastodon.
    let (status, _) = api(
        app(),
        "PUT",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"data": {"policy": "all"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn subscriptions_are_per_token(pool: PgPool) {
    let (_, user_id, token_a) = user_with_token(&pool, "alice", "read write push").await;
    // A second token for the same user.
    let token_b = generate_secret();
    let app_row = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "second-client",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: "x",
            redirect_uris: &[],
            scopes: "push",
        },
    )
    .await
    .unwrap();
    oauth::create_token(
        &pool,
        &hash_secret(&token_b),
        app_row.id,
        Some(user_id),
        "push",
    )
    .await
    .unwrap();

    let app = || build_router(test_state_with(pool.clone(), Arc::default()));
    let (p256dh, auth, _, _) = client_keys();
    for (token, endpoint) in [(&token_a, "a"), (&token_b, "b")] {
        let mut body = subscription_body(&p256dh, &auth, &json!({}));
        body["subscription"]["endpoint"] = json!(format!("https://push.example/{endpoint}"));
        let (status, _) = api(
            app(),
            "POST",
            "/api/v1/push/subscription",
            Some(token),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (_, body_a) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token_a),
        None,
    )
    .await;
    let (_, body_b) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token_b),
        None,
    )
    .await;
    assert_eq!(body_a["endpoint"], "https://push.example/a");
    assert_eq!(body_b["endpoint"], "https://push.example/b");

    // Deleting one token's subscription leaves the other's alone.
    api(
        app(),
        "DELETE",
        "/api/v1/push/subscription",
        Some(&token_a),
        None,
    )
    .await;
    let (status, _) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token_a),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api(
        app(),
        "GET",
        "/api/v1/push/subscription",
        Some(&token_b),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn push_scope_is_required(pool: PgPool) {
    let (_, _, token) = user_with_token(&pool, "alice", "read write").await;
    let app = || build_router(test_state_with(pool.clone(), Arc::default()));
    let (p256dh, auth, _, _) = client_keys();
    for (method, body) in [
        ("POST", Some(subscription_body(&p256dh, &auth, &json!({})))),
        ("GET", None),
        ("PUT", Some(json!({"data": {"policy": "all"}}))),
        ("DELETE", None),
    ] {
        let (status, body) = api(
            app(),
            method,
            "/api/v1/push/subscription",
            Some(&token),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
        assert_eq!(
            body["error"],
            "This action is outside the authorized scopes"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn validation_wording_matches_mastodon(pool: PgPool) {
    let (_, _, token) = user_with_token(&pool, "alice", "push").await;
    let app = || build_router(test_state_with(pool.clone(), Arc::default()));
    let (p256dh, auth, _, _) = client_keys();

    // Missing the whole subscription object.
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "param is missing or its value is empty: subscription"
    );

    // Missing keys.
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"subscription": {"endpoint": "https://push.example/x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "param is missing or its value is empty: keys"
    );

    // Everything blank: Rails collects all model errors in declaration order.
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"subscription": {"endpoint": "", "keys": {"p256dh": "", "auth": ""}}})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Endpoint can't be blank, Endpoint is invalid, \
         Key p256dh can't be blank, Key auth can't be blank, \
         is not a valid Ed25519 or Curve25519 key"
    );

    // Non-http(s) endpoint.
    let mut bad = subscription_body(&p256dh, &auth, &json!({}));
    bad["subscription"]["endpoint"] = json!("ftp://push.example/x");
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(bad),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Validation failed: Endpoint is invalid");

    // Well-formed base64 that is not a P-256 point.
    let mut bad = subscription_body(&p256dh, &auth, &json!({}));
    bad["subscription"]["keys"]["p256dh"] = json!(URL_SAFE_NO_PAD.encode([7u8; 65]));
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(bad),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: is not a valid Ed25519 or Curve25519 key"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rails_bracketed_form_bodies_parse(pool: PgPool) {
    let (_, _, token) = user_with_token(&pool, "alice", "push").await;
    let app = build_router(test_state_with(pool.clone(), Arc::default()));
    let (p256dh, auth, _, _) = client_keys();

    let form = format!(
        "subscription[endpoint]=https://push.example/form&\
         subscription[keys][p256dh]={p256dh}&\
         subscription[keys][auth]={auth}&\
         subscription[standard]=true&\
         data[alerts][favourite]=true&\
         data[policy]=follower"
    );
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/push/subscription")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["endpoint"], "https://push.example/form");
    assert_eq!(body["standard"], true);
    assert_eq!(body["alerts"], json!({"favourite": true}));
    assert_eq!(body["policy"], "follower");
}

/// Subscribes `token` with `extra` merged into the subscription object.
async fn subscribe(
    state: &AppState,
    token: &str,
    p256dh: &str,
    auth: &str,
    alerts: &Value,
    standard: bool,
) {
    let mut body = subscription_body(p256dh, auth, alerts);
    body["subscription"]["standard"] = json!(standard);
    let (status, response) = api(
        build_router(state.clone()),
        "POST",
        "/api/v1/push/subscription",
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
}

/// Posts a status as `token`; returns its id.
async fn post_status(state: &AppState, token: &str, text: &str) -> String {
    let (status, body) = api(
        build_router(state.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": text})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn favourite_dispatches_decryptable_legacy_push(pool: PgPool) {
    let federation = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), federation.clone());
    let (_, _, alice_token) = user_with_token(&pool, "alice", "read write push").await;
    let (_, _, bob_token) = user_with_token(&pool, "bob", "read write").await;

    let (p256dh, auth_b64, ua_secret, auth) = client_keys();
    subscribe(
        &state,
        &alice_token,
        &p256dh,
        &auth_b64,
        &json!({"favourite": true}),
        false,
    )
    .await;

    let status_id = post_status(&state, &alice_token, "Hello <world> & friends").await;
    let (code, _) = api(
        build_router(state.clone()),
        "POST",
        &format!("/api/v1/statuses/{status_id}/favourite"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    assert_eq!(web_push::pending_count(&pool).await.unwrap(), 1);
    assert_eq!(plamenu::web_push::run_due(&state).await, 1);
    assert_eq!(web_push::pending_count(&pool).await.unwrap(), 0);

    let pushes = federation.pushes();
    assert_eq!(pushes.len(), 1);
    let push = &pushes[0];
    assert_eq!(push.endpoint, "https://push.example/wpush/abc123");
    let header = |name: &str| {
        push.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    };
    assert_eq!(header("Ttl"), "172800");
    assert_eq!(header("Urgency"), "normal");
    assert_eq!(header("Content-Encoding"), "aesgcm");
    assert!(header("Authorization").starts_with("WebPush "));
    // Crypto-Key carries the ephemeral dh key and the VAPID key, unpadded.
    let (_, instance) = api(
        build_router(state.clone()),
        "GET",
        "/api/v2/instance",
        None,
        None,
    )
    .await;
    let server_key = instance["configuration"]["vapid"]["public_key"]
        .as_str()
        .unwrap();
    assert!(
        header("Crypto-Key").ends_with(&format!(";p256ecdsa={}", server_key.trim_end_matches('='))),
    );

    let payload = decrypt_legacy(&ua_secret, &auth, &push.headers, &push.body);
    assert_eq!(payload["access_token"], Value::String(alice_token.clone()));
    assert_eq!(payload["preferred_locale"], "en");
    assert_eq!(payload["notification_type"], "favourite");
    assert_eq!(payload["title"], "bob favorited your post");
    // Status content, tags stripped, entities decoded (the composer escaped
    // the raw `<world>`), truncated to 140.
    assert_eq!(payload["body"], "Hello <world> & friends");
    assert_eq!(payload["icon"], "https://plamenu.test/static/missing.png");

    // notification_id refers to the real notification (integer-valued, where
    // the listing's ids are strings).
    let (_, listing) = api(
        build_router(state.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let listed_id: i64 = listing[0]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(payload["notification_id"].as_i64().unwrap(), listed_id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn standard_subscription_gets_rfc8291_push(pool: PgPool) {
    let federation = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), federation.clone());
    let (alice, _, alice_token) = user_with_token(&pool, "alice", "read write push").await;
    let (bob, _, _) = user_with_token(&pool, "bob", "read").await;

    let (p256dh, auth_b64, ua_secret, auth) = client_keys();
    subscribe(
        &state,
        &alice_token,
        &p256dh,
        &auth_b64,
        &json!({"follow": true}),
        true,
    )
    .await;

    // A follow notification: its push body falls back to the sender's bio.
    notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();
    assert_eq!(plamenu::web_push::run_due(&state).await, 1);

    let pushes = federation.pushes();
    assert_eq!(pushes.len(), 1);
    let push = &pushes[0];
    let header = |name: &str| {
        push.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    };
    assert_eq!(header("Content-Encoding"), "aes128gcm");
    let authorization = header("Authorization");
    assert!(authorization.starts_with("vapid t="));

    // The VAPID JWT verifies against the advertised server key.
    let (_, instance) = api(
        build_router(state.clone()),
        "GET",
        "/api/v2/instance",
        None,
        None,
    )
    .await;
    let server_key = instance["configuration"]["vapid"]["public_key"]
        .as_str()
        .unwrap();
    let jwt = authorization
        .strip_prefix("vapid t=")
        .unwrap()
        .split(",k=")
        .next()
        .unwrap();
    assert_eq!(
        authorization.split(",k=").nth(1).unwrap(),
        server_key.trim_end_matches('=')
    );
    let verifying = VerifyingKey::from_sec1_bytes(&URL_SAFE.decode(server_key).unwrap()).unwrap();
    let (message, signature) = jwt.rsplit_once('.').unwrap();
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
    verifying.verify(message.as_bytes(), &signature).unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(message.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["aud"], "https://push.example");
    assert_eq!(claims["sub"], "mailto:admin@plamenu.test");

    let payload = decrypt_standard(&ua_secret, &auth, &push.body);
    assert_eq!(payload["notification_type"], "follow");
    assert_eq!(payload["title"], "bob is now following you");
    assert_eq!(payload["body"], "test account"); // bob's bio
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dead_subscriptions_are_pruned_on_4xx(pool: PgPool) {
    let federation = Arc::new(StubFederation::default());
    federation.set_push_status(410);
    let state = test_state_with(pool.clone(), federation.clone());
    let (alice, _, alice_token) = user_with_token(&pool, "alice", "read write push").await;
    let (bob, _, _) = user_with_token(&pool, "bob", "read").await;

    let (p256dh, auth_b64, _, _) = client_keys();
    subscribe(
        &state,
        &alice_token,
        &p256dh,
        &auth_b64,
        &json!({"follow": true}),
        false,
    )
    .await;
    notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();

    assert_eq!(plamenu::web_push::run_due(&state).await, 1);
    assert_eq!(federation.pushes().len(), 1);
    // The subscription died with its queued jobs.
    assert_eq!(web_push::pending_count(&pool).await.unwrap(), 0);
    let (status, _) = api(
        build_router(state.clone()),
        "GET",
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rate_limited_pushes_retry_then_give_up(pool: PgPool) {
    let federation = Arc::new(StubFederation::default());
    federation.set_push_status(429);
    let state = test_state_with(pool.clone(), federation.clone());
    let (alice, _, alice_token) = user_with_token(&pool, "alice", "read write push").await;
    let (bob, _, _) = user_with_token(&pool, "bob", "read").await;

    let (p256dh, auth_b64, _, _) = client_keys();
    subscribe(
        &state,
        &alice_token,
        &p256dh,
        &auth_b64,
        &json!({"follow": true}),
        false,
    )
    .await;
    notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();

    // Retried up to MAX_ATTEMPTS, then dropped — the subscription survives.
    for attempt in 1..=web_push::MAX_ATTEMPTS {
        web_push::make_all_due(&pool).await.unwrap();
        assert_eq!(
            plamenu::web_push::run_due(&state).await,
            1,
            "attempt {attempt}"
        );
        let expected_pending = u64::from(attempt < web_push::MAX_ATTEMPTS);
        assert_eq!(
            web_push::pending_count(&pool).await.unwrap(),
            expected_pending
        );
    }
    assert_eq!(
        federation.pushes().len(),
        usize::try_from(web_push::MAX_ATTEMPTS).unwrap()
    );
    let (status, _) = api(
        build_router(state.clone()),
        "GET",
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn worker_rechecks_pushability_before_sending(pool: PgPool) {
    let federation = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), federation.clone());
    let (alice, _, alice_token) = user_with_token(&pool, "alice", "read write push").await;
    let (bob, _, _) = user_with_token(&pool, "bob", "read").await;

    let (p256dh, auth_b64, _, _) = client_keys();
    subscribe(
        &state,
        &alice_token,
        &p256dh,
        &auth_b64,
        &json!({"follow": true}),
        false,
    )
    .await;
    notification::create(&pool, alice.id, bob.id, "follow", None)
        .await
        .unwrap();
    assert_eq!(web_push::pending_count(&pool).await.unwrap(), 1);

    // The user flips notifications off before the worker runs.
    let (status, _) = api(
        build_router(state.clone()),
        "PUT",
        "/api/v1/push/subscription",
        Some(&alice_token),
        Some(json!({"data": {"policy": "none", "alerts": {"follow": true}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(plamenu::web_push::run_due(&state).await, 1);
    assert!(federation.pushes().is_empty(), "nothing must be sent");
    assert_eq!(web_push::pending_count(&pool).await.unwrap(), 0);
}
