//! Mastodon's `/api/web/*` browser-client compatibility routes.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{TEST_DOMAIN, create_local_account, test_app};
use http_body_util::BodyExt;
use p256::elliptic_curve::sec1::ToSec1Point;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, oauth, status, user, web_push};
use serde_json::{Value, json};
use tower::ServiceExt;

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
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    ApiResponse {
        status,
        headers,
        json,
    }
}

async fn api_json(
    app: Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    cookie: Option<&str>,
    body: Option<Value>,
) -> ApiResponse {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    send(app, request).await
}

async fn create_user(pool: &PgPool, username: &str, email: &str, password: &str) -> (Account, i64) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password(password).unwrap();
    let row = user::create(pool, account.id, Some(email), &hash)
        .await
        .unwrap();
    (account, row.id)
}

async fn user_with_token(pool: &PgPool, username: &str, scopes: &str) -> (Account, i64, String) {
    let (account, user_id) = create_user(
        pool,
        username,
        &format!("{username}@example.com"),
        "correct horse battery",
    )
    .await;
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "web-api-tests",
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
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(user_id), scopes)
        .await
        .unwrap();
    (account, user_id, token)
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

async fn login_cookie(app: Router, email: &str, password: &str) -> String {
    let body = serde_urlencoded::to_string([("email", email), ("password", password)]).unwrap();
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{:?}",
        response.json
    );
    cookie_pair(
        response
            .headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("session cookie"),
    )
    .to_owned()
}

fn client_keys() -> (String, String) {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).unwrap();
    let secret = p256::SecretKey::from_slice(&bytes).unwrap();
    let mut auth = [0_u8; 16];
    getrandom::fill(&mut auth).unwrap();
    (
        URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
        URL_SAFE_NO_PAD.encode(auth),
    )
}

fn subscription_body(p256dh: &str, auth: &str) -> Value {
    json!({
        "subscription": {
            "endpoint": "https://push.example/web/abc123",
            "standard": true,
            "keys": { "p256dh": p256dh, "auth": auth },
        },
        "data": {
            "policy": "followed",
            "alerts": { "mention": true, "bogus": true },
        },
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_settings_store_raw_json_for_session_cookie(pool: PgPool) {
    let (_, user_id) =
        create_user(&pool, "alice", "alice@example.com", "correct horse battery").await;
    let cookie = login_cookie(
        test_app(pool.clone()),
        "alice@example.com",
        "correct horse battery",
    )
    .await;
    let data = json!({
        "theme": "mastodon-light",
        "columns": ["home", "notifications"],
        "boost_modal": true,
    });
    let response = api_json(
        test_app(pool.clone()),
        "PATCH",
        "/api/web/settings",
        None,
        Some(&cookie),
        Some(json!({ "data": data })),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json, json!({}));

    let stored = sqlx::query_scalar!("SELECT data FROM web_settings WHERE user_id = $1", user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, data);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_push_subscription_lifecycle_by_id(pool: PgPool) {
    let (_, user_id, token) = user_with_token(&pool, "alice", "read write push").await;
    let (p256dh, auth) = client_keys();

    let response = api_json(
        test_app(pool.clone()),
        "POST",
        "/api/web/push_subscriptions",
        Some(&token),
        None,
        Some(subscription_body(&p256dh, &auth)),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    let id = response.json["id"].as_i64().unwrap();
    assert_eq!(response.json["endpoint"], "https://push.example/web/abc123");
    assert_eq!(response.json["standard"], true);
    assert_eq!(response.json["policy"], "followed");
    assert_eq!(response.json["alerts"]["mention"], true);
    assert_eq!(response.json["alerts"]["favourite"], false);
    assert!(
        response.json["server_key"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    let stored = web_push::find_for_user_by_id(&pool, user_id, id)
        .await
        .unwrap()
        .expect("subscription row");
    assert_eq!(stored.access_token, token);

    let response = api_json(
        test_app(pool.clone()),
        "PATCH",
        &format!("/api/web/push_subscriptions/{id}"),
        Some(&token),
        None,
        Some(json!({ "data": { "policy": "all", "alerts": { "follow": "true" } } })),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["policy"], "all");
    assert_eq!(response.json["alerts"], json!({ "follow": true }));

    let response = api_json(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/web/push_subscriptions/{id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json, json!({}));
    assert!(
        web_push::find_for_user_by_id(&pool, user_id, id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_embed_returns_oembed_for_visible_local_status(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice Example").await;
    let item = status::create_local(
        &pool,
        status::NewLocalStatus::new(account.id, "<p>Hello embed</p>", "public", None),
    )
    .await
    .unwrap();

    let response = api_json(
        test_app(pool),
        "GET",
        &format!("/api/web/embeds/{}", item.id),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    assert_eq!(response.json["type"], "rich");
    assert_eq!(response.json["version"], "1.0");
    assert_eq!(response.json["author_name"], "Alice Example");
    assert_eq!(
        response.json["author_url"],
        format!("https://{TEST_DOMAIN}/@alice")
    );
    assert_eq!(response.json["provider_name"], TEST_DOMAIN);
    assert!(
        response.json["html"]
            .as_str()
            .unwrap()
            .contains(&format!("https://{TEST_DOMAIN}/@alice/{}", item.id))
    );
}
