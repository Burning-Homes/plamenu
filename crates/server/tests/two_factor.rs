//! Integration tests for TOTP two-factor auth: enrolment, the web and
//! OAuth login challenges, replay/recovery/expiry handling, and disabling.
//!
//! The tests seed a known secret (the RFC 6238 sample) so they can compute
//! valid codes with `plamenu::totp::code_for` instead of reading them back out
//! of the server, and encrypt it exactly as `crypto::otp_box` does with the
//! test config's `encryption_secret`.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu::totp::PERIOD_SECONDS;
use plamenu_db::{PgPool, oauth, two_factor, user};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

/// The RFC 6238 sample 160-bit secret.
const SECRET_BYTES: &[u8] = b"12345678901234567890";

fn secret_b32() -> String {
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, SECRET_BYTES)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The valid TOTP code right now for [`SECRET_BYTES`].
fn code_now() -> String {
    plamenu::totp::code_for(SECRET_BYTES, now_epoch() / PERIOD_SECONDS)
}

// ---- HTTP harness ------------------------------------------------------

struct Resp {
    status: StatusCode,
    location: Option<String>,
    csp: Option<String>,
    set_cookie: Option<String>,
    set_cookies: Vec<String>,
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
    let csp = header(header::CONTENT_SECURITY_POLICY);
    let set_cookie = header(header::SET_COOKIE);
    let set_cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        csp,
        set_cookie,
        set_cookies,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn post(app: &Router, uri: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Resp {
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

async fn login_password(app: &Router) -> Resp {
    post(
        app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
    )
    .await
}

/// The `name=value` head of a `Set-Cookie`, ready to send back as `Cookie`.
fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

fn cookie_header(set_cookies: &[String]) -> String {
    set_cookies
        .iter()
        .map(|set_cookie| cookie_pair(set_cookie))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Pulls a hidden field's value out of rendered markup.
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

/// The text between two markers (first occurrence).
fn between<'a>(body: &'a str, start: &str, end: &str) -> &'a str {
    let s = body.find(start).expect("start marker") + start.len();
    let rest = &body[s..];
    &rest[..rest.find(end).expect("end marker")]
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

/// Turns on 2FA for `user_id` with the known [`SECRET_BYTES`], encrypted the
/// way `crypto::otp_box` does under the test config.
async fn enable_totp(pool: &PgPool, user_id: i64) {
    let secret_box = plamenu::crypto::otp_box(&common::test_config())
        .expect("the shared test config carries an encryption root");
    let encrypted = secret_box.encrypt(secret_b32().as_bytes());
    user::set_otp_secret(pool, user_id, &encrypted)
        .await
        .unwrap();
    assert!(user::enable_otp(pool, user_id).await.unwrap());
}

/// Logs in through the challenge and returns the session cookie pair.
async fn login_with_totp(app: &Router) -> String {
    let token = hidden_value(&login_password(app).await.body, "challenge_token");
    let done = post(
        app,
        "/login/challenge",
        None,
        &[("challenge_token", &token), ("code", &code_now())],
    )
    .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    cookie_pair(&done.set_cookie.expect("session cookie")).to_owned()
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn enrol_totp_shows_qr_and_hands_back_recovery_codes(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = cookie_pair(&login_password(&app).await.set_cookie.expect("cookie")).to_owned();

    // Starts on the enable form.
    let page = get(&app, "/settings/security", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Enable two-factor authentication"));

    // Setting up stashes a provisional secret and shows the QR; nothing is
    // enabled until a code is confirmed.
    let setup = post(
        &app,
        "/web/settings/two_factor/setup",
        Some(&cookie),
        &[("csrf", &csrf(&page.body)), ("current_password", PASSWORD)],
    )
    .await;
    assert_eq!(setup.status, StatusCode::SEE_OTHER);
    assert_eq!(setup.location.as_deref(), Some("/settings/security"));
    assert!(
        !user::find_by_id(&pool, uid)
            .await
            .unwrap()
            .unwrap()
            .otp_required_for_login
    );

    let provisional = get(&app, "/settings/security", Some(&cookie)).await;
    assert!(provisional.body.contains("<svg"), "QR svg rendered");
    assert!(provisional.body.contains("Setup key:"));
    let secret = between(&provisional.body, "<code>", "</code>");
    let secret_bytes =
        base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
    let csrf = csrf(&provisional.body);

    // A wrong code does not enable it.
    let wrong = post(
        &app,
        "/web/settings/two_factor/confirm",
        Some(&cookie),
        &[("csrf", &csrf), ("code", "000000")],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::SEE_OTHER);
    assert_eq!(
        wrong.location.as_deref(),
        Some("/settings/security?error=code")
    );
    assert!(
        !user::find_by_id(&pool, uid)
            .await
            .unwrap()
            .unwrap()
            .otp_required_for_login
    );

    // The right code enables 2FA and shows ten recovery codes exactly once.
    let good_code = plamenu::totp::code_for(&secret_bytes, now_epoch() / PERIOD_SECONDS);
    let confirmed = post(
        &app,
        "/web/settings/two_factor/confirm",
        Some(&cookie),
        &[("csrf", &csrf), ("code", &good_code)],
    )
    .await;
    assert_eq!(confirmed.status, StatusCode::OK);
    assert!(confirmed.body.contains("Save your recovery codes"));
    let user = user::find_by_id(&pool, uid).await.unwrap().unwrap();
    assert!(user.otp_required_for_login);
    assert_eq!(
        two_factor::backup_codes_remaining(&pool, uid)
            .await
            .unwrap(),
        i64::try_from(two_factor::BACKUP_CODE_COUNT).unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn totp_login_needs_the_code(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);

    // The password alone yields the challenge page, not a session.
    let pw = login_password(&app).await;
    assert_eq!(pw.status, StatusCode::OK);
    assert!(pw.set_cookie.is_none());
    assert!(pw.body.contains("Two-factor authentication"));

    // The code completes the sign-in.
    let cookie = login_with_totp(&app).await;
    let home = get(&app, "/", Some(&cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn totp_login_rejects_a_wrong_code(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);

    let token = hidden_value(&login_password(&app).await.body, "challenge_token");
    let bad = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &token), ("code", "000000")],
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.set_cookie.is_none());
    assert!(bad.body.contains("was incorrect"));

    // The challenge is still live, so the correct code still works.
    let ok = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &token), ("code", &code_now())],
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert!(ok.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn totp_code_cannot_be_replayed(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);
    let code = code_now();

    // First login consumes the timestep.
    let t1 = hidden_value(&login_password(&app).await.body, "challenge_token");
    let first = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &t1), ("code", &code)],
    )
    .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert!(first.set_cookie.is_some());

    // A second login with the same code is refused, even inside its window.
    let t2 = hidden_value(&login_password(&app).await.body, "challenge_token");
    let second = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &t2), ("code", &code)],
    )
    .await;
    assert_eq!(second.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(second.set_cookie.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn recovery_code_signs_in_once(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    // A recovery code that is not a 6-digit number, so it never reads as TOTP.
    let recovery = "abcd1234ef567890";
    two_factor::replace_backup_codes(&pool, uid, &[plamenu::auth::hash_secret(recovery)])
        .await
        .unwrap();
    let app = common::test_app(pool.clone());

    let t1 = hidden_value(&login_password(&app).await.body, "challenge_token");
    let first = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &t1), ("code", recovery)],
    )
    .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert!(first.set_cookie.is_some());
    assert_eq!(
        two_factor::backup_codes_remaining(&pool, uid)
            .await
            .unwrap(),
        0
    );

    // The same code cannot be used twice.
    let t2 = hidden_value(&login_password(&app).await.body, "challenge_token");
    let second = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", &t2), ("code", recovery)],
    )
    .await;
    assert_eq!(second.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(second.set_cookie.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_challenge_token_restarts_login(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let app = common::test_app(pool);

    let resp = post(
        &app,
        "/login/challenge",
        None,
        &[("challenge_token", "deadbeef"), ("code", &code_now())],
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert!(resp.set_cookie.is_none());
    assert!(resp.body.contains("expired"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn disabling_needs_the_password_and_clears_everything(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    two_factor::replace_backup_codes(&pool, uid, &[plamenu::auth::hash_secret("leftover")])
        .await
        .unwrap();
    let app = common::test_app(pool.clone());
    let cookie = login_with_totp(&app).await;
    let csrf = csrf(&get(&app, "/settings/security", Some(&cookie)).await.body);

    // The wrong password leaves 2FA on.
    let wrong = post(
        &app,
        "/web/settings/two_factor/disable",
        Some(&cookie),
        &[("csrf", &csrf), ("current_password", "nope")],
    )
    .await;
    assert_eq!(
        wrong.location.as_deref(),
        Some("/settings/security?error=current_password")
    );
    assert!(
        user::find_by_id(&pool, uid)
            .await
            .unwrap()
            .unwrap()
            .otp_required_for_login
    );

    // The right password clears the secret, requirement and recovery codes.
    let ok = post(
        &app,
        "/web/settings/two_factor/disable",
        Some(&cookie),
        &[("csrf", &csrf), ("current_password", PASSWORD)],
    )
    .await;
    assert_eq!(
        ok.location.as_deref(),
        Some("/settings/security?saved=disabled")
    );
    let user = user::find_by_id(&pool, uid).await.unwrap().unwrap();
    assert!(!user.otp_required_for_login);
    assert!(user.otp_secret.is_none());
    assert_eq!(
        two_factor::backup_codes_remaining(&pool, uid)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_authorize_completes_with_second_factor(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    let redirect_uris = vec!["https://client.example/cb".to_owned()];
    oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "TestClient",
            website: None,
            client_id: "test-client",
            client_secret_hash: &plamenu::auth::hash_secret("client-secret"),
            redirect_uris: &redirect_uris,
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let app = common::test_app(pool);

    // Chromium applies `form-action` to a redirect following a form POST.
    // The authorization and second-factor documents must therefore admit the
    // already-validated callback origin or the final redirect is blocked.
    let consent = get(
        &app,
        "/oauth/authorize?response_type=code&client_id=test-client\
         &redirect_uri=https%3A%2F%2Fclient.example%2Fcb&scope=read",
        None,
    )
    .await;
    assert_eq!(consent.status, StatusCode::OK);
    let consent_csp = consent.csp.as_deref().expect("OAuth consent CSP");
    assert!(
        consent_csp.contains("form-action 'self' https://client.example;"),
        "{consent_csp}"
    );

    // The password step yields a 2FA prompt carrying the request params.
    let step1 = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(step1.status, StatusCode::OK);
    assert!(step1.body.contains("Two-factor authentication"));
    let challenge_csp = step1.csp.as_deref().expect("OAuth challenge CSP");
    assert!(
        challenge_csp.contains("form-action 'self' https://client.example;"),
        "{challenge_csp}"
    );
    let token = hidden_value(&step1.body, "challenge_token");

    // The code mints the grant and redirects back with an authorization code.
    let step2 = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("challenge_token", &token),
            ("code", &code_now()),
        ],
    )
    .await;
    assert_eq!(step2.status, StatusCode::FOUND);
    assert!(
        step2
            .location
            .as_deref()
            .expect("redirect")
            .starts_with("https://client.example/cb?code=")
    );
    assert!(
        step2
            .set_cookies
            .iter()
            .any(|cookie| cookie.starts_with("__Host-plamenu_session=")),
        "OAuth 2FA establishes a reusable browser session"
    );
    assert!(
        step2
            .set_cookies
            .iter()
            .any(|cookie| cookie.starts_with("__Host-plamenu_accounts=")),
        "OAuth 2FA adds the account to the browser roster"
    );

    // If the client loses that callback (the mobile-popup failure that
    // motivated this behavior), restarting the request reuses the login and
    // presents a real consent decision rather than asking for password + 2FA.
    let cookies = cookie_header(&step2.set_cookies);
    let retry = get(
        &app,
        "/oauth/authorize?response_type=code&client_id=test-client\
         &redirect_uri=https%3A%2F%2Fclient.example%2Fcb&scope=read&state=retry-state",
        Some(&cookies),
    )
    .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert!(retry.body.contains("Authorize"));
    assert!(retry.body.contains("Deny"));
    assert!(retry.body.contains("@alice"));
    assert!(retry.body.contains("name=\"decision\" value=\"allow\""));

    let consent_csrf = hidden_value(&retry.body, "csrf");
    let allow = post(
        &app,
        "/oauth/authorize",
        Some(&cookies),
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("state", "retry-state"),
            ("decision", "allow"),
            ("csrf", &consent_csrf),
        ],
    )
    .await;
    assert_eq!(allow.status, StatusCode::FOUND);
    let location = allow.location.as_deref().expect("consent redirect");
    assert!(location.starts_with("https://client.example/cb?code="));
    assert!(location.ends_with("&state=retry-state"));

    let bad_csrf = post(
        &app,
        "/oauth/authorize",
        Some(&cookies),
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("decision", "allow"),
            ("csrf", "not-the-session-token"),
        ],
    )
    .await;
    assert_eq!(bad_csrf.status, StatusCode::FORBIDDEN);

    let deny = post(
        &app,
        "/oauth/authorize",
        Some(&cookies),
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("state", "deny-state"),
            ("decision", "deny"),
            ("csrf", &consent_csrf),
        ],
    )
    .await;
    assert_eq!(deny.status, StatusCode::FOUND);
    assert_eq!(
        deny.location.as_deref(),
        Some("https://client.example/cb?error=access_denied&state=deny-state")
    );
}

async fn seed_app(pool: &PgPool, client_id: &str, secret: &str, redirect: &str) {
    oauth::create_app(
        pool,
        oauth::NewApp {
            name: client_id,
            website: None,
            client_id,
            client_secret_hash: &plamenu::auth::hash_secret(secret),
            redirect_uris: &[redirect.to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
}

/// The second-factor leg mints the grant from the authorization
/// request bound to the challenge at password time. Substituting a different
/// client / callback / scope into the second POST changes nothing — the code
/// goes to the app the user actually reviewed, with the scope they approved.
#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_challenge_is_bound_to_its_authorization_request(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    enable_totp(&pool, uid).await;
    seed_app(&pool, "app-a", "secret-a", "https://a.example/cb").await;
    seed_app(&pool, "app-b", "secret-b", "https://b.example/cb").await;
    let app = common::test_app(pool);

    // Password proven for app-a, scope `read`.
    let step1 = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "app-a"),
            ("redirect_uri", "https://a.example/cb"),
            ("scope", "read"),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(step1.status, StatusCode::OK);
    let token = hidden_value(&step1.body, "challenge_token");

    // The captured challenge token is replayed inside a form rewritten for
    // app-b with a wider scope.
    let step2 = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "app-b"),
            ("redirect_uri", "https://b.example/cb"),
            ("scope", "read write"),
            ("challenge_token", &token),
            ("code", &code_now()),
        ],
    )
    .await;
    // The mint is driven by the stored request: the code lands on app-a's
    // callback, not app-b's.
    assert_eq!(step2.status, StatusCode::FOUND);
    let location = step2.location.as_deref().expect("redirect");
    assert!(
        location.starts_with("https://a.example/cb?code="),
        "grant must go to the bound app's callback, got {location}"
    );

    // And the code is exchangeable only by app-a, for the bound scope.
    let code = location
        .rsplit("code=")
        .next()
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let exchange = post(
        &app,
        "/oauth/token",
        None,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", "app-a"),
            ("client_secret", "secret-a"),
            ("redirect_uri", "https://a.example/cb"),
        ],
    )
    .await;
    assert_eq!(exchange.status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&exchange.body).unwrap();
    assert_eq!(json["scope"], "read", "the bound scope, not the form's");
}

/// RFC 7636 syntax (43–128 unreserved characters) is enforced on
/// the challenge at both authorize entry paths, and on the verifier before it
/// is hashed at the token exchange.
#[sqlx::test(migrations = "../db/migrations")]
async fn pkce_challenge_and_verifier_syntax_are_enforced(pool: PgPool) {
    seed_alice(&pool).await;
    seed_app(&pool, "app-a", "secret-a", "https://a.example/cb").await;
    let app = common::test_app(pool);

    // GET: a malformed challenge never renders a consent page.
    let get_bad = get(
        &app,
        "/oauth/authorize?response_type=code&client_id=app-a\
         &redirect_uri=https%3A%2F%2Fa.example%2Fcb\
         &code_challenge=too-short&code_challenge_method=S256",
        None,
    )
    .await;
    assert_eq!(get_bad.status, StatusCode::BAD_REQUEST);

    // POST: the same syntax check runs before the password is spent.
    let post_bad = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "app-a"),
            ("redirect_uri", "https://a.example/cb"),
            ("scope", "read"),
            ("code_challenge", "too-short"),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(post_bad.status, StatusCode::BAD_REQUEST);

    // A verifier that breaks RFC 7636 syntax is refused even when its S256
    // hash matches the stored challenge — the syntax gate runs pre-hash.
    let short_verifier = "tiny";
    let challenge = plamenu::auth::pkce_s256(short_verifier);
    let minted = post(
        &app,
        "/oauth/authorize",
        None,
        &[
            ("client_id", "app-a"),
            ("redirect_uri", "https://a.example/cb"),
            ("scope", "read"),
            ("code_challenge", &challenge),
            ("email", EMAIL),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(minted.status, StatusCode::FOUND);
    let location = minted.location.as_deref().expect("redirect");
    let code = location
        .rsplit("code=")
        .next()
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let exchange = post(
        &app,
        "/oauth/token",
        None,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", "app-a"),
            ("client_secret", "secret-a"),
            ("redirect_uri", "https://a.example/cb"),
            ("code_verifier", short_verifier),
        ],
    )
    .await;
    assert_eq!(exchange.status, StatusCode::BAD_REQUEST);
    assert!(exchange.body.contains("invalid_grant"));
}
