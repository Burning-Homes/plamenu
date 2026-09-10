//! P4 — Pleroma-style rich-text posting: the `content_type` parameter on
//! create/edit/scheduled statuses, the markdown/HTML render pipelines, the
//! `/source` and instance-metadata extensions, and the AP `source` property.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{StubFederation, create_local_account, test_app, test_state_with};
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
            "client_name": "richtext",
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

async fn ap_object(app: Router, path: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn markdown_post_renders_html_and_keeps_source(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (_bob, _) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    let (status, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "**bold** _em_ hi @bob #Tag https://example.com/x [named](https://example.com/named)",
            "content_type": "text/markdown",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    let content = posted["content"].as_str().unwrap();
    assert!(content.contains("<strong>bold</strong>"), "{content}");
    assert!(content.contains("<em>em</em>"), "{content}");
    // Mentions, hashtags and bare URLs get the same anchors as plain posts.
    assert!(content.contains(r#"class="u-url mention""#), "{content}");
    assert!(content.contains("/tags/tag"), "{content}");
    assert!(
        content.contains(r#"<span class="invisible">https://</span>"#),
        "bare URLs render the shortened anchor: {content}"
    );
    // Markdown link syntax survives as a regular (sanitized) anchor.
    assert!(
        content.contains(r#"href="https://example.com/named""#),
        "{content}"
    );
    // Mention/tag rows were persisted like a plain post's.
    assert_eq!(posted["mentions"][0]["acct"], "bob");
    assert_eq!(posted["tags"][0]["name"], "tag");

    // `/source` returns the raw markdown and its format.
    let id = posted["id"].as_str().unwrap();
    let (_, source) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(source["content_type"], "text/markdown");
    assert!(
        source["text"].as_str().unwrap().starts_with("**bold**"),
        "{source}"
    );

    // The AP Note carries the raw source alongside the rendered content.
    let note = ap_object(app(), &format!("/users/alice/statuses/{id}")).await;
    assert_eq!(note["source"]["mediaType"], "text/markdown");
    assert!(
        note["source"]["content"]
            .as_str()
            .unwrap()
            .starts_with("**bold**"),
        "{note}"
    );
    assert!(
        note["content"]
            .as_str()
            .unwrap()
            .contains("<strong>bold</strong>")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn markdown_leaves_code_and_existing_links_alone(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (status, posted) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "run `#not_a_tag` then\n```\n@not_a_mention\n```",
            "content_type": "text/markdown",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    let content = posted["content"].as_str().unwrap();
    assert!(content.contains("<code>#not_a_tag</code>"), "{content}");
    assert!(content.contains("@not_a_mention"), "{content}");
    assert!(!content.contains("hashtag"), "{content}");
    assert!(!content.contains("mention\""), "{content}");
    assert!(posted["mentions"].as_array().unwrap().is_empty());
    assert!(posted["tags"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn html_post_is_sanitized_and_linkified(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (status, posted) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "<p>hello <b>there</b> #tag</p><script>alert(1)</script><p onclick=\"x()\">safe</p>",
            "content_type": "text/html",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    let content = posted["content"].as_str().unwrap();
    assert!(content.contains("<b>there</b>"), "{content}");
    assert!(content.contains("/tags/tag"), "{content}");
    assert!(!content.contains("script"), "{content}");
    assert!(!content.contains("onclick"), "{content}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unsupported_and_absent_content_types_stay_plain(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // Absent: plain text, markdown syntax is literal.
    let (_, plain) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**not bold**"})),
    )
    .await;
    assert_eq!(plain["content"], "<p>**not bold**</p>");

    // Unsupported (bbcode) falls back to plain, like Pleroma.
    let (status, bbcode) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "[b]nope[/b]", "content_type": "text/bbcode"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bbcode["content"], "<p>[b]nope[/b]</p>");

    let id = plain["id"].as_str().unwrap();
    let (_, source) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(source["content_type"], "text/plain");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn user_default_format_applies_when_param_absent(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let user_row = user::find_by_email(&pool, "alice@plamenu.test")
        .await
        .unwrap()
        .unwrap();
    let mut settings = user::settings_by_user_id(&pool, user_row.id)
        .await
        .unwrap()
        .unwrap();
    settings.posting_default_content_type = user::PostingDefaultFormat::Markdown;
    user::update_settings(&pool, user_row.id, settings)
        .await
        .unwrap();

    let (_, posted) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**bold by default**"})),
    )
    .await;
    assert!(
        posted["content"]
            .as_str()
            .unwrap()
            .contains("<strong>bold by default</strong>"),
        "{posted}"
    );

    // An explicit param still wins over the default.
    let (_, explicit) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**literal**", "content_type": "text/plain"})),
    )
    .await;
    assert_eq!(explicit["content"], "<p>**literal**</p>");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_can_switch_format_and_keeps_it_otherwise(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let (_, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**one**", "content_type": "text/markdown"})),
    )
    .await;
    let id = posted["id"].as_str().unwrap().to_owned();
    assert!(posted["content"].as_str().unwrap().contains("<strong>"));

    // Edit without content_type: the stored markdown format re-renders.
    let (status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "**two**"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert!(
        edited["content"]
            .as_str()
            .unwrap()
            .contains("<strong>two</strong>"),
        "{edited}"
    );

    // Edit switching to plain: markdown becomes literal, `/source` follows.
    let (_, replain) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "**two**", "content_type": "text/plain"})),
    )
    .await;
    assert_eq!(replain["content"], "<p>**two**</p>");
    let (_, source) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(source["content_type"], "text/plain");
    assert_eq!(source["text"], "**two**");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn format_only_edit_is_a_real_edit(pool: PgPool) {
    // Same text, same rendered HTML, different format: the no-change
    // shortcut must not swallow it, or `/source` would lie to editors.
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());
    let (_, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "just words", "content_type": "text/plain"})),
    )
    .await;
    let id = posted["id"].as_str().unwrap().to_owned();

    let (status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "just words", "content_type": "text/markdown"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let (_, source) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(source["content_type"], "text/markdown");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_status_captures_and_replays_format(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let scheduled_at = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let (status, scheduled) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "**later**",
            "content_type": "text/markdown",
            "scheduled_at": scheduled_at,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{scheduled}");
    assert_eq!(scheduled["params"]["content_type"], "text/markdown");

    // Force it due and let the sweeper publish it.
    let scheduled_id: i64 = scheduled["id"].as_str().unwrap().parse().unwrap();
    sqlx::query!(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
        scheduled_id,
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[]));
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);

    let (_, timeline) = api(
        app(),
        "GET",
        "/api/v1/accounts/lookup?acct=alice",
        None,
        None,
    )
    .await;
    let account_id = timeline["id"].as_str().unwrap();
    let (_, statuses) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{account_id}/statuses"),
        Some(&token),
        None,
    )
    .await;
    let content = statuses[0]["content"].as_str().unwrap();
    assert!(content.contains("<strong>later</strong>"), "{content}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_metadata_advertises_post_formats(pool: PgPool) {
    let app = || test_app(pool.clone());
    let expected = json!(["text/plain", "text/markdown", "text/html"]);

    let (_, v1) = api(app(), "GET", "/api/v1/instance", None, None).await;
    assert_eq!(v1["pleroma"]["metadata"]["post_formats"], expected);

    let (_, v2) = api(app(), "GET", "/api/v2/instance", None, None).await;
    assert_eq!(v2["pleroma"]["metadata"]["post_formats"], expected);

    let (_, nodeinfo) = api(app(), "GET", "/nodeinfo/2.0", None, None).await;
    assert_eq!(nodeinfo["metadata"]["postFormats"], expected);
}
