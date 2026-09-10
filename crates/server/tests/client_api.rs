//! Client API integration tests: app registration, the `OAuth2` PKCE flow,
//! posting, timelines, scopes and revocation — through the real router.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret, pkce_s256};
use plamenu_db::{PgPool, follow, oauth, status, user};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const REDIRECT_URI: &str = "https://app.example/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

async fn create_user(pool: &PgPool, username: &str, email: &str, password: &str) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password(password).unwrap();
    user::create(pool, account.id, Some(email), &hash)
        .await
        .unwrap();
}

struct ApiResponse {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    json: Value,
}

async fn send(app: Router, request: Request<Body>) -> ApiResponse {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    ApiResponse {
        status,
        headers,
        json,
    }
}

async fn post_json(app: Router, uri: &str, bearer: Option<&str>, body: Value) -> ApiResponse {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    send(
        app,
        builder
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
}

async fn post_form(app: Router, uri: &str, fields: &[(&str, &str)]) -> ApiResponse {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

async fn get(app: Router, uri: &str, bearer: Option<&str>) -> ApiResponse {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    send(app, builder.body(Body::empty()).unwrap()).await
}

/// Registers an app (form-encoded, the classic client behavior).
async fn register_app(pool: &PgPool) -> (String, String) {
    let response = post_form(
        test_app(pool.clone()),
        "/api/v1/apps",
        &[
            ("client_name", "test client"),
            ("redirect_uris", REDIRECT_URI),
            ("scopes", "read write"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    (
        response.json["client_id"].as_str().unwrap().to_owned(),
        response.json["client_secret"].as_str().unwrap().to_owned(),
    )
}

/// Runs the authorization-code flow with PKCE; returns the code.
async fn authorize(pool: &PgPool, client_id: &str, email: &str, password: &str) -> String {
    let response = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "read write"),
            ("state", "st&te"),
            ("code_challenge", &pkce_s256(VERIFIER)),
            ("email", email),
            ("password", password),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::FOUND, "{:?}", response.json);
    let location = response.headers[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with(REDIRECT_URI), "{location}");
    let query = location.split_once('?').unwrap().1;
    let params: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap();
    let mut code = None;
    let mut state_param = None;
    for (key, value) in params {
        match key.as_str() {
            "code" => code = Some(value),
            "state" => state_param = Some(value),
            _ => {}
        }
    }
    assert_eq!(
        state_param.as_deref(),
        Some("st&te"),
        "state must roundtrip"
    );
    code.expect("authorization code in redirect")
}

async fn exchange(
    pool: &PgPool,
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
) -> ApiResponse {
    post_json(
        test_app(pool.clone()),
        "/oauth/token",
        None,
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        }),
    )
    .await
}

/// The whole flow in one call, for tests that just need a token.
async fn user_token(pool: &PgPool, email: &str, password: &str) -> String {
    let (client_id, client_secret) = register_app(pool).await;
    let code = authorize(pool, &client_id, email, password).await;
    let response = exchange(pool, &client_id, &client_secret, &code, VERIFIER).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    response.json["access_token"].as_str().unwrap().to_owned()
}

/// Mints a bearer token for a fresh user carrying exactly `scopes`, minted
/// directly rather than through the interactive OAuth flow (exercised
/// elsewhere) so a test can pin an arbitrary granular scope string.
async fn token_with_scopes(pool: &PgPool, username: &str, scopes: &str) -> String {
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
            name: "scope-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write follow push",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(row.id), scopes)
        .await
        .unwrap();
    token
}

#[sqlx::test(migrations = "../db/migrations")]
async fn full_pkce_flow_grants_a_working_token(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "correct horse").await;
    let (client_id, client_secret) = register_app(&pool).await;

    // The consent form renders.
    let form_uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read+write&code_challenge={}&code_challenge_method=S256",
        urlencoding(REDIRECT_URI),
        pkce_s256(VERIFIER),
    );
    let response = get(test_app(pool.clone()), &form_uri, None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.json.as_str().unwrap().contains("test client"));

    let code = authorize(&pool, &client_id, "alice@plamenu.test", "correct horse").await;
    let token_response = exchange(&pool, &client_id, &client_secret, &code, VERIFIER).await;
    assert_eq!(token_response.status, StatusCode::OK);
    assert_eq!(token_response.json["token_type"], "Bearer");
    assert_eq!(token_response.json["scope"], "read write");
    let token = token_response.json["access_token"].as_str().unwrap();

    let me = get(
        test_app(pool.clone()),
        "/api/v1/accounts/verify_credentials",
        Some(token),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["username"], "alice");
    assert_eq!(me.json["acct"], "alice");
    assert!(me.json["id"].is_string(), "ids must be strings");
    assert_eq!(me.json["source"]["privacy"], "public");

    // The code was single-use.
    let replay = exchange(&pool, &client_id, &client_secret, &code, VERIFIER).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
}

fn urlencoding(s: &str) -> String {
    serde_urlencoded::to_string([("x", s)])
        .unwrap()
        .split_off(2)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn wrong_pkce_verifier_or_password_is_rejected(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "correct horse").await;
    let (client_id, client_secret) = register_app(&pool).await;

    // Wrong password never yields a code.
    let response = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("email", "alice@plamenu.test"),
            ("password", "wrong"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    // Wrong PKCE verifier kills the exchange.
    let code = authorize(&pool, &client_id, "alice@plamenu.test", "correct horse").await;
    let response = exchange(&pool, &client_id, &client_secret, &code, "not-the-verifier").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // Unregistered redirect_uri is refused before any credentials check.
    let response = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "https://evil.example/steal"),
            ("email", "alice@plamenu.test"),
            ("password", "correct horse"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // Wrong client secret cannot exchange a code.
    let code = authorize(&pool, &client_id, "alice@plamenu.test", "correct horse").await;
    let response = exchange(&pool, &client_id, "wrong-secret", &code, VERIFIER).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn client_credentials_token_has_no_user(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let (client_id, client_secret) = register_app(&pool).await;
    let response = post_json(
        test_app(pool.clone()),
        "/oauth/token",
        None,
        json!({
            "grant_type": "client_credentials",
            "client_id": client_id,
            "client_secret": client_secret,
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let token = response.json["access_token"].as_str().unwrap();

    // App-level tokens cannot act as a user.
    let me = get(
        test_app(pool.clone()),
        "/api/v1/accounts/verify_credentials",
        Some(token),
    )
    .await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn posting_and_home_timeline_work(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;

    // Post via the API.
    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "hello <clients>", "visibility": "public"}),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.json);
    assert_eq!(posted.json["content"], "<p>hello &lt;clients&gt;</p>");
    assert_eq!(posted.json["visibility"], "public");
    assert_eq!(posted.json["account"]["username"], "alice");
    let status_id = posted.json["id"].as_str().unwrap().to_owned();

    // A followed remote account's status shows up too.
    let bob = RemoteUser::new("remote.example", "bob");
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    let alice = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create_outgoing(&pool, alice.id, remote.id, "uri")
        .await
        .unwrap();
    follow::mark_accepted(&pool, alice.id, remote.id)
        .await
        .unwrap();
    status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: remote.id,
            content: "<p>from bob</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    let home = get(
        test_app(pool.clone()),
        "/api/v1/timelines/home",
        Some(&token),
    )
    .await;
    assert_eq!(home.status, StatusCode::OK);
    let items = home.json.as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Newest first; bob's status was created after alice's.
    assert_eq!(items[0]["content"], "<p>from bob</p>");
    assert_eq!(items[0]["account"]["acct"], "bob@remote.example");
    assert_eq!(items[1]["id"], status_id.as_str());

    // Keyset pagination: limit=1 yields a Link header, max_id pages past it.
    let first_page = get(
        test_app(pool.clone()),
        "/api/v1/timelines/home?limit=1",
        Some(&token),
    )
    .await;
    assert_eq!(first_page.json.as_array().unwrap().len(), 1);
    let link = first_page.headers[header::LINK].to_str().unwrap();
    assert!(link.contains("rel=\"next\""), "{link}");
    let first_id: i64 = first_page.json[0]["id"].as_str().unwrap().parse().unwrap();
    let second_page = get(
        test_app(pool.clone()),
        &format!("/api/v1/timelines/home?limit=1&max_id={first_id}"),
        Some(&token),
    )
    .await;
    assert_eq!(second_page.json[0]["id"], status_id.as_str());

    // The posted status is publicly viewable without a token.
    let shown = get(
        test_app(pool.clone()),
        &format!("/api/v1/statuses/{status_id}"),
        None,
    )
    .await;
    assert_eq!(shown.status, StatusCode::OK);
    assert_eq!(shown.json["content"], "<p>hello &lt;clients&gt;</p>");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_application_follows_mastodon_visibility(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;

    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "posted through an app", "visibility": "public"}),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.json);
    assert_eq!(
        posted.json["application"],
        json!({"name": "test client", "website": null})
    );
    let status_id = posted.json["id"].as_str().unwrap();

    let public = get(
        test_app(pool.clone()),
        &format!("/api/v1/statuses/{status_id}"),
        None,
    )
    .await;
    assert_eq!(
        public.json["application"],
        json!({"name": "test client", "website": null})
    );

    sqlx::query!(
        "UPDATE users SET show_application = FALSE WHERE email = $1",
        "alice@plamenu.test",
    )
    .execute(&pool)
    .await
    .unwrap();

    let hidden_public = get(
        test_app(pool.clone()),
        &format!("/api/v1/statuses/{status_id}"),
        None,
    )
    .await;
    assert_eq!(hidden_public.json["application"], Value::Null);

    let owner = get(
        test_app(pool.clone()),
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
    )
    .await;
    assert_eq!(
        owner.json["application"],
        json!({"name": "test client", "website": null})
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn content_warnings_round_trip_through_the_api(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;

    // A content warning is stored, forces `sensitive` on, and the language
    // is echoed back.
    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "spider pics", "spoiler_text": "arachnophobia", "language": "en"}),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.json);
    assert_eq!(posted.json["spoiler_text"], "arachnophobia");
    assert_eq!(posted.json["sensitive"], json!(true));
    assert_eq!(posted.json["language"], "en");

    // `sensitive` works without a CW, string booleans (form clients) parse
    // like Mastodon's, and omitted language falls back to the user's default.
    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "beach photos", "sensitive": "true"}),
    )
    .await;
    assert_eq!(posted.json["sensitive"], json!(true));
    assert_eq!(posted.json["spoiler_text"], "");
    assert_eq!(posted.json["language"], "en");

    // Mastodon quirk: a CW-only post becomes a plain post of the CW text,
    // but stays sensitive.
    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"spoiler_text": "actually the post"}),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.json);
    assert_eq!(posted.json["content"], "<p>actually the post</p>");
    assert_eq!(posted.json["spoiler_text"], "");
    assert_eq!(posted.json["sensitive"], json!(true));

    // Garbage language codes are rejected.
    let bad = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "x", "language": "not a language"}),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scopes_and_visibility_are_enforced(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    // A read-only app/token.
    let response = post_form(
        test_app(pool.clone()),
        "/api/v1/apps",
        &[
            ("client_name", "read only"),
            ("redirect_uris", REDIRECT_URI),
            ("scopes", "read"),
        ],
    )
    .await;
    let client_id = response.json["client_id"].as_str().unwrap().to_owned();
    let client_secret = response.json["client_secret"].as_str().unwrap().to_owned();
    let auth_response = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "read"),
            ("email", "alice@plamenu.test"),
            ("password", "pw"),
        ],
    )
    .await;
    let location = auth_response.headers[header::LOCATION].to_str().unwrap();
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let token_response = post_json(
        test_app(pool.clone()),
        "/oauth/token",
        None,
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": REDIRECT_URI,
        }),
    )
    .await;
    let token = token_response.json["access_token"]
        .as_str()
        .unwrap()
        .to_owned();

    // Reading works, writing is out of scope.
    let me = get(
        test_app(pool.clone()),
        "/api/v1/accounts/verify_credentials",
        Some(&token),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    let denied = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "should fail"}),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    // An unknown visibility is refused, not silently widened — while every
    // Mastodon level (including a mention-less direct self-DM) and Plamenu's
    // local-only extension are accepted.
    create_user(&pool, "carol", "carol@plamenu.test", "pw2").await;
    let write_token = user_token(&pool, "carol@plamenu.test", "pw2").await;
    let unsupported = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&write_token),
        json!({"status": "secret", "visibility": "limited"}),
    )
    .await;
    assert_eq!(unsupported.status, StatusCode::UNPROCESSABLE_ENTITY);
    let direct = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&write_token),
        json!({"status": "secret", "visibility": "direct"}),
    )
    .await;
    assert_eq!(direct.status, StatusCode::OK);
    assert_eq!(direct.json["visibility"], "direct");
    let local = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&write_token),
        json!({"status": "same instance", "visibility": "local"}),
    )
    .await;
    assert_eq!(local.status, StatusCode::OK);
    assert_eq!(local.json["visibility"], "local");

    // No token at all.
    let anonymous = get(test_app(pool.clone()), "/api/v1/timelines/home", None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let garbage = get(test_app(pool), "/api/v1/timelines/home", Some("garbage")).await;
    assert_eq!(garbage.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn granular_scope_does_not_grant_broad_family(pool: PgPool) {
    // A resource-specific scope used to satisfy the whole family
    // — a `read:statuses` token could read notifications, a `write:accounts`
    // token could post. The lattice now only lets a *broad* grant reach down to
    // a granular requirement, never the reverse.

    // Reading a sibling resource: broad `read` may, granular `read:statuses`
    // may not (both are "read" family, but the granular grant is confined).
    let statuses_reader = token_with_scopes(&pool, "narrow_reader", "read:statuses").await;
    let denied = get(
        test_app(pool.clone()),
        "/api/v1/notifications",
        Some(&statuses_reader),
    )
    .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "read:statuses must not read notifications: {:?}",
        denied.json
    );

    let broad_reader = token_with_scopes(&pool, "broad_reader", "read").await;
    let allowed = get(
        test_app(pool.clone()),
        "/api/v1/notifications",
        Some(&broad_reader),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "broad read reads notifications: {:?}",
        allowed.json
    );

    // Writing: broad `write` may post, granular `write:accounts` may not.
    let accounts_writer = token_with_scopes(&pool, "narrow_writer", "write:accounts").await;
    let denied = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&accounts_writer),
        json!({ "status": "should fail" }),
    )
    .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "write:accounts must not create a status: {:?}",
        denied.json
    );

    let broad_writer = token_with_scopes(&pool, "broad_writer", "write").await;
    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&broad_writer),
        json!({ "status": "hello from a broad write token" }),
    )
    .await;
    assert_eq!(
        posted.status,
        StatusCode::OK,
        "broad write creates a status: {:?}",
        posted.json
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn revoked_tokens_stop_working(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let (client_id, client_secret) = register_app(&pool).await;
    let code = authorize(&pool, &client_id, "alice@plamenu.test", "pw").await;
    let token_response = exchange(&pool, &client_id, &client_secret, &code, VERIFIER).await;
    let token = token_response.json["access_token"]
        .as_str()
        .unwrap()
        .to_owned();

    let revoke = post_json(
        test_app(pool.clone()),
        "/oauth/revoke",
        None,
        json!({"client_id": client_id, "client_secret": client_secret, "token": token}),
    )
    .await;
    assert_eq!(revoke.status, StatusCode::OK);

    let me = get(
        test_app(pool.clone()),
        "/api/v1/accounts/verify_credentials",
        Some(&token),
    )
    .await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oob_flow_shows_the_code_inline(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let response = post_form(
        test_app(pool.clone()),
        "/api/v1/apps",
        &[
            ("client_name", "cli tool"),
            ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
        ],
    )
    .await;
    let client_id = response.json["client_id"].as_str().unwrap().to_owned();

    let response = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("email", "alice@plamenu.test"),
            ("password", "pw"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response
            .json
            .as_str()
            .unwrap()
            .contains("Authorization code")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn batch_statuses_returns_visible_in_request_order(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;

    // Two of alice's own posts.
    let first = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "first", "visibility": "public"}),
    )
    .await;
    let first_id = first.json["id"].as_str().unwrap().to_owned();
    let second = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "second", "visibility": "public"}),
    )
    .await;
    let second_id = second.json["id"].as_str().unwrap().to_owned();

    // A followers-only post by someone alice does not follow: not visible.
    create_user(&pool, "bob", "bob@plamenu.test", "pw").await;
    let bob_token = user_token(&pool, "bob@plamenu.test", "pw").await;
    let hidden = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&bob_token),
        json!({"status": "secret", "visibility": "private"}),
    )
    .await;
    let hidden_id = hidden.json["id"].as_str().unwrap().to_owned();

    // Request second, then first, plus the hidden one and an unknown id.
    let uri = format!(
        "/api/v1/statuses?id[]={second_id}&id[]={first_id}&id[]={hidden_id}&id[]=999999999"
    );
    let response = get(test_app(pool.clone()), &uri, Some(&token)).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    let ids: Vec<&str> = response
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    // Request order preserved; hidden + unknown silently dropped.
    assert_eq!(ids, vec![second_id.as_str(), first_id.as_str()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn batch_statuses_over_limit_is_unprocessable(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;
    let query = (0..21)
        .map(|i| format!("id[]={i}"))
        .collect::<Vec<_>>()
        .join("&");
    let response = get(
        test_app(pool.clone()),
        &format!("/api/v1/statuses?{query}"),
        Some(&token),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A `silence` domain block marks every account on that server `limited` in
/// the Mastodon account entity, so clients can flag them — the same signal a
/// per-account silence emits.
#[sqlx::test(migrations = "../db/migrations")]
async fn domain_silence_marks_remote_account_limited(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    let uri = format!("/api/v1/accounts/{}", remote.id);

    let before = get(test_app(pool.clone()), &uri, None).await;
    assert_eq!(before.status, StatusCode::OK);
    assert!(
        before.json.get("limited").is_none(),
        "not limited before the domain block"
    );

    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "remote.example",
            severity: "silence",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();

    let after = get(test_app(pool.clone()), &uri, None).await;
    assert_eq!(after.status, StatusCode::OK);
    assert_eq!(after.json["limited"], Value::Bool(true));
}

/// The timeline-order preference: `published` (the default) sorts by post
/// date, `received` by ingest order — stored server-side, so it applies to
/// every client of the account.
#[sqlx::test(migrations = "../db/migrations")]
async fn timeline_order_setting_reorders_home(pool: PgPool) {
    create_user(&pool, "alice", "alice@plamenu.test", "pw").await;
    let token = user_token(&pool, "alice@plamenu.test", "pw").await;

    let posted = post_json(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        json!({"status": "fresh", "visibility": "public"}),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{:?}", posted.json);
    let fresh_id = posted.json["id"].as_str().unwrap().to_owned();

    // A followed remote account's *old* post arrives late (a backfill or
    // thread fetch): its snowflake id is newer than alice's post, its
    // publish date far older.
    let bob = RemoteUser::new("remote.example", "bob");
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    let alice = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create_outgoing(&pool, alice.id, remote.id, "uri")
        .await
        .unwrap();
    follow::mark_accepted(&pool, alice.id, remote.id)
        .await
        .unwrap();
    let old = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/old",
            account_id: remote.id,
            content: "<p>old</p>",
            created_at: time::macros::datetime!(2026-01-01 00:00 UTC),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    // Default (published): the old post sinks below alice's fresh one.
    let home = get(
        test_app(pool.clone()),
        "/api/v1/timelines/home",
        Some(&token),
    )
    .await;
    let ids: Vec<&str> = home
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [fresh_id.as_str(), old.id.to_string().as_str()]);

    // Pagination under published order: the max_id cursor is translated to a
    // (sort_at, id) bound, so the old post — whose snowflake id is *larger*
    // than the cursor — still shows up on the next page.
    let second_page = get(
        test_app(pool.clone()),
        &format!("/api/v1/timelines/home?limit=1&max_id={fresh_id}"),
        Some(&token),
    )
    .await;
    assert_eq!(second_page.json[0]["id"], old.id.to_string());

    // Flip the account to ingest order; every client now reads it that way.
    let user_row = user::find_by_email(&pool, "alice@plamenu.test")
        .await
        .unwrap()
        .unwrap();
    let mut settings = user::settings_by_user_id(&pool, user_row.id)
        .await
        .unwrap()
        .unwrap();
    settings.timeline_order = user::TimelineOrder::Received;
    user::update_settings(&pool, user_row.id, settings)
        .await
        .unwrap()
        .unwrap();

    let home = get(
        test_app(pool.clone()),
        "/api/v1/timelines/home",
        Some(&token),
    )
    .await;
    let ids: Vec<&str> = home
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [old.id.to_string().as_str(), fresh_id.as_str()]);
}

// ---- Public app-registration validation -------------------------------

/// Posts to `POST /api/v1/apps` with the given form fields and returns the raw
/// response, so a test can assert on rejections as well as successes.
async fn register_app_form(pool: &PgPool, fields: &[(&str, &str)]) -> ApiResponse {
    post_form(test_app(pool.clone()), "/api/v1/apps", fields).await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn app_registration_rejects_blank_name(pool: PgPool) {
    let response = register_app_form(
        &pool,
        &[("client_name", "   "), ("redirect_uris", REDIRECT_URI)],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        response.json
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn app_registration_rejects_dangerous_redirect_uris(pool: PgPool) {
    // A script-scheme callback would run in the browser if it ever reached a
    // navigation; it must never be storable.
    let script = register_app_form(
        &pool,
        &[
            ("client_name", "evil"),
            ("redirect_uris", "javascript:alert(document.cookie)"),
        ],
    )
    .await;
    assert_eq!(
        script.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        script.json
    );

    // Fragments are rejected too (Mastodon's rule).
    let fragment = register_app_form(
        &pool,
        &[
            ("client_name", "frag"),
            ("redirect_uris", "https://app.example/cb#tok"),
        ],
    )
    .await;
    assert_eq!(
        fragment.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        fragment.json
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn app_registration_rejects_non_http_website(pool: PgPool) {
    let response = register_app_form(
        &pool,
        &[
            ("client_name", "site"),
            ("redirect_uris", REDIRECT_URI),
            ("website", "javascript:alert(1)"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        response.json
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn app_registration_rejects_unknown_scope(pool: PgPool) {
    let response = register_app_form(
        &pool,
        &[
            ("client_name", "greedy"),
            ("redirect_uris", REDIRECT_URI),
            ("scopes", "read superuser"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        response.json
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn app_registration_accepts_and_normalizes_scopes(pool: PgPool) {
    // Granular scopes are accepted, and duplicates collapse (first-seen order).
    let response = register_app_form(
        &pool,
        &[
            ("client_name", "granular"),
            ("redirect_uris", REDIRECT_URI),
            ("scopes", "read read:statuses write read"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    let client_id = response.json["client_id"].as_str().unwrap();
    let app = plamenu_db::oauth::find_app_by_client_id(&pool, client_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.scopes, "read read:statuses write");
}
