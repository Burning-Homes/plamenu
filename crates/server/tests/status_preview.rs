//! `POST /api/v1/statuses/preview`: server-side sanitized rendering of a
//! draft (pairs with P4 `content_type`) that persists nothing.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "preview",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    let client_id = app_response.1["client_id"].as_str().unwrap().to_owned();
    let client_secret = app_response.1["client_secret"].as_str().unwrap().to_owned();
    let auth = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", "read write"),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await;
    let code = auth
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let token = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        })),
    )
    .await;
    (
        account,
        token.1["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn post_form(app: Router, uri: &str, fields: &[(&str, &str)]) -> String {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn markdown_preview_renders_html_without_persisting(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let (status, preview) = api(
        app(),
        "POST",
        "/api/v1/statuses/preview",
        Some(&token),
        Some(json!({ "status": "**bold** and _soft_", "content_type": "text/markdown" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let content = preview["content"].as_str().unwrap();
    assert!(content.contains("<strong>bold</strong>"), "{content}");
    assert!(content.contains("<em>soft</em>"), "{content}");
    // Unsaved-record stand-ins.
    assert_eq!(preview["id"], "0");
    assert!(preview["uri"].is_null());
    assert!(preview["url"].is_null());
    assert_eq!(preview["account"]["id"], alice.id.to_string());

    // Nothing was written: the author's statuses stay empty.
    let (_, timeline) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", alice.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(timeline.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preview_matches_what_create_would_render(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());
    let body = json!({ "status": "# Heading\n\nwith `code`", "content_type": "text/markdown" });

    let (_, preview) = api(
        app(),
        "POST",
        "/api/v1/statuses/preview",
        Some(&token),
        Some(body.clone()),
    )
    .await;
    let (_, posted) = api(app(), "POST", "/api/v1/statuses", Some(&token), Some(body)).await;
    // The whole point of the endpoint: the previewed HTML is byte-identical to
    // what the real post stores, so a client never drifts from the server.
    assert_eq!(preview["content"], posted["content"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preview_resolves_known_local_mentions(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (bob, _) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    let (status, preview) = api(
        app(),
        "POST",
        "/api/v1/statuses/preview",
        Some(&token),
        Some(json!({ "status": "hi @bob!" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let mentions = preview["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0]["id"], bob.id.to_string());
    assert_eq!(mentions[0]["username"], "bob");
    assert!(
        preview["content"]
            .as_str()
            .unwrap()
            .contains("@<span>bob</span>")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preview_enforces_the_same_validation_as_create(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // Blank draft — the create endpoint's "Text can't be blank".
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/statuses/preview",
        Some(&token),
        Some(json!({ "status": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Over the character limit.
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/statuses/preview",
        Some(&token),
        Some(json!({ "status": "x".repeat(6000) })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preview_requires_authentication(pool: PgPool) {
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses/preview",
        None,
        Some(json!({ "status": "hi" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
