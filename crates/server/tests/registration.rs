//! Self-service registration: `POST /api/v1/accounts` and the
//! `/api/v1/emails/*` companions, the confirmation/approval lifecycle with
//! its mails, webhooks and staff notifications, the `require_user!` token
//! ladder for not-yet-functional logins, the web sign-up pages, and the
//! instance entities' registration block.

mod common;

use std::sync::Arc;

use altcha::{Challenge, Payload, SolveChallengeOptions, solve_challenge};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{
    StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_smtp, test_config,
    test_state_smtp,
};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{build_router, webhooks};
use plamenu_db::instance_settings::{self, RegistrationsMode, SettingsUpdate};
use plamenu_db::{PgPool, account, email, oauth, role, user, username_block, webhook};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Sets who may self-register, keeping the other settings untouched.
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

/// An OAuth app plus a client-credentials (user-less) bearer token — what
/// `POST /api/v1/accounts` authenticates with.
async fn app_with_token(pool: &PgPool, name: &str) -> (oauth::App, String) {
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name,
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
    (app, token)
}

/// A directly-minted user-level bearer token for `app_id`.
async fn mint_user_token(pool: &PgPool, app_id: i64, user_id: i64, scopes: &str) -> String {
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app_id, Some(user_id), scopes)
        .await
        .unwrap();
    token
}

/// A functional admin login (account + user + seeded Admin role) with a
/// bearer token carrying user and admin scopes.
async fn admin_with_token(pool: &PgPool, username: &str) -> (account::Account, String) {
    let acct = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        acct.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let admin_role = role::find_by_name(pool, "Admin").await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, acct.id, Some(admin_role.id))
            .await
            .unwrap()
    );
    let (app, _) = app_with_token(pool, &format!("{username}-app")).await;
    let token = mint_user_token(pool, app.id, row.id, "read write admin:read admin:write").await;
    (acct, token)
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
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
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

/// A Russian-preferring GET.
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
    let fields = form_with_altcha(app, uri, fields).await;
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

async fn get_page_accept(app: &Router, uri: &str, accept: &str) -> Page {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap();
    send_page(app, request).await
}

async fn post_form(app: &Router, uri: &str, fields: &[(&str, &str)]) -> Page {
    let fields = form_with_altcha(app, uri, fields).await;
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    send_page(app, request).await
}

async fn form_with_altcha(
    app: &Router,
    uri: &str,
    fields: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut fields: Vec<_> = fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    if uri == "/signup" && !fields.iter().any(|(name, _)| name == "altcha") {
        fields.push(("altcha".to_owned(), altcha_payload(app).await));
    }
    fields
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

async fn web_login(app: &Router, email: &str, password: &str) -> Page {
    post_form(app, "/login", &[("email", email), ("password", password)]).await
}

/// The `admin.sign_up` notifications visible to `token`, in API order.
async fn admin_sign_ups(app: &Router, token: &str) -> Vec<Value> {
    let (status, notifications) = api(app, "GET", "/api/v1/notifications", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    notifications
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "admin.sign_up")
        .cloned()
        .collect()
}

async fn assert_account_subresources_are_published(
    app: &Router,
    account_id: i64,
    expected: StatusCode,
) {
    for suffix in ["followers", "following", "endorsements", "featured_tags"] {
        let (status, _) = api(
            app,
            "GET",
            &format!("/api/v1/accounts/{account_id}/{suffix}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, expected, "{suffix} publication boundary");
    }
    let (collections_status, _) = api(
        app,
        "GET",
        &format!("/api/v1/accounts/{account_id}/collections"),
        None,
        None,
    )
    .await;
    assert_eq!(
        collections_status, expected,
        "account collections publication boundary"
    );
}

/// Every public identity surface must cross the same registration activation
/// boundary. Admin serializers deliberately do not use this helper: staff
/// still need to review the pending row.
async fn assert_account_is_published(
    app: &Router,
    search_token: &str,
    account: &account::Account,
    published: bool,
) {
    let expected = if published {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let webfinger = get_page(
        app,
        "/.well-known/webfinger?resource=acct:vesna%40plamenu.test",
    )
    .await;
    assert_eq!(webfinger.status, expected, "WebFinger publication boundary");

    let actor = get_page_accept(
        app,
        &format!("/ap/accounts/{}", account.id),
        "application/activity+json",
    )
    .await;
    assert_eq!(actor.status, expected, "actor publication boundary");
    let profile = get_page(app, "/@vesna").await;
    assert_eq!(
        profile.status, expected,
        "HTML profile publication boundary"
    );

    let (lookup_status, _) = api(
        app,
        "GET",
        "/api/v1/accounts/lookup?acct=vesna%40plamenu.test",
        None,
        None,
    )
    .await;
    assert_eq!(
        lookup_status, expected,
        "account lookup publication boundary"
    );
    let (show_status, _) = api(
        app,
        "GET",
        &format!("/api/v1/accounts/{}", account.id),
        None,
        None,
    )
    .await;
    assert_eq!(show_status, expected, "account entity publication boundary");

    assert_account_subresources_are_published(app, account.id, expected).await;

    let (batch_status, batch) = api(
        app,
        "GET",
        &format!("/api/v1/accounts?id[]={}", account.id),
        None,
        None,
    )
    .await;
    assert_eq!(batch_status, StatusCode::OK);
    let account_id = account.id.to_string();
    assert_eq!(
        batch.as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["id"].as_str() == Some(account_id.as_str()))),
        published,
        "batch account publication boundary"
    );

    let (search_status, search) = api(
        app,
        "GET",
        "/api/v2/search?q=vesna%40plamenu.test&type=accounts",
        Some(search_token),
        None,
    )
    .await;
    assert_eq!(search_status, StatusCode::OK);
    assert_eq!(
        search["accounts"].as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["id"].as_str() == Some(account_id.as_str()))),
        published,
        "account search publication boundary"
    );

    let (directory_status, directory) =
        api(app, "GET", "/api/v1/directory?order=new", None, None).await;
    assert_eq!(directory_status, StatusCode::OK);
    assert_eq!(
        directory.as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["id"].as_str() == Some(account_id.as_str()))),
        published,
        "profile directory publication boundary"
    );
}

/// Takes every queued e-mail, consuming the queue so later assertions see
/// only mail enqueued after this call.
async fn take_emails(pool: &PgPool) -> Vec<email::EmailJob> {
    email::make_all_due(pool).await.unwrap();
    let jobs = email::claim_due(pool, 50).await.unwrap();
    for job in &jobs {
        email::complete(pool, job.id).await.unwrap();
    }
    jobs
}

/// The raw confirmation token out of a confirmation-mail body.
fn confirmation_token(mail_body: &str) -> String {
    mail_body
        .split("confirmation_token=")
        .nth(1)
        .expect("confirmation link in mail body")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

fn signup_body(username: &str, email: &str) -> Value {
    json!({
        "username": username,
        "email": email,
        "password": "correct horse battery",
        "agreement": true,
    })
}

async fn add_webhook(pool: &PgPool, url: &str, events: &[&str]) {
    let events: Vec<String> = events.iter().map(|&e| e.to_owned()).collect();
    webhook::create(
        pool,
        webhook::NewWebhook {
            url,
            events: &events,
            secret: "s3cret",
            template: None,
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_returns_token_and_queues_confirmation_mail(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    add_webhook(
        &pool,
        "https://consumer.example/hook",
        &[webhook::ACCOUNT_CREATED],
    )
    .await;
    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_smtp(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["scope"], "read write");
    assert!(body["created_at"].as_i64().is_some());
    let user_token = body["access_token"].as_str().unwrap().to_owned();

    // The user exists, is approved (open mode) but unconfirmed.
    let created = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(created.approved);
    assert!(!created.confirmed());
    assert!(!created.functional());

    // One confirmation mail with the /auth/confirmation link.
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    assert!(mails[0].subject.contains("Confirmation instructions"));
    assert!(
        mails[0]
            .body
            .contains("https://plamenu.test/auth/confirmation?confirmation_token=")
    );

    // The `account.created` webhook fired.
    assert_eq!(webhook::pending_deliveries(&pool).await.unwrap(), 1);
    assert_eq!(webhooks::run_due(&state).await, 1);
    let posts = stub.webhook_posts();
    assert_eq!(posts.len(), 1);
    let payload: Value = serde_json::from_str(&posts[0].body).unwrap();
    assert_eq!(payload["event"], "account.created");
    assert_eq!(payload["object"]["username"], "vesna");

    // The sign-up token can drive the e-mail endpoints but nothing else:
    // the require_user! ladder 403s an unconfirmed login.
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/emails/check_confirmation",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Bool(false));
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        "Your login is missing a confirmed e-mail address"
    );

    // A user-level token cannot sign up accounts.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&user_token),
        Some(signup_body("second", "second@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        "This method requires an client credentials authentication"
    );
}

/// A successful signup commits the account, its Ed25519 key material, the user,
/// and the confirmation-mail job together — the Ed25519 keys are present the
/// instant signup returns rather than waiting on a separate write or the
/// startup backfill.
#[sqlx::test(migrations = "../db/migrations")]
async fn signup_commits_account_keys_and_mail_atomically(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    let (status, _) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let created = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    // The account exists with encrypted normalized RSA+Ed25519 rows already
    // set in the same commit; no plaintext account column is involved.
    let keys = plamenu_db::actor_key::usable_for_account(&pool, created.account_id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|key| {
        key.algorithm == "ed25519"
            && key.public_key.starts_with("z6Mk")
            && key
                .encrypted_private_key
                .as_deref()
                .is_some_and(|value| value.starts_with("fsk1."))
    }));
    // Exactly one confirmation mail committed with the account.
    assert_eq!(email::pending(&pool).await.unwrap(), 1);
}

/// The signup transaction is all-or-nothing: if the user insert fails after the
/// account and its keys are written (the "raced past the upfront check" path),
/// the whole transaction rolls back and leaves no account, no keys, and a free
/// username — never a userless account shadowing the name. This
/// drives the exact transaction primitives `sign_up` composes.
#[sqlx::test(migrations = "../db/migrations")]
async fn signup_transaction_rolls_back_completely_on_late_user_failure(pool: PgPool) {
    // Occupy an e-mail so the in-transaction user insert fails with EmailTaken.
    let taken = create_local_account(&pool, "taken", "Taken").await;
    let pw = hash_password("pw").unwrap();
    user::create(&pool, taken.id, Some("dup@example.com"), &pw)
        .await
        .unwrap();
    let (app, _) = app_with_token(&pool, "signup-app").await;

    let mut tx = pool.begin().await.unwrap();
    let rsa = plamenu_ap::keys::generate_keypair().unwrap();
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let created = account::create_local(
        &mut *tx,
        account::NewLocalAccount {
            username: "raceloser",
            display_name: "",
            note: "",
            public_key_pem: &rsa.public_pem,
        },
    )
    .await
    .unwrap();
    let keyring = plamenu::crypto::FederationKeyring::from_config(&test_config()).unwrap();
    plamenu::key_store::provision_account_tx(
        &mut tx,
        &keyring,
        TEST_DOMAIN,
        &created,
        &rsa,
        &ed25519,
    )
    .await
    .unwrap();
    let result = user::create_registered_conn(
        &mut tx,
        user::NewRegisteredUser {
            account_id: created.id,
            email: Some("dup@example.com"),
            password_hash: "h",
            approved: true,
            confirmation_token_hash: None,
            locale: None,
            sign_up_ip: None,
            created_by_application_id: app.id,
            invite_request_text: None,
            invite_id: None,
            time_zone: None,
            age_verified_at: None,
        },
    )
    .await;
    assert!(
        matches!(result, Err(plamenu_db::DbError::EmailTaken)),
        "the duplicate e-mail must fail the user insert"
    );
    // `sign_up` maps that error and returns without committing; dropping the
    // transaction rolls the account and keys back with it.
    drop(tx);

    // The account never existed: username free, no orphaned keys.
    assert!(
        account::find_local_by_username(&pool, "raceloser")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_validation_errors_use_mastodon_shape(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    // Occupy a username and an e-mail address.
    let taken = create_local_account(&pool, "taken", "Taken").await;
    let hash = hash_password("pw").unwrap();
    user::create(&pool, taken.id, Some("dup@example.com"), &hash)
        .await
        .unwrap();

    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(json!({
            "username": "taken",
            "email": "dup@example.com",
            "password": "short",
            "agreement": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let message = body["error"].as_str().unwrap();
    assert!(message.starts_with("Validation failed: "), "{message}");
    let details = &body["details"];
    assert_eq!(details["agreement"][0]["error"], "ERR_ACCEPTED");
    assert_eq!(details["username"][0]["error"], "ERR_TAKEN");
    assert_eq!(details["email"][0]["error"], "ERR_TAKEN");
    assert_eq!(details["password"][0]["error"], "ERR_TOO_SHORT");

    // Malformed username and address are ERR_INVALID; blanks are ERR_BLANK.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(json!({
            "username": "bad name!",
            "email": "not-an-address",
            "password": "",
            "agreement": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let details = &body["details"];
    assert_eq!(details["username"][0]["error"], "ERR_INVALID");
    assert_eq!(details["email"][0]["error"], "ERR_INVALID");
    assert_eq!(details["password"][0]["error"], "ERR_BLANK");

    // Nothing was created and no mail was queued by the failed attempts.
    assert!(
        account::find_local_by_username(&pool, "bad name!")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(email::pending(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_respects_username_blocklist(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    username_block::create(&pool, "admin", true, false)
        .await
        .unwrap();
    username_block::create(&pool, "press", true, true)
        .await
        .unwrap();
    let app = test_app(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    // Homoglyph variants of a reserved name are refused by the blocklist.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("4dm1n", "impostor@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["details"]["username"][0]["error"], "ERR_RESERVED");
    assert!(
        account::find_local_by_username(&pool, "4dm1n")
            .await
            .unwrap()
            .is_none()
    );

    // An allow-with-approval reservation signs up but skips open mode's
    // automatic approval, landing in the admin queue.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("press", "press@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created = user::find_by_email(&pool, "press@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(!created.approved);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_refused_when_closed(pool: PgPool) {
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    // Default mode is `none`: Mastodon's NotPermittedError wording.
    let app = test_app_smtp(pool.clone());
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "This action is not allowed");
    assert!(
        user::find_by_email(&pool, "vesna@example.com")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_without_smtp_skips_confirmation(pool: PgPool) {
    // Open mode without an SMTP relay: the confirmation step is skipped
    // (nothing could deliver it), so the login is functional immediately.
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let (_, app_token) = app_with_token(&pool, "signup-app").await;
    let no_mail = test_app(pool.clone());
    let (status, body) = api(
        &no_mail,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_owned();

    // The address is stored, and the user starts confirmed + approved.
    let created = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(created.confirmed());
    assert!(created.functional());
    assert_eq!(email::pending(&pool).await.unwrap(), 0);

    // The token passes the require_user! ladder right away.
    let (status, body) = api(
        &no_mail,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["username"], "vesna");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signup_without_email_is_functional_immediately(pool: PgPool) {
    // E-mail is optional even with a mail relay configured: a sign-up without
    // an address has nothing to confirm and no mail is queued.
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(json!({
            "username": "vesna",
            "password": "correct horse battery",
            "agreement": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_owned();
    assert_eq!(email::pending(&pool).await.unwrap(), 0);

    let account = account::find_local_by_username(&pool, "vesna")
        .await
        .unwrap()
        .unwrap();
    let created = user::find_by_account_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(created.email, None);
    assert!(created.confirmed());
    assert!(created.functional());

    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["username"], "vesna");

    // check_confirmation reports the (vacuously) confirmed state.
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/emails/check_confirmation",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Bool(true));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn confirmation_link_unlocks_login_once(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_owned();
    let token = confirmation_token(&take_emails(&pool).await[0].body);

    // Unconfirmed: the web login refuses with Devise's inactive message.
    let login = web_login(&app, "vesna@example.com", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
    assert!(login.body.contains("confirm your e-mail address"));

    // The e-mailed landing confirms the address.
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert_eq!(landing.status, StatusCode::OK);
    assert!(landing.body.contains("E-mail confirmed"));
    let confirmed = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(confirmed.functional());

    // Becoming functional sent the welcome mail.
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    assert!(mails[0].subject.starts_with("Welcome to"));

    // Single use: the same link is now invalid.
    let reused = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(reused.body.contains("Invalid confirmation link"));

    // The login works and the sign-up token now resolves normally.
    let login = web_login(&app, "vesna@example.com", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert!(login.set_cookie.is_some());
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"], "vesna");
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/emails/check_confirmation",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Bool(true));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn confirmation_grants_approval_when_mode_flipped_open(pool: PgPool) {
    // Sign up while approvals are required...
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(!pending.approved);

    // ...then the server opens registrations before the link is clicked:
    // Mastodon's grant_approval_on_confirmation.
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let token = confirmation_token(&take_emails(&pool).await[0].body);
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(landing.body.contains("E-mail confirmed"));
    assert!(!landing.body.contains("awaits review"));
    let confirmed = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(confirmed.functional());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn approved_mode_holds_sign_up_until_admin_approval(pool: PgPool) {
    let (admin_account, admin_token) = admin_with_token(&pool, "admin").await;
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    add_webhook(
        &pool,
        "https://consumer.example/hook",
        &[webhook::ACCOUNT_APPROVED],
    )
    .await;
    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_smtp(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(json!({
            "username": "vesna",
            "email": "vesna@example.com",
            "password": "correct horse battery",
            "agreement": true,
            "reason": "I would like to join",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_owned();

    // Only the confirmation mail so far; staff hears after confirmation.
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    let token = confirmation_token(&mails[0].body);

    // Confirming lands on the pending-review page and mails the staff.
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(landing.body.contains("awaits review"));
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "admin@plamenu.test");
    assert!(mails[0].subject.contains("up for review"));
    assert!(mails[0].body.contains("vesna@example.com"));

    // The Mastodon-compatible `admin.sign_up` notification is already waiting
    // in the admin's client at review time — not held back until approval.
    let pending_sign_ups = admin_sign_ups(&app, &admin_token).await;
    assert_eq!(pending_sign_ups.len(), 1);
    assert_eq!(pending_sign_ups[0]["account"]["username"], "vesna");
    assert_ne!(
        pending_sign_ups[0]["account"]["id"],
        Value::String(admin_account.id.to_string())
    );

    // Still not functional: API 403 and web login refusal, in the
    // pending-approval wording.
    let (status, body) = api(
        &app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "Your login is currently pending approval");
    let login = web_login(&app, "vesna@example.com", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
    assert!(login.body.contains("awaiting approval"));

    let vesna = account::find_local_by_username(&pool, "vesna")
        .await
        .unwrap()
        .unwrap();
    // Confirmation alone must not publish the reserved actor identity or any
    // of its already-provisioned keys.
    assert_account_is_published(&app, &admin_token, &vesna, false).await;
    assert_eq!(account::count_public_local(&pool).await.unwrap(), 1);

    // Admin approval makes both the login and public identity functional and
    // runs prepare_new_user!.
    let (status, body) = api(
        &app,
        "POST",
        &format!("/api/v1/admin/accounts/{}/approve", vesna.id),
        Some(&admin_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let approved = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(approved.functional());
    assert_account_is_published(&app, &admin_token, &vesna, true).await;
    assert_eq!(account::count_public_local(&pool).await.unwrap(), 2);

    // Welcome mail to the user...
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    assert!(mails[0].subject.starts_with("Welcome to"));

    // ...still exactly one staff `admin.sign_up` — approval reuses the ping
    // raised at review time rather than notifying the same applicant twice...
    let sign_ups = admin_sign_ups(&app, &admin_token).await;
    assert_eq!(
        sign_ups.len(),
        1,
        "approval must not duplicate the sign-up notification"
    );
    assert_eq!(sign_ups[0]["account"]["username"], "vesna");

    // ...and the `account.approved` webhook.
    assert_eq!(webhooks::run_due(&state).await, 1);
    let posts = stub.webhook_posts();
    assert_eq!(posts.len(), 1);
    let payload: Value = serde_json::from_str(&posts[0].body).unwrap();
    assert_eq!(payload["event"], "account.approved");
    assert_eq!(payload["object"]["username"], "vesna");

    let login = web_login(&app, "vesna@example.com", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn approval_before_confirmation_defers_functional_side_effects(pool: PgPool) {
    let (_, admin_token) = admin_with_token(&pool, "admin").await;
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let app = test_app_smtp(pool.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = confirmation_token(&take_emails(&pool).await[0].body);

    // Approving an unconfirmed sign-up flips `approved` but must not fire
    // the welcome mail yet — the address is still unverified.
    let vesna = account::find_local_by_username(&pool, "vesna")
        .await
        .unwrap()
        .unwrap();
    let (status, _) = api(
        &app,
        "POST",
        &format!("/api/v1/admin/accounts/{}/approve", vesna.id),
        Some(&admin_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(take_emails(&pool).await.len(), 0);
    assert_account_is_published(&app, &admin_token, &vesna, false).await;

    // The side effects run when the link is clicked instead.
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(landing.body.contains("E-mail confirmed"));
    assert!(!landing.body.contains("awaits review"));
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    assert!(mails[0].subject.starts_with("Welcome to"));
    let confirmed = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(confirmed.functional());
    assert_account_is_published(&app, &admin_token, &vesna, true).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn confirmation_resend_rotates_token_and_can_fix_the_address(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let (signup_app, app_token) = app_with_token(&pool, "signup-app").await;
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "typo@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_owned();
    let first_token = confirmation_token(&take_emails(&pool).await[0].body);

    // Resend with a corrected address: new mail, new token, e-mail updated.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/emails/confirmations",
        Some(&user_token),
        Some(json!({ "email": "vesna@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].recipient, "vesna@example.com");
    let second_token = confirmation_token(&mails[0].body);
    assert_ne!(first_token, second_token);
    assert!(
        user::find_by_email(&pool, "typo@example.com")
            .await
            .unwrap()
            .is_none()
    );

    // The rotated-out token no longer confirms anything.
    let stale = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={first_token}"),
    )
    .await;
    assert!(stale.body.contains("Invalid confirmation link"));

    // Changing to an occupied address is Mastodon's 422.
    let other = create_local_account(&pool, "other", "Other").await;
    let hash = hash_password("pw").unwrap();
    user::create(&pool, other.id, Some("other@example.com"), &hash)
        .await
        .unwrap();
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/emails/confirmations",
        Some(&user_token),
        Some(json!({ "email": "other@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: E-mail address has already been taken"
    );

    // Only the app the user signed up with may drive the resend.
    let vesna = user::find_by_email(&pool, "vesna@example.com")
        .await
        .unwrap()
        .unwrap();
    let (other_app, _) = app_with_token(&pool, "other-app").await;
    assert_ne!(other_app.id, signup_app.id);
    let foreign_token = mint_user_token(&pool, other_app.id, vesna.id, "read write").await;
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/emails/confirmations",
        Some(&foreign_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("originally signed-up with")
    );

    // After confirming, the resend endpoint refuses.
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={second_token}"),
    )
    .await;
    assert!(landing.body.contains("E-mail confirmed"));
    take_emails(&pool).await; // discard the welcome mail
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/emails/confirmations",
        Some(&user_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("awaiting confirmation")
    );
    assert_eq!(email::pending(&pool).await.unwrap(), 0);
}

/// TZ slice 5 (G4): sign-up offers a time zone, so a new account is not
/// silently pinned to UTC forever. The choice survives a failed submission and
/// reaches `users.time_zone` on success.
#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_captures_a_time_zone(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());

    let form = get_page(&app, "/signup").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains(r#"name="time_zone""#), "{}", form.body);
    // The inventory reaches the form, half-hour zones included.
    assert!(form.body.contains("Europe/Berlin"), "{}", form.body);
    assert!(
        form.body.contains("(UTC+05:45) Asia/Kathmandu"),
        "{}",
        form.body
    );

    // A rejected submission keeps the chosen zone selected.
    let mismatch = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "different"),
            ("time_zone", "Europe/Berlin"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        mismatch.body.contains(r#"value="Europe/Berlin" selected"#),
        "the chosen zone should survive a re-render"
    );

    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("time_zone", "Europe/Berlin"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert!(submitted.status.is_success() || submitted.status.is_redirection());

    let stored: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM users ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("Europe/Berlin"));
}

/// A zone outside the inventory degrades to the server default rather than
/// reaching `AT TIME ZONE` as an arbitrary string.
#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_rejects_a_zone_outside_the_inventory(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("time_zone", "'; DROP TABLE users --"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert!(submitted.status.is_success() || submitted.status.is_redirection());
    let stored: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM users ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored, None,
        "an unlisted zone must fall back to the default"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_form_registers_and_confirms(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());

    let form = get_page(&app, "/signup").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains(r#"action="/signup""#));
    assert!(
        form.body
            .contains(r#"challenge="/signup/altcha/challenge""#)
    );
    assert!(form.body.contains("/assets/altcha.min.js?v=3.2.3"));

    // Mismatched password confirmation re-renders the form.
    let mismatch = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "different"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    // The signup form now shows the same catalog copy as the other password
    // forms for this rejection.
    assert!(mismatch.body.contains("The new passwords did not match."));

    // Validation failures come back as the banner, values round-tripped.
    let invalid = post_form(
        &app,
        "/signup",
        &[
            ("username", "bad name!"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        invalid
            .body
            .contains("That username contains characters that are not allowed.")
    );
    assert!(invalid.body.contains(r#"value="vesna@example.com""#));

    // A valid submission lands on the check-your-inbox page and queues the
    // confirmation mail; the link then confirms and the login works.
    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(submitted.status, StatusCode::OK);
    assert!(submitted.body.contains("Check your inbox"));
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    let token = confirmation_token(&mails[0].body);
    let landing = get_page(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(landing.body.contains("E-mail confirmed"));
    let login = web_login(&app, "vesna@example.com", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert!(login.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_requires_a_valid_single_use_altcha_proof(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());
    let fields = [
        ("username", "vesna"),
        ("email", ""),
        ("password", "correct horse battery"),
        ("password_confirmation", "correct horse battery"),
        ("agreement", "1"),
    ];

    let missing = send_page(
        &app,
        Request::builder()
            .method("POST")
            .uri("/signup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(missing.body.contains("Complete the anti-spam verification"));
    assert!(
        account::find_local_by_username(&pool, "vesna")
            .await
            .unwrap()
            .is_none()
    );

    let proof = altcha_payload(&app).await;
    let mut accepted_fields = fields.to_vec();
    accepted_fields.push(("altcha", proof.as_str()));
    let accepted = post_form(&app, "/signup", &accepted_fields).await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);

    let replay_fields = [
        ("username", "mira"),
        ("email", ""),
        ("password", "correct horse battery"),
        ("password_confirmation", "correct horse battery"),
        ("agreement", "1"),
        ("altcha", proof.as_str()),
    ];
    let replay = post_form(&app, "/signup", &replay_fields).await;
    assert_eq!(replay.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(replay.body.contains("Complete the anti-spam verification"));
    assert!(
        account::find_local_by_username(&pool, "mira")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_without_email_is_ready_immediately(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());

    // No e-mail: no confirmation to wait for, the account is ready and the
    // sign-up completes as a sign-in — a 303 home carrying the session
    // cookie, no interstitial page.
    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", ""),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(submitted.status, StatusCode::SEE_OTHER);
    assert!(submitted.set_cookie.is_some());
    assert_eq!(email::pending(&pool).await.unwrap(), 0);

    // The password works for a later, separate sign-in too.
    let login = web_login(&app, "vesna", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert!(login.set_cookie.is_some());
}

/// A cross-site form auto-submitting a sign-up must not be able to sign this
/// browser in. An open registration completes *as a sign-in* (the test above),
/// so without a same-origin ceremony an attacker's page could cross-site POST
/// credentials of its own choosing and leave the victim's browser holding a
/// session for an account the attacker also knows the password to — login CSRF
/// / session swapping, the same class closed on `/login`,
/// `/login/challenge` and `POST /oauth/authorize`.
#[sqlx::test(migrations = "../db/migrations")]
async fn cross_site_signup_cannot_sign_this_browser_in(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());

    let proof = altcha_payload(&app).await;
    let signup_from = |origin: &str| {
        let fields = [
            ("username", "mallory"),
            ("email", ""),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
            ("altcha", proof.as_str()),
        ];
        Request::builder()
            .method("POST")
            .uri("/signup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, origin)
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap()
    };

    // The attack shape: a foreign Origin. Refused, no session, and — because
    // the refusal lands before any work — no account created either.
    let attack = send_page(&app, signup_from("https://evil.example")).await;
    assert_eq!(attack.status, StatusCode::FORBIDDEN);
    assert!(
        attack.set_cookie.is_none(),
        "no session minted for a cross-origin sign-up",
    );
    assert!(
        account::find_local_by_username(&pool, "mallory")
            .await
            .unwrap()
            .is_none(),
        "a cross-origin sign-up must not create the account either",
    );

    // The identical POST from our own origin still registers and signs in —
    // the guard rejects only cross-origin requests.
    let legit = send_page(&app, signup_from("https://plamenu.test")).await;
    assert_eq!(legit.status, StatusCode::SEE_OTHER);
    assert!(
        legit.set_cookie.is_some(),
        "a same-origin sign-up still mints a session",
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_without_email_in_approved_mode_awaits_review(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let app = test_app_smtp(pool.clone());

    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", ""),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(submitted.status, StatusCode::OK);
    assert!(submitted.body.contains("awaits review by the moderators"));

    // Not approved yet: signing in is refused with the pending message.
    let login = web_login(&app, "vesna", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
    assert!(login.body.contains("awaiting approval"));

    // Admin approval makes the (already-confirmed) login functional.
    let account = account::find_local_by_username(&pool, "vesna")
        .await
        .unwrap()
        .unwrap();
    assert!(user::approve(&pool, account.id).await.unwrap());
    let login = web_login(&app, "vesna", "correct horse battery").await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert!(login.set_cookie.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_signup_shows_closed_notice_when_unavailable(pool: PgPool) {
    // Mode `none` (the default): the closed page, and POST refuses too.
    let app = test_app_smtp(pool.clone());
    let closed = get_page(&app, "/signup").await;
    assert_eq!(closed.status, StatusCode::OK);
    assert!(closed.body.contains("Registrations are closed"));
    let submitted = post_form(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert!(submitted.body.contains("Registrations are closed"));
    assert!(
        user::find_by_email(&pool, "vesna@example.com")
            .await
            .unwrap()
            .is_none()
    );

    // Open mode without SMTP still offers the form: sign-up simply skips
    // the confirmation step.
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let no_mail = test_app(pool.clone());
    let form = get_page(&no_mail, "/signup").await;
    assert!(form.body.contains("Create an account"));

    // The approved-mode form carries the reason textarea.
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let form = get_page(&app, "/signup").await;
    assert!(form.body.contains("reviewed by the moderators"));
    assert!(form.body.contains(r#"name="reason""#));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn deleted_account_reserves_username_but_frees_email(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_smtp(pool.clone(), stub);
    let app = build_router(state.clone());
    let (_, app_token) = app_with_token(&pool, "signup-app").await;

    // A signed-up-and-deleted account (Mastodon: reserve_username true,
    // reserve_email false).
    let vesna = create_local_account(&pool, "vesna", "Vesna").await;
    let hash = hash_password("pw").unwrap();
    user::create(&pool, vesna.id, Some("vesna@example.com"), &hash)
        .await
        .unwrap();
    plamenu::moderation::self_delete_account(&state, &vesna)
        .await
        .unwrap();

    // The tombstoned username cannot be registered again...
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna", "second@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["details"]["username"][0]["error"], "ERR_TAKEN");

    // ...but the e-mail address is free for a new sign-up.
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/accounts",
        Some(&app_token),
        Some(signup_body("vesna2", "vesna@example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_entities_reflect_registration_state(pool: PgPool) {
    let app = test_app_smtp(pool.clone());

    // Default: closed.
    let (_, v1) = api(&app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(v1["registrations"], Value::Bool(false));
    assert_eq!(v1["approval_required"], Value::Bool(false));
    let (_, v2) = api(&app, "GET", "/api/v2/instance", None, None).await;
    assert_eq!(v2["registrations"]["enabled"], Value::Bool(false));

    // Open with a mail relay: enabled.
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let (_, v1) = api(&app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(v1["registrations"], Value::Bool(true));
    assert_eq!(v1["approval_required"], Value::Bool(false));

    // Approved mode: enabled + approval_required.
    set_registrations_mode(&pool, RegistrationsMode::Approved).await;
    let (_, v1) = api(&app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(v1["registrations"], Value::Bool(true));
    assert_eq!(v1["approval_required"], Value::Bool(true));
    let (_, v2) = api(&app, "GET", "/api/v2/instance", None, None).await;
    assert_eq!(v2["registrations"]["enabled"], Value::Bool(true));
    assert_eq!(v2["registrations"]["approval_required"], Value::Bool(true));
    assert_eq!(v2["registrations"]["message"], Value::Null);

    // No SMTP does not close registrations: e-mail (and therefore its
    // confirmation) is optional.
    let no_mail = test_app(pool.clone());
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let (_, v1) = api(&no_mail, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(v1["registrations"], Value::Bool(true));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn russian_signup_localizes_the_pages_the_errors_and_the_mail(pool: PgPool) {
    set_registrations_mode(&pool, RegistrationsMode::Open).await;
    let app = test_app_smtp(pool.clone());

    // A validation failure is re-stated from the catalog, not from the API's
    // English `details` description.
    let invalid = post_form_in_russian(
        &app,
        "/signup",
        &[
            ("username", "bad name!"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        invalid
            .body
            .contains("Имя пользователя содержит недопустимые символы."),
        "body {}",
        invalid.body
    );

    let submitted = post_form_in_russian(
        &app,
        "/signup",
        &[
            ("username", "vesna"),
            ("email", "vesna@example.com"),
            ("password", "correct horse battery"),
            ("password_confirmation", "correct horse battery"),
            ("agreement", "1"),
        ],
    )
    .await;
    assert_eq!(submitted.status, StatusCode::OK);
    assert!(submitted.body.contains("Проверьте почту"));

    // The sign-up stored the applicant's locale, so the confirmation mail is
    // written in it — and carries no directional isolates into plain text.
    let mails = take_emails(&pool).await;
    assert_eq!(mails.len(), 1);
    assert!(
        mails[0].subject.contains("подтверждение адреса"),
        "subject {}",
        mails[0].subject
    );
    assert!(
        mails[0].body.contains("Здравствуйте, @vesna!"),
        "body {}",
        mails[0].body
    );
    assert!(!mails[0].body.contains('\u{2068}'));

    // The confirmation landing negotiates the header like the other anonymous
    // pages.
    let token = confirmation_token(&mails[0].body);
    let landing = get_page_in_russian(
        &app,
        &format!("/auth/confirmation?confirmation_token={token}"),
    )
    .await;
    assert!(landing.body.contains("lang=\"ru\""));
    assert!(landing.body.contains("Адрес подтверждён"));
}
