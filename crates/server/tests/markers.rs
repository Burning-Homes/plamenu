//! Read-state: `/api/v1/markers` and the notification read endpoints
//! (`unread_count`, `dismiss`, `clear`, single-notification show).

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, notification, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    // Drive the real OAuth machinery once per user.
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "markers",
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

/// Generic JSON API call; returns (status, body).
async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, value) = api_full(app, method, uri, bearer, body).await;
    (status, value)
}

/// Like [`api`] but also returns the `Link` header.
async fn api_full(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Value) {
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
    let link = response
        .headers()
        .get(header::LINK)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, link, value)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn markers_roundtrip_with_versions(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    // Nothing saved yet: empty map, with or without the timeline param.
    let (code, body) = api(
        app(),
        "GET",
        "/api/v1/markers?timeline[]=home&timeline[]=notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (_, body) = api(app(), "GET", "/api/v1/markers", Some(&alice_token), None).await;
    assert_eq!(body, json!({}));

    // First save: version 1, id echoed back as a string.
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({"home": {"last_read_id": "123"}})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["home"]["last_read_id"], "123");
    assert_eq!(body["home"]["version"], 1);
    assert!(body["home"]["updated_at"].is_string());

    // Re-submitting the same position changes nothing.
    let (_, body) = api(
        app(),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({"home": {"last_read_id": "123"}})),
    )
    .await;
    assert_eq!(body["home"]["version"], 1);

    // A new position bumps the version; both timelines in one request, the
    // numeric id form too.
    let (_, body) = api(
        app(),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({
            "home": {"last_read_id": 456},
            "notifications": {"last_read_id": "789"},
        })),
    )
    .await;
    assert_eq!(body["home"]["last_read_id"], "456");
    assert_eq!(body["home"]["version"], 2);
    assert_eq!(body["notifications"]["last_read_id"], "789");
    assert_eq!(body["notifications"]["version"], 1);

    // GET filters to the requested timelines.
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/markers?timeline[]=home",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body["home"]["last_read_id"], "456");
    assert!(body.get("notifications").is_none());
    // The bare (non-array) param form works too.
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/markers?timeline=notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body["notifications"]["last_read_id"], "789");

    // Unknown timeline keys are ignored on save, unknown names match nothing.
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({"bogus": {"last_read_id": "1"}})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/markers?timeline[]=bogus",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!({}));

    // Rails-style form-encoded bodies (bracketed keys) are accepted too.
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/markers")
        .header(header::AUTHORIZATION, format!("Bearer {alice_token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "home[last_read_id]=55&notifications[last_read_id]=66",
        ))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let form_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(form_body["home"]["last_read_id"], "55");
    assert_eq!(form_body["notifications"]["last_read_id"], "66");

    // Markers are per user.
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/markers?timeline[]=home",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(body, json!({}));

    // And require authentication.
    let (code, _) = api(app(), "GET", "/api/v1/markers?timeline[]=home", None, None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notification_read_state_endpoints(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    notification::create(&pool, alice.id, carol.id, "follow", None)
        .await
        .unwrap();
    notification::create(&pool, alice.id, carol.id, "favourite", None)
        .await
        .unwrap();
    notification::create(&pool, alice.id, carol.id, "reblog", None)
        .await
        .unwrap();
    let (_, items) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 3);
    let newest = items[0]["id"].as_str().unwrap().to_owned();
    let middle = items[1]["id"].as_str().unwrap().to_owned();

    // Everything is unread without a marker; the limit caps the count.
    let (code, body) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({"count": 3}));
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count?limit=2",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!({"count": 2}));

    // Reading up to the middle notification leaves one unread.
    api(
        app(),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({"notifications": {"last_read_id": middle}})),
    )
    .await;
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!({"count": 1}));

    // Single-notification show, recipient-scoped.
    let (code, body) = api(
        app(),
        "GET",
        &format!("/api/v1/notifications/{newest}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["type"], "reblog");
    assert_eq!(body["account"]["username"], "carol");
    let (code, body) = api(
        app(),
        "GET",
        &format!("/api/v1/notifications/{newest}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");

    // Dismiss removes one; a repeat (or someone else's id) is a 404.
    let (code, body) = api(
        app(),
        "POST",
        &format!("/api/v1/notifications/{newest}/dismiss"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/notifications/{newest}/dismiss"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (_, items) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(items.as_array().unwrap().len(), 2);

    // Clear removes the rest; carol's notifications are untouched.
    notification::create(&pool, carol.id, alice.id, "follow", None)
        .await
        .unwrap();
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/notifications/clear",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (_, items) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(items.as_array().unwrap().len(), 0);
    let (_, body) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!({"count": 0}));
    let (_, items) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(items.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_paginate_with_link_headers_and_min_id(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    for kind in ["follow", "favourite", "reblog"] {
        notification::create(&pool, alice.id, carol.id, kind, None)
            .await
            .unwrap();
    }

    let (code, link, items) = api_full(
        app(),
        "GET",
        "/api/v1/notifications?limit=2",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "reblog");
    let link = link.unwrap();
    let oldest_on_page = items[1]["id"].as_str().unwrap();
    let newest_on_page = items[0]["id"].as_str().unwrap();
    assert!(link.contains(&format!("max_id={oldest_on_page}>; rel=\"next\"")));
    assert!(link.contains(&format!("min_id={newest_on_page}>; rel=\"prev\"")));

    // Walking older via the next link bound.
    let (_, items) = api(
        app(),
        "GET",
        &format!("/api/v1/notifications?limit=2&max_id={oldest_on_page}"),
        Some(&alice_token),
        None,
    )
    .await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "follow");
    let oldest = items[0]["id"].as_str().unwrap().to_owned();

    // min_id fetches the page just above a known id, newest first.
    let (_, items) = api(
        app(),
        "GET",
        &format!("/api/v1/notifications?limit=2&min_id={oldest}"),
        Some(&alice_token),
        None,
    )
    .await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "reblog");
    assert_eq!(items[1]["type"], "favourite");

    // since_id returns the newest page above the bound.
    let (_, items) = api(
        app(),
        "GET",
        &format!("/api/v1/notifications?limit=1&since_id={oldest}"),
        Some(&alice_token),
        None,
    )
    .await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "reblog");
}
