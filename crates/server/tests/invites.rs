//! Invites: the shareable `/invite/{code}` link (JSON bootstrap +
//! browser redirect), invite-based sign-up bypassing closed/approval modes,
//! the uses counter, the `/settings/invites` management page, and the
//! instance entity's `invites_enabled`.

mod common;

use altcha::{Challenge, Payload, SolveChallengeOptions, solve_challenge};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{create_local_account, test_app_smtp};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::instance_settings::{self, RegistrationsMode, SettingsUpdate};
use plamenu_db::invite::{self, NewInvite};
use plamenu_db::{PgPool, oauth, role, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn set_registrations_mode(pool: &PgPool, mode: RegistrationsMode) {
    let s = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        SettingsUpdate {
            registrations_mode: mode,
            ..s.as_update()
        },
    )
    .await
    .unwrap();
}

/// Re-grants `invite_users` on the default "User" role — the admin-board
/// knob migration 0015 switches off, so members may not invite until an
/// admin turns it back on.
async fn enable_member_invites(pool: &PgPool) {
    let user_role = role::find_by_id(pool, role::DEFAULT_ROLE_ID)
        .await
        .unwrap()
        .unwrap();
    role::update(
        pool,
        user_role.id,
        &user_role.name,
        &user_role.color,
        user_role.position,
        user_role.permissions | role::permission::INVITE_USERS,
        user_role.highlighted,
    )
    .await
    .unwrap();
}

/// A functional local login; returns its user id.
async fn seed_user(pool: &PgPool, username: &str, password: &str) -> i64 {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password(password).unwrap();
    user::create(
        pool,
        account.id,
        Some(&format!("{username}@example.com")),
        &hash,
    )
    .await
    .unwrap()
    .id
}

/// An app-level (`client_credentials`) token for `POST /api/v1/accounts`.
async fn app_token(pool: &PgPool) -> String {
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "invite-tests",
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
    oauth::create_token(pool, &hash_secret(&token), app.id, None, "read write")
        .await
        .unwrap();
    token
}

async fn seed_invite(pool: &PgPool, user_id: i64, code: &str, max_uses: Option<i32>) {
    invite::create(
        pool,
        NewInvite {
            user_id,
            code,
            expires_in: None,
            max_uses,
            comment: "",
        },
    )
    .await
    .unwrap();
}

async fn api(
    app: &Router,
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
            .header(header::ACCEPT, "application/json")
            .body(Body::from(json.to_string())),
        None => builder
            .header(header::ACCEPT, "application/json")
            .body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

struct Page {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    body: String,
}

async fn send_page(app: &Router, request: Request<Body>) -> Page {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let header_str = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    let location = header_str(header::LOCATION);
    let set_cookie = header_str(header::SET_COOKIE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Page {
        status,
        location,
        set_cookie,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get_page(app: &Router, uri: &str, cookie: Option<&str>) -> Page {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    send_page(app, builder.body(Body::empty()).unwrap()).await
}

async fn post_form(app: &Router, uri: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Page {
    let mut fields: Vec<_> = fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    if uri == "/signup" && !fields.iter().any(|(name, _)| name == "altcha") {
        fields.push(("altcha".to_owned(), altcha_payload(app).await));
    }
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    send_page(
        app,
        builder
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap(),
    )
    .await
}

async fn altcha_payload(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/signup/altcha/challenge")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let challenge: Challenge = serde_json::from_slice(&bytes).unwrap();
    let solution = solve_challenge(SolveChallengeOptions::new(&challenge))
        .unwrap()
        .expect("test ALTCHA challenge is solvable");
    STANDARD.encode(
        serde_json::to_vec(&Payload {
            challenge,
            solution,
        })
        .unwrap(),
    )
}

fn csrf_of(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

async fn login(app: &Router, email: &str, password: &str) -> String {
    let resp = post_form(
        app,
        "/login",
        None,
        &[("email", email), ("password", password)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    resp.set_cookie
        .expect("session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn signup_body(username: &str, email: &str, invite_code: &str) -> Value {
    json!({
        "username": username,
        "email": email,
        "password": "correct horse battery",
        "agreement": true,
        "invite_code": invite_code,
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invite_link_serves_json_and_redirects_browsers(pool: PgPool) {
    let alice = seed_user(&pool, "alice", "pw").await;
    seed_invite(&pool, alice, "AbCd1234", None).await;
    seed_invite(&pool, alice, "DeadCode", Some(1)).await;
    sqlx::query("UPDATE invites SET uses = 1 WHERE code = 'DeadCode'")
        .execute(&pool)
        .await
        .unwrap();
    let app = test_app_smtp(pool.clone());

    // JSON bootstrap for apps.
    let (status, body) = api(&app, "GET", "/invite/AbCd1234", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["invite_code"], "AbCd1234");
    assert_eq!(
        body["instance_api_url"],
        "https://plamenu.test/api/v2/instance"
    );

    // Browsers land on the sign-up form with the code attached.
    let browser = get_page(&app, "/invite/AbCd1234", None).await;
    assert_eq!(browser.status, StatusCode::SEE_OTHER);
    assert_eq!(
        browser.location.as_deref(),
        Some("/signup?invite_code=AbCd1234")
    );

    // A used-up code is a 401, an unknown one a 404.
    let (status, body) = api(&app, "GET", "/invite/DeadCode", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "This invite is no longer valid");
    let (status, _) = api(&app, "GET", "/invite/NoSuch00", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invite_bypasses_closed_registrations_and_counts_uses(pool: PgPool) {
    // Registrations stay at the default `none`.
    let alice = seed_user(&pool, "alice", "pw").await;
    seed_invite(&pool, alice, "AbCd1234", Some(1)).await;
    let app = test_app_smtp(pool.clone());
    let token = app_token(&pool).await;

    // Without the invite the door is closed.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&token),
        Some(json!({
            "username": "vesna",
            "email": "vesna@example.com",
            "password": "correct horse battery",
            "agreement": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // With it the sign-up succeeds and is approved upfront.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&token),
        Some(signup_body("vesna", "vesna@example.com", "AbCd1234")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let vesna = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(vesna.approved);
    assert!(!vesna.confirmed()); // the confirmation mail still applies

    // The single use is consumed: the code is dead now.
    let (status, body) = api(&app, "GET", "/invite/AbCd1234", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, _) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&token),
        Some(signup_body("mira", "mira@example.com", "AbCd1234")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invite_grants_approval_in_approved_mode(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let alice = seed_user(&pool, "alice", "pw").await;
    seed_invite(&pool, alice, "AbCd1234", None).await;
    let app = test_app_smtp(pool.clone());
    let token = app_token(&pool).await;

    // An invited sign-up skips the review queue...
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&token),
        Some(signup_body("vesna", "vesna@example.com", "AbCd1234")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        user::find_by_email(&pool, "vesna@example.com")
            .await
            .unwrap()
            .unwrap()
            .approved
    );

    // ...while a garbage code is simply ignored (open gate via mode) and the
    // sign-up queues for approval like any other.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&token),
        Some(signup_body("mira", "mira@example.com", "nonsense")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !user::find_by_email(&pool, "mira@example.com")
            .await
            .unwrap()
            .unwrap()
            .approved
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_invite_management_and_invited_signup(pool: PgPool) {
    // Registrations closed: only the invite opens the web form.
    seed_user(&pool, "alice", "correct horse battery").await;
    enable_member_invites(&pool).await;
    let app = test_app_smtp(pool.clone());
    let cookie = login(&app, "alice@example.com", "correct horse battery").await;

    // The settings section renders, linking to the dedicated creation page,
    // which mints an invite.
    let page = get_page(&app, "/settings/invites", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/settings/invites/new"));
    assert!(page.body.contains("not created any invites yet"));
    let form_page = get_page(&app, "/settings/invites/new", Some(&cookie)).await;
    assert_eq!(form_page.status, StatusCode::OK);
    assert!(form_page.body.contains("Generate invite link"));
    let csrf = csrf_of(&form_page.body);
    let created = post_form(
        &app,
        "/web/settings/invites",
        Some(&cookie),
        &[
            ("csrf", &csrf),
            ("max_uses", "5"),
            ("expires_in", "86400"),
            ("comment", "for friends"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);

    let page = get_page(&app, "/settings/invites", Some(&cookie)).await;
    assert!(page.body.contains("/invite/"));
    assert!(page.body.contains("for friends"));
    assert!(page.body.contains("0 / 5"));
    let code = page
        .body
        .split("/invite/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();

    // Anonymous /signup is closed, but the invite link opens it.
    let closed = get_page(&app, "/signup", None).await;
    assert!(closed.body.contains("Registrations are closed"));
    let form = get_page(&app, &format!("/signup?invite_code={code}"), None).await;
    assert!(form.body.contains("Create an account"));
    assert!(form.body.contains(&format!(r#"value="{code}""#)));

    // The invited web sign-up goes through and consumes a use.
    let submitted = post_form(
        &app,
        "/signup",
        None,
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
            ("invite_code", &code),
        ],
    )
    .await;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.body);
    assert!(submitted.body.contains("Check your inbox"));
    let page = get_page(&app, "/settings/invites", Some(&cookie)).await;
    assert!(page.body.contains("1 / 5"));

    // Deactivation kills the link.
    let invite_id = invite::list_by_user(
        &pool,
        user::find_by_email(&pool, "alice@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
    )
    .await
    .unwrap()[0]
        .id;
    let expired = post_form(
        &app,
        &format!("/web/settings/invites/{invite_id}/expire"),
        Some(&cookie),
        &[("csrf", &csrf)],
    )
    .await;
    assert_eq!(expired.status, StatusCode::SEE_OTHER);
    let (status, _) = api(&app, "GET", &format!("/invite/{code}"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let reopened = get_page(&app, &format!("/signup?invite_code={code}"), None).await;
    assert!(reopened.body.contains("Registrations are closed"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_v1_advertises_invites(pool: PgPool) {
    let app = test_app_smtp(pool.clone());

    // Off by default (migration 0015): the default role may not invite.
    let (status, body) = api(&app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["invites_enabled"], Value::Bool(false));

    // Granting the permission on the default role flips the flag.
    enable_member_invites(&pool).await;
    let (status, body) = api(&app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["invites_enabled"], Value::Bool(true));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invites_forbidden_without_role_permission(pool: PgPool) {
    // No `invite_users` grant anywhere: every invite surface is closed.
    seed_user(&pool, "alice", "correct horse battery").await;
    let app = test_app_smtp(pool.clone());
    let cookie = login(&app, "alice@example.com", "correct horse battery").await;

    // The settings navigation hides the section…
    let account_page = get_page(&app, "/settings/account", Some(&cookie)).await;
    assert_eq!(account_page.status, StatusCode::OK);
    assert!(!account_page.body.contains("/settings/invites"));

    // …and the endpoints themselves refuse.
    let page = get_page(&app, "/settings/invites", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    let form_page = get_page(&app, "/settings/invites/new", Some(&cookie)).await;
    assert_eq!(form_page.status, StatusCode::FORBIDDEN);
    let csrf = csrf_of(&account_page.body);
    let created = post_form(
        &app,
        "/web/settings/invites",
        Some(&cookie),
        &[("csrf", &csrf), ("comment", "sneaky")],
    )
    .await;
    assert_eq!(created.status, StatusCode::FORBIDDEN);
    assert!(
        invite::list_by_user(
            &pool,
            user::find_by_email(&pool, "alice@example.com")
                .await
                .unwrap()
                .unwrap()
                .id,
        )
        .await
        .unwrap()
        .is_empty()
    );
}
