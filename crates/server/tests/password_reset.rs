//! Self-service password reset: the request form and its
//! anti-enumeration behavior, the e-mailed edit link, validation, the
//! sign-out-everywhere side effect, and the no-SMTP refusal.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app, test_app_smtp};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::{PgPool, email, oauth, user};
use serde_json::Value;
use tower::ServiceExt;

struct Page {
    status: StatusCode,
    set_cookie: Option<String>,
    body: String,
}

async fn send_page(app: &Router, request: Request<Body>) -> Page {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Page {
        status,
        set_cookie,
        // Localized copy carries Fluent's directional isolates around
        // interpolated values; strip them so assertions read as the page does.
        body: String::from_utf8_lossy(&bytes).replace(['\u{2068}', '\u{2069}'], ""),
    }
}

/// A Russian-preferring GET — the reset pages are anonymous, so the header is
/// the only signal they have.
async fn get_page_in_russian(app: &Router, uri: &str) -> Page {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT_LANGUAGE, "ru")
        .body(Body::empty())
        .unwrap();
    send_page(app, request).await
}

/// A Russian-preferring form POST.
async fn post_form_in_russian(app: &Router, uri: &str, fields: &[(&str, &str)]) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT_LANGUAGE, "ru")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    send_page(app, request).await
}

async fn get_page(app: &Router, uri: &str) -> Page {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    send_page(app, request).await
}

async fn post_form(app: &Router, uri: &str, fields: &[(&str, &str)]) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    send_page(app, request).await
}

async fn web_login(app: &Router, email: &str, password: &str) -> Page {
    post_form(app, "/login", &[("email", email), ("password", password)]).await
}

/// Takes every queued e-mail, consuming the queue. The reset request enqueues
/// its mail from a background task, decoupling the response from
/// the account-dependent work), so this polls briefly for the queue to fill
/// before draining it; flows that expect *no* mail assert on the empty result
/// after the same bounded wait.
async fn take_emails(pool: &PgPool) -> Vec<email::EmailJob> {
    for _ in 0..100 {
        email::make_all_due(pool).await.unwrap();
        let jobs = email::claim_due(pool, 50).await.unwrap();
        if !jobs.is_empty() {
            for job in &jobs {
                email::complete(pool, job.id).await.unwrap();
            }
            return jobs;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Vec::new()
}

/// The raw reset token out of a reset-mail body.
fn reset_token(mail_body: &str) -> String {
    mail_body
        .split("reset_password_token=")
        .nth(1)
        .expect("reset link in mail body")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

/// A functional local login with a live API token, for asserting the
/// sign-out-everywhere effect.
async fn seed_user(pool: &PgPool, username: &str, password: &str) -> (i64, String) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password(password).unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@example.com")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "reset-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write",
    )
    .await
    .unwrap();
    (row.id, token)
}

async fn api_get(app: &Router, path: &str, token: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reset_flow_changes_password_and_signs_out_everywhere(pool: PgPool) {
    let (_, api_token) = seed_user(&pool, "vesna", "old password").await;
    let app = test_app_smtp(pool.clone());

    // The login page links the flow; the request form renders.
    let login_page = get_page(&app, "/login").await;
    assert!(login_page.body.contains("/auth/password/new"));
    let form = get_page(&app, "/auth/password/new").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains(r#"action="/auth/password""#));

    // Requesting the mail queues one message with the edit link.
    let sent = post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert!(sent.body.contains("Check your inbox"));
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    assert!(mails[0].subject.contains("Reset password instructions"));
    assert!(
        mails[0]
            .body
            .contains("https://plamenu.test/auth/password/edit?reset_password_token=")
    );
    let token = reset_token(&mails[0].body);

    // The link lands on the new-password form.
    let edit = get_page(
        &app,
        &format!("/auth/password/edit?reset_password_token={token}"),
    )
    .await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.body.contains("Choose a new password"));

    // Mismatch and too-short passwords re-render with the error.
    let mismatch = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "new password!"),
            ("password_confirmation", "different!"),
        ],
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    // The reset form now shows the same catalog copy as the signed-in change
    // form for the same rejection.
    assert!(mismatch.body.contains("The new passwords did not match."));
    let short = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "short"),
            ("password_confirmation", "short"),
        ],
    )
    .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(short.body.contains("too short"));

    // A valid submission changes the password...
    let done = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "new password!"),
            ("password_confirmation", "new password!"),
        ],
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.body.contains("Password changed"));

    // ...invalidates the old one, works with the new one...
    let old = web_login(&app, "vesna@example.com", "old password").await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);
    let new = web_login(&app, "vesna@example.com", "new password!").await;
    assert_eq!(new.status, StatusCode::SEE_OTHER);
    assert!(new.set_cookie.is_some());

    // ...and revoked the pre-existing API token.
    let (status, body) = api_get(&app, "/api/v1/accounts/verify_credentials", &api_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Single use: the link is dead now.
    let reused = get_page(
        &app,
        &format!("/auth/password/edit?reset_password_token={token}"),
    )
    .await;
    assert!(reused.body.contains("Invalid reset link"));
    let resubmit = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "sneaky password"),
            ("password_confirmation", "sneaky password"),
        ],
    )
    .await;
    assert!(resubmit.body.contains("Invalid reset link"));
}

/// The anonymous reset-completion POST validates the token before
/// spending an Argon2 hash, so an invented token cannot turn the route into a
/// hashing oracle. A live token still hashes exactly once.
#[sqlx::test(migrations = "../db/migrations")]
async fn reset_completion_validates_the_token_before_hashing(pool: PgPool) {
    seed_user(&pool, "vesna", "old password").await;
    let app = test_app_smtp(pool.clone());

    // A garbage (but non-empty) token is refused without any Argon2 work.
    let before = plamenu::auth::password_hash_count();
    let bogus = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", "not-a-real-token"),
            ("password", "a brand new secret"),
            ("password_confirmation", "a brand new secret"),
        ],
    )
    .await;
    assert!(bogus.body.contains("Invalid reset link"));
    assert_eq!(
        plamenu::auth::password_hash_count(),
        before,
        "a bogus reset token must not spend an Argon2 hash"
    );

    // A real token from the reset mail hashes exactly once and changes the
    // password.
    post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    let mails = take_emails(&pool).await;
    let token = reset_token(&mails[0].body);
    let before = plamenu::auth::password_hash_count();
    let done = post_form(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "a brand new secret"),
            ("password_confirmation", "a brand new secret"),
        ],
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.body.contains("Password changed"));
    assert_eq!(
        plamenu::auth::password_hash_count(),
        before + 1,
        "a valid reset hashes the new password exactly once"
    );
    let login = web_login(&app, "vesna@example.com", "a brand new secret").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reset_request_does_not_leak_account_existence(pool: PgPool) {
    seed_user(&pool, "vesna", "old password").await;
    let app = test_app_smtp(pool.clone());

    // Unknown address: the same check-your-inbox page, but no mail.
    let unknown = post_form(&app, "/auth/password", &[("email", "nobody@example.com")]).await;
    assert_eq!(unknown.status, StatusCode::OK);
    assert!(unknown.body.contains("Check your inbox"));
    assert_eq!(email::pending(&pool).await.unwrap(), 0);

    // Known address: identical page, one mail — enqueued from the background
    // task the response deliberately does not wait for.
    let known = post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    assert!(known.body.contains("Check your inbox"));
    for _ in 0..100 {
        if email::pending(&pool).await.unwrap() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(email::pending(&pool).await.unwrap(), 1);

    // A second request rotates the token: only the newest link works.
    let first = reset_token(&take_emails(&pool).await[0].body);
    post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    let second = reset_token(&take_emails(&pool).await[0].body);
    assert_ne!(first, second);
    let stale = get_page(
        &app,
        &format!("/auth/password/edit?reset_password_token={first}"),
    )
    .await;
    assert!(stale.body.contains("Invalid reset link"));
    let live = get_page(
        &app,
        &format!("/auth/password/edit?reset_password_token={second}"),
    )
    .await;
    assert!(live.body.contains("Choose a new password"));

    // Garbage or missing tokens land on the invalid page.
    let garbage = get_page(&app, "/auth/password/edit?reset_password_token=nope").await;
    assert!(garbage.body.contains("Invalid reset link"));
    let missing = get_page(&app, "/auth/password/edit").await;
    assert!(missing.body.contains("Invalid reset link"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reset_unavailable_without_smtp(pool: PgPool) {
    seed_user(&pool, "vesna", "old password").await;
    let app = test_app(pool.clone());

    // No "Forgot your password?" link on the login page.
    let login_page = get_page(&app, "/login").await;
    assert!(!login_page.body.contains("/auth/password/new"));

    // Both the form and the submission answer with the notice; no mail.
    let form = get_page(&app, "/auth/password/new").await;
    assert!(form.body.contains("Password reset unavailable"));
    let submitted = post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    assert!(submitted.body.contains("Password reset unavailable"));
    assert_eq!(email::pending(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reset_pages_negotiate_russian_and_the_mail_follows_the_recipient(pool: PgPool) {
    let (user_id, _) = seed_user(&pool, "vesna", "old password").await;
    let app = test_app_smtp(pool.clone());

    // The anonymous pages negotiate the header.
    let form = get_page_in_russian(&app, "/auth/password/new").await;
    assert!(form.body.contains("lang=\"ru\""));
    assert!(form.body.contains("Сброс пароля"));
    assert!(form.body.contains("Отправить ссылку"));

    let sent =
        post_form_in_russian(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    assert!(sent.body.contains("Проверьте почту"));
    assert!(sent.body.contains("vesna@example.com"));

    // The mail, though, is read outside any request: an account with no stored
    // preference gets English even when the request asked for Russian.
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert!(mails[0].subject.contains("Reset password instructions"));
    assert!(mails[0].body.contains("Hello,"));

    // With a stored interface locale, the same mail is written in it.
    user::update_locale(&pool, user_id, Some("ru"))
        .await
        .unwrap();
    let sent = post_form(&app, "/auth/password", &[("email", "vesna@example.com")]).await;
    assert_eq!(sent.status, StatusCode::OK);
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert!(
        mails[0].subject.contains("инструкции по сбросу пароля"),
        "subject {}",
        mails[0].subject
    );
    assert!(
        mails[0].body.contains("Здравствуйте!"),
        "body {}",
        mails[0].body
    );
    // No stray directional isolates in plain text.
    assert!(!mails[0].body.contains('\u{2068}'));

    // The link still lands on a Russian form, and its validation is localized.
    let token = reset_token(&mails[0].body);
    let edit = get_page_in_russian(
        &app,
        &format!("/auth/password/edit?reset_password_token={token}"),
    )
    .await;
    assert!(edit.body.contains("Выберите новый пароль"));
    let mismatch = post_form_in_russian(
        &app,
        "/auth/password/edit",
        &[
            ("reset_password_token", token.as_str()),
            ("password", "new password!"),
            ("password_confirmation", "different!"),
        ],
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        mismatch.body.contains("Новые пароли не совпадают."),
        "body {}",
        mismatch.body
    );
}
