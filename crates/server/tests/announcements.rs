//! Announcements API: listing published announcements with per-user `read`
//! state, reacting/unreacting (with the distinct-emoji limit and scope gate),
//! and dismissing.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::announcement::{self, NewAnnouncement};
use plamenu_db::{PgPool, user};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Percent-encodes every non-alphanumeric byte so a Unicode emoji can ride in
/// the request path.
fn enc(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            write!(out, "%{b:02X}").unwrap();
        }
    }
    out
}

async fn user_with_token(pool: &PgPool, username: &str, scopes: &str) -> (Account, String) {
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
            "client_name": "announcements",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": scopes,
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
            ("scope", scopes),
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn index_lists_only_published_with_read_state(pool: PgPool) {
    let (_account, token) = user_with_token(&pool, "viewer", "read write").await;

    let live = announcement::create(
        &pool,
        NewAnnouncement {
            text: "Welcome to the instance!",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // A future-scheduled (unpublished) announcement must not surface.
    announcement::create(
        &pool,
        NewAnnouncement {
            text: "Coming soon",
            scheduled_at: Some(time::OffsetDateTime::now_utc() + time::Duration::hours(1)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/announcements",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1, "only the published announcement is listed");
    assert_eq!(list[0]["id"], live.id.to_string());
    assert!(list[0]["content"].as_str().unwrap().contains("Welcome"));
    assert_eq!(list[0]["read"], json!(false));
    assert_eq!(list[0]["reactions"].as_array().unwrap().len(), 0);

    // Dismissing flips `read`.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/announcements/{}/dismiss", live.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/announcements",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap()[0]["read"], json!(true));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn react_then_unreact(pool: PgPool) {
    let (_account, token) = user_with_token(&pool, "fan", "read write").await;
    let ann = announcement::create(
        &pool,
        NewAnnouncement {
            text: "React to me",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = format!("/api/v1/announcements/{}/reactions/{}", ann.id, enc("🎉"));

    let (status, _) = api(test_app(pool.clone()), "PUT", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/announcements",
        Some(&token),
        None,
    )
    .await;
    let reactions = body.as_array().unwrap()[0]["reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["name"], json!("🎉"));
    assert_eq!(reactions[0]["count"], json!(1));
    assert_eq!(reactions[0]["me"], json!(true));

    // Removing it returns 200; removing again is a 404 (no such reaction).
    let (status, _) = api(test_app(pool.clone()), "DELETE", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(test_app(pool.clone()), "DELETE", &path, Some(&token), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/announcements",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        body.as_array().unwrap()[0]["reactions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn react_enforces_scope_and_existence(pool: PgPool) {
    let (_account, read_token) = user_with_token(&pool, "lurker", "read").await;
    let ann = announcement::create(
        &pool,
        NewAnnouncement {
            text: "x",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = format!("/api/v1/announcements/{}/reactions/{}", ann.id, enc("👍"));

    // A read-only token can't react.
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &path,
        Some(&read_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An unknown announcement is a 404 even with a write token.
    let (_account, write_token) = user_with_token(&pool, "writer", "read write").await;
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/announcements/999999/reactions/{}", enc("👍")),
        Some(&write_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn react_rejects_a_ninth_distinct_emoji(pool: PgPool) {
    let (account, token) = user_with_token(&pool, "reactor", "read write").await;
    let ann = announcement::create(
        &pool,
        NewAnnouncement {
            text: "limited",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Seed the 8 allowed distinct emoji directly.
    for emoji in ["😀", "😁", "😂", "🤣", "😃", "😄", "😅", "😆"] {
        announcement::create_reaction(&pool, account.id, ann.id, emoji, None)
            .await
            .unwrap();
    }

    // A brand-new 9th emoji is rejected…
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/announcements/{}/reactions/{}", ann.id, enc("🎉")),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // …but re-reacting with one of the existing emoji still works.
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/announcements/{}/reactions/{}", ann.id, enc("😀")),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
