//! Posting-limit tests: the configurable `instance_settings` limits
//! (character count, media attachments, poll options) are advertised by the
//! instance API and enforced on status create/edit, with the character count
//! weighed like Mastodon's `StatusLengthValidator`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, instance_settings, user};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Generic JSON API call; returns (status, body).
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

/// Form-encoded POST; returns (status, body text).
async fn post_form(
    app: Router,
    uri: &str,
    bearer: Option<&str>,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

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
            "client_name": "limits",
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
        None,
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", "read write"),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await
    .1;
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

/// Sets the posting limits, keeping the other settings untouched.
async fn set_limits(pool: &PgPool, max_characters: i32, max_media: i32, poll_options: i32) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        instance_settings::SettingsUpdate {
            max_characters,
            max_media_attachments: max_media,
            poll_max_options: poll_options,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

async fn post_status(pool: &PgPool, token: &str, body: Value) -> (StatusCode, Value) {
    api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(body),
    )
    .await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn character_limit_counts_like_mastodons_validator(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;

    // Exactly at the default 5000 passes; one over is refused with
    // Mastodon's wording.
    let (status, _) = post_status(&pool, &token, json!({"status": "a".repeat(5000)})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_status(&pool, &token, json!({"status": "a".repeat(5001)})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Text character limit of 5000 exceeded"
    );

    // The spoiler counts toward the same budget.
    let (status, _) = post_status(
        &pool,
        &token,
        json!({"status": "a".repeat(4990), "spoiler_text": "b".repeat(10)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_status(
        &pool,
        &token,
        json!({"status": "a".repeat(4990), "spoiler_text": "b".repeat(11)}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Text character limit of 5000 exceeded"
    );

    // A URL is a fixed 23 characters, however long: 4976 letters + space +
    // a 60-char URL is 5037 raw characters but weighs exactly 5000.
    let url = format!("https://example.com/{}", "p".repeat(40));
    let over_limit_raw = format!("{} {url}", "a".repeat(4976));
    assert_eq!(over_limit_raw.chars().count(), 5037);
    let (status, _) = post_status(&pool, &token, json!({"status": over_limit_raw})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_status(
        &pool,
        &token,
        json!({"status": format!("{} {url}", "a".repeat(4977))}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A remote mention's domain is free: `@bob@remote.example` weighs 4.
    let (status, _) = post_status(
        &pool,
        &token,
        json!({"status": format!("{} @bob@remote.example", "a".repeat(4995))}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_status(
        &pool,
        &token,
        json!({"status": format!("{} @bob@remote.example", "a".repeat(4996))}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Grapheme clusters, not chars: 5000 combining-accent pairs are 10000
    // chars but count as 5000.
    let (status, _) = post_status(&pool, &token, json!({"status": "e\u{301}".repeat(5000)})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_status(&pool, &token, json!({"status": "e\u{301}".repeat(5001)})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn configured_limits_are_advertised_and_enforced(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    set_limits(&pool, 10, 2, 3).await;

    // Both instance entities report the live values.
    let (_, v1) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/instance",
        None,
        None,
    )
    .await;
    assert_eq!(v1["configuration"]["statuses"]["max_characters"], 10);
    assert_eq!(v1["configuration"]["statuses"]["max_media_attachments"], 2);
    assert_eq!(v1["configuration"]["polls"]["max_options"], 3);
    let (_, v2) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/instance",
        None,
        None,
    )
    .await;
    assert_eq!(v2["configuration"]["statuses"]["max_characters"], 10);
    assert_eq!(v2["configuration"]["statuses"]["max_media_attachments"], 2);
    assert_eq!(v2["configuration"]["polls"]["max_options"], 3);

    // The character limit is enforced at the configured value.
    let (status, _) = post_status(&pool, &token, json!({"status": "a".repeat(10)})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_status(&pool, &token, json!({"status": "a".repeat(11)})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Text character limit of 10 exceeded"
    );

    // The attachment cap is checked before the ids are resolved, so bogus
    // ids demonstrate the count refusal.
    let (status, body) = post_status(
        &pool,
        &token,
        json!({"status": "hi", "media_ids": ["1", "2", "3"]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Cannot attach more than 2 files");

    // The poll option cap follows the setting, with the live number in the
    // refusal.
    let (status, body) = post_status(
        &pool,
        &token,
        json!({
            "status": "vote",
            "poll": {"options": ["a", "b", "c", "d"], "expires_in": 3600},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Options can't contain more than 3 items"
    );
    let (status, _) = post_status(
        &pool,
        &token,
        json!({
            "status": "vote",
            "poll": {"options": ["a", "b", "c"], "expires_in": 3600},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edits_respect_the_character_limit(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (status, created) = post_status(&pool, &token, json!({"status": "short"})).await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    let (status, body) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "a".repeat(5001)})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Text character limit of 5000 exceeded"
    );

    // An in-budget edit still lands.
    let (status, edited) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "a".repeat(5000)})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["id"].as_str().unwrap(), id);
}
