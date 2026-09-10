//! Integration tests for `WebAuthn` security keys.
//!
//! A full crypto round-trip needs a real authenticator, so these tests cover
//! the wiring around webauthn-rs rather than its verification: that the option
//! endpoints are gated and persist ceremony state, that malformed finishes are
//! rejected, that the login challenge offers the security-key path only when a
//! key is registered, and that removal / the TOTP-disable cascade work. The
//! crypto itself is covered by webauthn-rs's own test suite.
//!
//! The session trick: we sign in with the password *before* enabling TOTP, so a
//! plain password login yields a cookie; the `WebUser` extractor re-reads
//! `otp_required_for_login` on every request, so enabling TOTP afterwards still
//! puts the account into the two-factor state the registration surface needs.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_secret};
use plamenu_db::{PgPool, oauth, user, webauthn_credential};
use serde_json::{Value, json};
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
        // Localized pages carry Fluent's directional isolates around
        // interpolated values; strip them so assertions read as the page does.
        body: String::from_utf8(bytes.to_vec())
            .unwrap()
            .replace(['\u{2068}', '\u{2069}'], ""),
    }
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn post_form(app: &Router, uri: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body)).unwrap()).await
}

async fn post_json(app: &Router, uri: &str, cookie: Option<&str>, body: &Value) -> Resp {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body.to_string())).unwrap()).await
}

/// A Russian-preferring GET. No account in these tests stores an interface
/// locale, so `Accept-Language` decides.
async fn get_in_russian(app: &Router, uri: &str, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT_LANGUAGE, "ru");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

/// A Russian-preferring form POST.
async fn post_form_in_russian(app: &Router, uri: &str, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT_LANGUAGE, "ru");
    send(app, request.body(Body::from(body)).unwrap()).await
}

/// A Russian-preferring JSON POST — the shim's own requests carry the header,
/// so the rejections it renders come back translated.
async fn post_json_in_russian(app: &Router, uri: &str, cookie: &str, body: &Value) -> Resp {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_LANGUAGE, "ru")
        .header(header::COOKIE, cookie);
    send(app, request.body(Body::from(body.to_string())).unwrap()).await
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

fn hidden_value(body: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = body
        .find(&marker)
        .unwrap_or_else(|| panic!("missing hidden {name}"))
        + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

fn csrf(body: &str) -> String {
    hidden_value(body, "csrf")
}

// ---- Fixtures ----------------------------------------------------------

async fn seed_alice(pool: &PgPool) -> i64 {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(EMAIL), &hash)
        .await
        .unwrap()
        .id
}

/// Turns on the TOTP requirement without any real secret — these tests never
/// complete a TOTP challenge, they only need `otp_required_for_login` set (the
/// gate `WebAuthn` registration sits behind).
async fn enable_totp(pool: &PgPool, user_id: i64) {
    user::set_otp_secret(pool, user_id, "dummy-encrypted")
        .await
        .unwrap();
    assert!(user::enable_otp(pool, user_id).await.unwrap());
}

/// A password-only login (valid while the account has no TOTP requirement yet),
/// returning the session cookie pair.
async fn password_session(app: &Router) -> String {
    let resp = post_form(
        app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    cookie_pair(&resp.set_cookie.expect("session cookie")).to_owned()
}

/// A well-formed-but-bogus registration attestation (valid base64url fields, so
/// it deserializes; not a real credential, so verification must fail).
fn junk_attestation() -> Value {
    json!({
        "id": "AAAA",
        "rawId": "AAAA",
        "type": "public-key",
        "response": { "attestationObject": "AAAA", "clientDataJSON": "AAAA" }
    })
}

/// A well-formed-but-bogus login assertion.
fn junk_assertion() -> Value {
    json!({
        "id": "AAAA",
        "rawId": "AAAA",
        "type": "public-key",
        "response": {
            "authenticatorData": "AAAA",
            "clientDataJSON": "AAAA",
            "signature": "AAAA"
        }
    })
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn register_options_needs_totp_then_persists_state(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = password_session(&app).await;

    // Without TOTP the account cannot add a key.
    let settings = get(&app, "/settings/security", Some(&cookie)).await;
    let too_early = post_json(
        &app,
        "/web/settings/webauthn/options",
        Some(&cookie),
        &json!({ "csrf": csrf(&settings.body), "nickname": "YubiKey" }),
    )
    .await;
    assert_eq!(too_early.status, StatusCode::UNPROCESSABLE_ENTITY);

    // With TOTP on, options come back with a challenge and are stashed server-side.
    enable_totp(&pool, uid).await;
    let settings = get(&app, "/settings/security", Some(&cookie)).await;
    assert!(settings.body.contains("Security keys"));
    let token_csrf = csrf(&settings.body);
    let resp = post_json(
        &app,
        "/web/settings/webauthn/options",
        Some(&cookie),
        &json!({ "csrf": token_csrf, "nickname": "YubiKey" }),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&resp.body).unwrap();
    let challenge_token = body["challenge_token"].as_str().expect("challenge token");
    assert!(body["options"]["publicKey"]["challenge"].is_string());
    assert!(body["options"]["publicKey"]["user"]["id"].is_string());

    let challenge = plamenu_db::two_factor::find_challenge(
        &pool,
        &hash_secret(challenge_token),
        "webauthn_reg",
    )
    .await
    .unwrap()
    .expect("registration challenge stored");
    assert_eq!(challenge.user_id, uid);
    assert!(challenge.webauthn_state.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn register_options_rejects_bad_csrf(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = password_session(&app).await;
    enable_totp(&pool, uid).await;

    let resp = post_json(
        &app,
        "/web/settings/webauthn/options",
        Some(&cookie),
        &json!({ "csrf": "not-the-token", "nickname": "YubiKey" }),
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn register_finish_rejects_garbage_attestation(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = password_session(&app).await;
    enable_totp(&pool, uid).await;

    let settings = get(&app, "/settings/security", Some(&cookie)).await;
    let csrf = csrf(&settings.body);
    let options = post_json(
        &app,
        "/web/settings/webauthn/options",
        Some(&cookie),
        &json!({ "csrf": csrf, "nickname": "YubiKey" }),
    )
    .await;
    let body: Value = serde_json::from_str(&options.body).unwrap();
    let challenge_token = body["challenge_token"].as_str().unwrap();

    let finish = post_json(
        &app,
        "/web/settings/webauthn",
        Some(&cookie),
        &json!({
            "csrf": csrf,
            "challenge_token": challenge_token,
            "credential": junk_attestation(),
        }),
    )
    .await;
    assert!(finish.status.is_client_error(), "status {}", finish.status);
    assert_eq!(
        webauthn_credential::count_by_user(&pool, uid)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_challenge_offers_key_only_when_registered(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool.clone());

    // No key yet: the challenge page shows only the TOTP form.
    let without = post_form(
        &app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(without.status, StatusCode::OK);
    assert!(!without.body.contains("data-webauthn-authenticate"));

    // Register a key (directly), and the security-key button appears.
    webauthn_credential::create(&pool, uid, "ext-1", "Key", &json!({ "k": 1 }), 0)
        .await
        .unwrap();
    let with = post_form(
        &app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await;
    assert!(with.body.contains("data-webauthn-authenticate"));
    assert!(with.body.contains("Use a security key"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_options_requires_a_live_challenge_and_keys(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);

    // An unknown token is a dead login.
    let unknown = post_json(
        &app,
        "/login/webauthn/options",
        None,
        &json!({ "challenge_token": "deadbeef" }),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    // A live challenge for an account with no keys cannot start an assertion.
    let pw = post_form(
        &app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await;
    let token = hidden_value(&pw.body, "challenge_token");
    let no_keys = post_json(
        &app,
        "/login/webauthn/options",
        None,
        &json!({ "challenge_token": token }),
    )
    .await;
    assert_eq!(no_keys.status, StatusCode::UNPROCESSABLE_ENTITY);
    let _ = uid;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_finish_without_ceremony_state_is_rejected(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);

    // A password challenge exists but the options step (which stores the auth
    // state) never ran, so a finish cannot proceed.
    let pw = post_form(
        &app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await;
    let token = hidden_value(&pw.body, "challenge_token");
    let finish = post_json(
        &app,
        "/login/webauthn",
        None,
        &json!({ "challenge_token": token, "credential": junk_assertion() }),
    )
    .await;
    assert_eq!(finish.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(finish.set_cookie.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn removing_a_key_and_disabling_totp_clear_credentials(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = password_session(&app).await;
    enable_totp(&pool, uid).await;
    let first = webauthn_credential::create(&pool, uid, "ext-1", "Key A", &json!({}), 0)
        .await
        .unwrap();
    webauthn_credential::create(&pool, uid, "ext-2", "Key B", &json!({}), 0)
        .await
        .unwrap();

    // The settings page lists the keys with remove forms.
    let settings = get(&app, "/settings/security", Some(&cookie)).await;
    assert!(settings.body.contains("Key A"));
    assert!(settings.body.contains("Key B"));
    let csrf = csrf(&settings.body);

    // Removing one leaves the other.
    let removed = post_form(
        &app,
        &format!("/web/settings/webauthn/{}/delete", first.id),
        Some(&cookie),
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert_eq!(
        webauthn_credential::count_by_user(&pool, uid)
            .await
            .unwrap(),
        1
    );

    // Turning TOTP off removes every remaining key (Mastodon's model).
    let disabled = post_form(
        &app,
        "/web/settings/two_factor/disable",
        Some(&cookie),
        &[("csrf", &csrf), ("current_password", PASSWORD)],
    )
    .await;
    assert_eq!(
        disabled.location.as_deref(),
        Some("/settings/security?saved=disabled")
    );
    assert_eq!(
        webauthn_credential::count_by_user(&pool, uid)
            .await
            .unwrap(),
        0
    );
}

// ---- OAuth (3rd-party) sign-in --------------------------------------------
//
// The `/oauth/authorize` flow mirrors the first-party sign-in: after a correct
// password, a 2FA account is offered TOTP and — when a key is registered — the
// security-key path, verified sessionlessly against a `context = "oauth"`
// challenge. These cover the same wiring the `login_*` tests cover for the web.

const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";

/// Registers an OAuth app (out-of-band redirect) and returns its `client_id`.
async fn oauth_app(pool: &PgPool) -> String {
    let client_id = generate_secret();
    oauth::create_app(
        pool,
        oauth::NewApp {
            name: "webauthn-oauth-tests",
            website: None,
            client_id: &client_id,
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[OOB.to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    client_id
}

/// Submits the password step of `/oauth/authorize` for a 2FA account, returning
/// the rendered challenge page (which carries the challenge token).
async fn oauth_password_step(app: &Router, client_id: &str) -> Resp {
    post_form(
        app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", client_id),
            ("redirect_uri", OOB),
            ("scope", "read"),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_challenge_offers_key_only_when_registered(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let client_id = oauth_app(&pool).await;
    let app = common::test_app(pool.clone());

    // No key yet: the 2FA prompt shows only the TOTP form.
    let without = oauth_password_step(&app, &client_id).await;
    assert_eq!(without.status, StatusCode::OK);
    assert!(without.body.contains("Two-factor authentication"));
    assert!(!without.body.contains("data-webauthn-authenticate"));

    // Register a key: the security-key button appears, aimed at the OAuth
    // ceremony endpoints.
    webauthn_credential::create(&pool, uid, "ext-1", "Key", &json!({ "k": 1 }), 0)
        .await
        .unwrap();
    let with = oauth_password_step(&app, &client_id).await;
    assert!(with.body.contains("Use a security key"));
    assert!(with.body.contains("/oauth/authorize/webauthn/options"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_login_options_requires_a_live_challenge_and_keys(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let client_id = oauth_app(&pool).await;
    let app = common::test_app(pool);

    // An unknown token is a dead login.
    let unknown = post_json(
        &app,
        "/oauth/authorize/webauthn/options",
        None,
        &json!({ "challenge_token": "deadbeef" }),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    // A live challenge for an account with no keys cannot start an assertion.
    let pw = oauth_password_step(&app, &client_id).await;
    let token = hidden_value(&pw.body, "challenge_token");
    let no_keys = post_json(
        &app,
        "/oauth/authorize/webauthn/options",
        None,
        &json!({ "challenge_token": token }),
    )
    .await;
    assert_eq!(no_keys.status, StatusCode::UNPROCESSABLE_ENTITY);
    let _ = uid;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_login_finish_without_ceremony_state_is_rejected(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let client_id = oauth_app(&pool).await;
    let app = common::test_app(pool.clone());
    // A key is registered (so the prompt offers the path), but the options step
    // that stashes the ceremony state never ran, so a finish cannot proceed.
    webauthn_credential::create(&pool, uid, "ext-1", "Key", &json!({ "k": 1 }), 0)
        .await
        .unwrap();

    let pw = oauth_password_step(&app, &client_id).await;
    let token = hidden_value(&pw.body, "challenge_token");
    let finish = post_json(
        &app,
        "/oauth/authorize/webauthn",
        None,
        &json!({
            "challenge_token": token,
            "credential": junk_assertion(),
            "client_id": client_id,
            "redirect_uri": OOB,
            "scope": "read",
        }),
    )
    .await;
    assert_eq!(finish.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(finish.set_cookie.is_none());
}

// ---- Localization ----------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn security_keys_block_negotiates_russian(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = password_session(&app).await;
    enable_totp(&pool, uid).await;

    let settings = get_in_russian(&app, "/settings/security", Some(&cookie)).await;
    assert!(settings.body.contains("Ключи безопасности"));
    assert!(
        settings
            .body
            .contains("Ключи безопасности пока не зарегистрированы.")
    );
    assert!(settings.body.contains("Добавить ключ безопасности"));
    // The shim has no catalog: its status wording rides along as data-*.
    assert!(
        settings
            .body
            .contains(r#"data-webauthn-prompt="Следуйте указаниям браузера…""#)
    );
    assert!(!settings.body.contains("Security keys"));

    // The rejections the shim renders verbatim are localized too.
    let no_name = post_json_in_russian(
        &app,
        "/web/settings/webauthn/options",
        &cookie,
        &json!({ "csrf": csrf(&settings.body), "nickname": "  " }),
    )
    .await;
    assert_eq!(no_name.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        no_name.body.contains("Дайте ключу безопасности название."),
        "body {}",
        no_name.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_consent_and_key_prompt_negotiate_russian(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    webauthn_credential::create(&pool, uid, "ext-1", "Key", &json!({ "k": 1 }), 0)
        .await
        .unwrap();
    let client_id = oauth_app(&pool).await;
    let app = common::test_app(pool.clone());

    // The sessionless consent screen negotiates the header, document language
    // included.
    let consent = get(
        &app,
        &format!("/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={OOB}&scope=read"),
        None,
    )
    .await;
    assert!(consent.body.contains("Authorize access"));
    let consent = send(
        &app,
        Request::builder()
            .uri(format!(
                "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={OOB}&scope=read"
            ))
            .header(header::ACCEPT_LANGUAGE, "ru")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(consent.body.contains("lang=\"ru\""));
    assert!(consent.body.contains("Разрешить доступ"));
    assert!(consent.body.contains("webauthn-oauth-tests"));

    // …and so does the second-factor prompt it leads to, security key included.
    let challenge = post_form_in_russian(
        &app,
        "/oauth/authorize",
        &[
            ("client_id", &client_id),
            ("redirect_uri", OOB),
            ("scope", "read"),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(challenge.status, StatusCode::OK);
    assert!(challenge.body.contains("Использовать ключ безопасности"));
    assert!(challenge.body.contains("/oauth/authorize/webauthn/options"));
    assert!(!challenge.body.contains("Two-factor authentication"));
}
