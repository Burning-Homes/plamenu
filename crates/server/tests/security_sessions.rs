//! Integration tests for the security page's account-access surfaces:
//! the active-sessions list, the authorized-applications list + revoke, and the
//! authentication-history log fed by successful and failed sign-ins.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu_db::{PgPool, oauth, user};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";
const FIREFOX_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0";

struct Resp {
    status: StatusCode,
    set_cookie: Option<String>,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    // Fluent brackets every interpolated `{ $var }` in bidi isolation marks
    // (U+2068 FSI / U+2069 PDI) — invisible control characters these tests
    // never assert on, so strip them and match the text a reader sees.
    let body = String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace(['\u{2068}', '\u{2069}'], "");
    Resp {
        status,
        set_cookie,
        body,
    }
}

async fn get(app: &Router, uri: &str, cookie: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post(app: &Router, uri: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Resp {
    post_ua(app, uri, cookie, fields, None).await
}

async fn post_ua(
    app: &Router,
    uri: &str,
    cookie: Option<&str>,
    fields: &[(&str, &str)],
    user_agent: Option<&str>,
) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if let Some(ua) = user_agent {
        request = request.header(header::USER_AGENT, ua);
    }
    send(app, request.body(Body::from(body)).unwrap()).await
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

fn csrf(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body.find(marker).expect("csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

async fn seed_alice(pool: &PgPool) -> i64 {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(EMAIL), &hash)
        .await
        .unwrap()
        .id
}

/// Signs in as alice with a Firefox User-Agent and returns the session cookie.
async fn login(app: &Router) -> String {
    let resp = post_ua(
        app,
        "/login",
        None,
        &[("email", EMAIL), ("password", PASSWORD)],
        Some(FIREFOX_UA),
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    cookie_pair(&resp.set_cookie.expect("session cookie")).to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn active_sessions_lists_the_current_browser(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let page = get(&app, "/settings/security", &cookie).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Active sessions"));
    // The signed-in browser is parsed from its User-Agent and flagged current.
    assert!(page.body.contains("Firefox on Linux"), "browser parsed");
    assert!(page.body.contains("This device"), "current session marked");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_second_session_can_be_revoked(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // A second, independent browser session (a fresh login).
    let other = login(&app).await;
    let other_token = oauth::find_active_token(&pool, &token_hash_of(&other))
        .await
        .unwrap()
        .expect("second session token");

    // The current session sees two sessions and can revoke the other one.
    let page = get(&app, "/settings/security", &cookie).await;
    let revoke_path = format!("/web/settings/security/sessions/{}/revoke", other_token.id);
    assert!(
        page.body.contains(&revoke_path),
        "revoke button for other session"
    );

    let revoked = post(
        &app,
        &revoke_path,
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);

    // The revoked token no longer resolves; the current session still works.
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&other))
            .await
            .unwrap()
            .is_none(),
        "other session revoked"
    );
    assert_eq!(
        get(&app, "/settings/security", &cookie).await.status,
        StatusCode::OK
    );
    let _ = uid;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn logout_revokes_the_active_token_server_side(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // The session token authenticates before logout.
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&cookie))
            .await
            .unwrap()
            .is_some(),
        "session token is live before logout"
    );

    let page = get(&app, "/settings/security", &cookie).await;
    let out = post(
        &app,
        "/logout",
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);

    // Logout must revoke the token server-side, not merely drop the browser
    // cookie — otherwise a copied token stays usable.
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&cookie))
            .await
            .unwrap()
            .is_none(),
        "logout revokes the active token server-side"
    );
}

/// Logout is a state-changing form like any other signed-in mutation: a forged
/// POST that cannot know the session-bound token must neither revoke the token
/// nor clear the cookie, on top of the `SameSite=Lax` protection.
#[sqlx::test(migrations = "../db/migrations")]
async fn logout_requires_the_session_csrf_token(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let out = post(&app, "/logout", Some(&cookie), &[("csrf", "not-it")]).await;
    assert_eq!(out.status, StatusCode::FORBIDDEN);
    assert!(
        out.set_cookie.is_none(),
        "a forged logout must not clear any cookie: {:?}",
        out.set_cookie
    );
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&cookie))
            .await
            .unwrap()
            .is_some(),
        "a forged logout must not revoke the session token"
    );
}

/// When the server-side revocation cannot be confirmed (database
/// unreachable), logout refuses — `503`, no cookie cleared — so the browser
/// keeps the token and a simple resubmit retries the revocation, instead of
/// looking signed out while the credential stays live server-side.
#[sqlx::test(migrations = "../db/migrations")]
async fn logout_fails_closed_when_revocation_cannot_be_confirmed(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/settings/security", &cookie).await;

    pool.close().await;
    let out = post(
        &app,
        "/logout",
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(out.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        out.set_cookie.is_none(),
        "no cookie is cleared while the token remains live: {:?}",
        out.set_cookie
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn changing_the_password_signs_out_everywhere_and_rotates_this_session(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // A second, independent browser session and a third-party app token, both
    // for the same account.
    let other = login(&app).await;
    let third = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "Birdwatch",
            website: Some("https://birdwatch.example"),
            client_id: "birdwatch",
            client_secret_hash: "h",
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    oauth::create_token(&pool, "thirdtokenhash", third.id, Some(uid), "read write")
        .await
        .unwrap();

    let hashes = || {
        [
            token_hash_of(&cookie),
            token_hash_of(&other),
            "thirdtokenhash".to_owned(),
        ]
    };
    for hash in hashes() {
        assert!(
            oauth::find_active_token(&pool, &hash)
                .await
                .unwrap()
                .is_some(),
            "token live before the change"
        );
    }

    let page = get(&app, "/settings/security", &cookie).await;
    let changed = post(
        &app,
        "/web/settings/security/password",
        Some(&cookie),
        &[
            ("csrf", &csrf(&page.body)),
            ("current_password", PASSWORD),
            ("new_password", "a brand new secret"),
            ("confirm_password", "a brand new secret"),
        ],
    )
    .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER);

    // Every prior token — this browser's, the other session's, and the
    // third-party app's — is now revoked: a password change signs
    // the account out everywhere, so a copied token stops working.
    for hash in hashes() {
        assert!(
            oauth::find_active_token(&pool, &hash)
                .await
                .unwrap()
                .is_none(),
            "token revoked by the password change"
        );
    }

    // The change rotated this browser onto a fresh token, so the device it was
    // made from stays signed in — with a *different* credential than before.
    let rotated = cookie_pair(&changed.set_cookie.expect("rotated session cookie")).to_owned();
    assert_ne!(rotated, cookie, "the session token was rotated, not reused");
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&rotated))
            .await
            .unwrap()
            .is_some(),
        "the rotated token is live"
    );
    assert_eq!(
        get(&app, "/settings/security", &rotated).await.status,
        StatusCode::OK,
        "the rotated cookie authenticates"
    );

    // The new password works and the old one no longer does.
    let stored = user::find_by_email(&pool, EMAIL).await.unwrap().unwrap();
    assert!(plamenu::auth::verify_password(
        "a brand new secret",
        &stored.password_hash
    ));
    assert!(!plamenu::auth::verify_password(
        PASSWORD,
        &stored.password_hash
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn password_change_enforces_the_shared_length_policy(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // A too-short new password is rejected by the shared policy
    // *before* anything is mutated or revoked.
    let page = get(&app, "/settings/security", &cookie).await;
    let rejected = post(
        &app,
        "/web/settings/security/password",
        Some(&cookie),
        &[
            ("csrf", &csrf(&page.body)),
            ("current_password", PASSWORD),
            ("new_password", "short"),
            ("confirm_password", "short"),
        ],
    )
    .await;
    assert_eq!(rejected.status, StatusCode::SEE_OTHER);

    // The current session is untouched and the password is unchanged.
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&cookie))
            .await
            .unwrap()
            .is_some(),
        "a rejected change revokes nothing"
    );
    let stored = user::find_by_email(&pool, EMAIL).await.unwrap().unwrap();
    assert!(plamenu::auth::verify_password(
        PASSWORD,
        &stored.password_hash
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn current_session_cannot_be_revoked_here(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    let token = oauth::find_active_token(&pool, &token_hash_of(&cookie))
        .await
        .unwrap()
        .unwrap();
    let page = get(&app, "/settings/security", &cookie).await;
    // The current row has no revoke button; posting its id anyway is a no-op.
    let path = format!("/web/settings/security/sessions/{}/revoke", token.id);
    assert!(
        !page.body.contains(&path),
        "no revoke button for current session"
    );
    let resp = post(&app, &path, Some(&cookie), &[("csrf", &csrf(&page.body))]).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(
        oauth::find_active_token(&pool, &token_hash_of(&cookie))
            .await
            .unwrap()
            .is_some(),
        "current session survives"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authorized_apps_list_and_revoke(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;

    // A third-party app with a live token for alice.
    let third = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "Birdwatch",
            website: Some("https://birdwatch.example"),
            client_id: "birdwatch",
            client_secret_hash: "h",
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    oauth::create_token(&pool, "thirdtokenhash", third.id, Some(uid), "read write")
        .await
        .unwrap();

    let page = get(&app, "/settings/security", &cookie).await;
    assert!(page.body.contains("Authorized applications"));
    assert!(page.body.contains("Birdwatch"), "third-party app listed");
    assert!(
        page.body.contains("https://birdwatch.example"),
        "app website linked"
    );
    // The first-party web app is never listed as a revocable authorization.
    assert!(
        !page.body.contains("Plamenu</a>"),
        "web app not listed as authorization"
    );

    let revoke_path = format!("/web/settings/security/apps/{}/revoke", third.id);
    assert!(page.body.contains(&revoke_path));
    let revoked = post(
        &app,
        &revoke_path,
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);

    // The token is gone and the app drops off the list.
    assert!(
        oauth::find_active_token(&pool, "thirdtokenhash")
            .await
            .unwrap()
            .is_none()
    );
    let after = get(&app, "/settings/security", &cookie).await;
    assert!(
        !after.body.contains("Birdwatch"),
        "app removed after revoke"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authentication_history_records_success_and_failure(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());

    // One failed attempt, then a successful login.
    let bad = post_ua(
        &app,
        "/login",
        None,
        &[("email", EMAIL), ("password", "wrong")],
        Some(FIREFOX_UA),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    let cookie = login(&app).await;

    let page = get(&app, "/settings/security", &cookie).await;
    assert!(page.body.contains("Authentication history"));
    assert!(page.body.contains("Success"), "successful sign-in logged");
    assert!(page.body.contains("Failed"), "failed sign-in logged");
    assert!(page.body.contains("Password"), "method labelled");
}

/// A login for an identifier that has no account must be indistinguishable from
/// a wrong password for a real one — not only in status/body but in the
/// expensive Argon2 work it performs, or its latency becomes an account
/// (e-mail) enumeration oracle.
#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_identifier_login_still_spends_a_password_verification(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());

    // Baseline: a wrong password for the real account is rejected and runs Argon2.
    let before_real = plamenu::auth::password_verify_count();
    let wrong = post_ua(
        &app,
        "/login",
        None,
        &[
            ("email", EMAIL),
            ("password", "definitely not the password"),
        ],
        Some(FIREFOX_UA),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(
        plamenu::auth::password_verify_count() > before_real,
        "a real account's wrong password runs Argon2",
    );

    // The unknown identifier: same rejection, and it must ALSO run Argon2 rather
    // than short-circuiting after the cheap lookup.
    let before_missing = plamenu::auth::password_verify_count();
    let missing = post_ua(
        &app,
        "/login",
        None,
        &[
            ("email", "nobody-here@example.com"),
            ("password", "definitely not the password"),
        ],
        Some(FIREFOX_UA),
    )
    .await;
    assert_eq!(
        missing.status, wrong.status,
        "missing identifier rejected exactly like a real wrong password",
    );
    assert!(
        plamenu::auth::password_verify_count() > before_missing,
        "an unknown identifier still spends an Argon2 verification (no timing oracle)",
    );
}

/// A cross-site form auto-submitting valid credentials must not be able to sign
/// this browser in — login CSRF / session swapping. The sign-in
/// POST is refused unless it demonstrably came from our own origin, while the
/// identical same-origin submit still works.
#[sqlx::test(migrations = "../db/migrations")]
async fn cross_site_login_form_cannot_change_the_active_session(pool: PgPool) {
    seed_alice(&pool).await;
    let app = common::test_app(pool.clone());

    let form = serde_urlencoded::to_string([("email", EMAIL), ("password", PASSWORD)]).unwrap();
    let login_from = |origin: &str, body: String| {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, origin)
            .body(Body::from(body))
            .unwrap()
    };

    // A foreign Origin — the shape a cross-site attack takes — is rejected with
    // no session cookie, even though the credentials are valid.
    let attack = send(&app, login_from("https://evil.example", form.clone())).await;
    assert_eq!(attack.status, StatusCode::FORBIDDEN);
    assert!(
        attack.set_cookie.is_none(),
        "no session minted for a cross-origin login",
    );

    // The identical POST from our own origin still signs in — the guard rejects
    // only cross-origin requests.
    let own_origin = format!("https://{}", common::TEST_DOMAIN);
    let legit = send(&app, login_from(&own_origin, form)).await;
    assert_eq!(legit.status, StatusCode::SEE_OTHER);
    assert!(
        legit.set_cookie.is_some(),
        "a same-origin login still mints a session",
    );
}

/// The SHA-256 hash a raw session token is stored under — the cookie carries
/// `name=<raw>`, and tokens live in the table as their hash.
fn token_hash_of(cookie_pair: &str) -> String {
    let raw = cookie_pair.split_once('=').unwrap().1;
    plamenu::auth::hash_secret(raw)
}

/// Ages a token's `created_at` back past the 90-day web-session window — a
/// runtime UPDATE, so it needs no offline `SQLx` metadata.
async fn expire_token(pool: &PgPool, token_hash: &str) {
    sqlx::query(
        "UPDATE oauth_tokens SET created_at = now() - interval '91 days' WHERE token_hash = $1",
    )
    .bind(token_hash)
    .execute(pool)
    .await
    .unwrap();
}

/// `GET /api/v1/accounts/verify_credentials` with a raw bearer token, returning
/// the status. Shares the `user_for_token` chokepoint with the cookie path.
async fn bearer_verify(app: &Router, raw_token: &str) -> StatusCode {
    let request = Request::builder()
        .uri("/api/v1/accounts/verify_credentials")
        .header(header::AUTHORIZATION, format!("Bearer {raw_token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

/// The session cookie advertises a 90-day lifetime; the backing token must
/// enforce it server-side, or a copied/leaked session stays usable forever.
/// A first-party web token aged past the window stops
/// authenticating on both the cookie and the raw-bearer paths, while a
/// third-party API token of the same age keeps working — Mastodon's own tokens
/// do not expire by design.
#[sqlx::test(migrations = "../db/migrations")]
async fn web_session_expires_after_the_advertised_lifetime(pool: PgPool) {
    let uid = seed_alice(&pool).await;
    let app = common::test_app(pool.clone());
    let cookie = login(&app).await;
    let raw = cookie.split_once('=').unwrap().1.to_owned();

    // Fresh, the session authenticates on both the browser and the API surface.
    assert_eq!(
        get(&app, "/settings/security", &cookie).await.status,
        StatusCode::OK,
        "a fresh session authenticates the browser",
    );
    assert_eq!(
        bearer_verify(&app, &raw).await,
        StatusCode::OK,
        "a fresh session authenticates the API",
    );

    // Age the web token just past the 90-day window.
    expire_token(&pool, &token_hash_of(&cookie)).await;

    // Both auth paths now reject it, exactly like a revoked token: the browser
    // is bounced to sign-in and the raw bearer is a 401.
    assert_eq!(
        get(&app, "/settings/security", &cookie).await.status,
        StatusCode::SEE_OTHER,
        "an aged session cookie no longer authenticates the browser",
    );
    assert_eq!(
        bearer_verify(&app, &raw).await,
        StatusCode::UNAUTHORIZED,
        "an aged session token no longer authenticates the API",
    );

    // A third-party token of the same age is untouched — only first-party web
    // sessions carry the server-side lifetime.
    let third = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "Birdwatch",
            website: None,
            client_id: "birdwatch",
            client_secret_hash: "h",
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let third_raw = "third-party-raw-token";
    oauth::create_token(
        &pool,
        &plamenu::auth::hash_secret(third_raw),
        third.id,
        Some(uid),
        "read write",
    )
    .await
    .unwrap();
    expire_token(&pool, &plamenu::auth::hash_secret(third_raw)).await;
    assert_eq!(
        bearer_verify(&app, third_raw).await,
        StatusCode::OK,
        "a same-age third-party token still authenticates",
    );
}
