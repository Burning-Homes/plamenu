//! Admin dashboard metrics (`/api/v1/admin/{measures,dimensions,retention}`):
//! the `AdminUser` gate (`view_dashboard` + `admin:read`), the `Admin::Measure`/
//! `Admin::Dimension`/`Admin::Cohort` wire shapes, sign-in-backed metrics
//! (`active_users`/`languages`/`sources`/retention), and a db-level day-series
//! check.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::remote;
use plamenu_db::account::Account;
use plamenu_db::media::NewLocalMedia;
use plamenu_db::{PgPool, media, metrics, role, user};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

/// Creates a local account + user and returns an OAuth token carrying `scope`.
async fn user_with_scope(pool: &PgPool, username: &str, scope: &str) -> (Account, String) {
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
            "client_name": "admin",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": scope,
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
            ("scope", scope),
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

/// Promotes an account to the seeded Admin role (which carries `view_dashboard`).
async fn make_admin(pool: &PgPool, account_id: i64) {
    let admin = role::find_by_name(pool, "Admin").await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(admin.id))
            .await
            .unwrap()
    );
}

/// Creates an additional plain local user (for counting); returns its user id.
async fn create_user(pool: &PgPool, username: &str) -> i64 {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap()
    .id
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

/// A `[start_at, end_at]` window covering the last few days through now.
fn window() -> (String, String) {
    let now = OffsetDateTime::now_utc();
    let start = (now - Duration::days(3)).format(&Rfc3339).unwrap();
    let end = now.format(&Rfc3339).unwrap();
    (start, end)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rejects_user_without_dashboard_permission(pool: PgPool) {
    // Holds admin:read but no role → no `view_dashboard`.
    let (_, token) = user_with_scope(&pool, "alice", "read admin:read").await;
    let (start, end) = window();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({ "keys": ["new_users"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no role → 403");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn measures_new_users_returns_measure_shape(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    create_user(&pool, "bob").await;
    create_user(&pool, "carol").await;

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({ "keys": ["new_users"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measures = body.as_array().expect("array body");
    assert_eq!(measures.len(), 1, "one requested key");
    let measure = &measures[0];
    assert_eq!(measure["key"], "new_users");
    assert!(measure["unit"].is_null(), "new_users has no unit");
    // moddy + bob + carol all registered within the window.
    assert_eq!(measure["total"], "3");
    assert!(
        measure.get("human_value").is_none(),
        "non-byte measure omits human_value"
    );
    assert!(
        measure.get("previous_total").is_some(),
        "in-range measure carries previous_total"
    );
    let data = measure["data"].as_array().expect("data series");
    assert_eq!(data.len(), 4, "3 days back through today inclusive");
    let today_total: i64 = data
        .iter()
        .filter_map(|p| p["value"].as_str())
        .filter_map(|v| v.parse::<i64>().ok())
        .sum();
    assert_eq!(today_total, 3, "all three land in the window");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dimensions_software_versions(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["software_versions"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dimensions = body.as_array().expect("array body");
    assert_eq!(dimensions.len(), 1);
    let dim = &dimensions[0];
    assert_eq!(dim["key"], "software_versions");
    let keys: Vec<&str> = dim["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["key"].as_str())
        .collect();
    assert!(keys.contains(&"plamenu"), "reports its own version");
    assert!(keys.contains(&"postgresql"), "reports the PG version");
    let plamenu_row = dim["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == "plamenu")
        .unwrap();
    assert_eq!(plamenu_row["value"], plamenu::FULL_VERSION);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_measure_key_is_skipped(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({ "keys": ["new_users", "not_a_measure"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measures = body.as_array().unwrap();
    assert_eq!(measures.len(), 1, "unknown key is filtered out");
    assert_eq!(measures[0]["key"], "new_users");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn db_new_users_series_buckets_by_day(pool: PgPool) {
    create_user(&pool, "bob").await;
    create_user(&pool, "carol").await;

    let now = OffsetDateTime::now_utc();
    let start = now - Duration::days(2);
    let measurement = metrics::new_users(&pool, start, now).await.unwrap();
    assert_eq!(measurement.total, 2);
    assert_eq!(
        measurement.previous_total,
        Some(0),
        "nobody before the window"
    );
    assert_eq!(measurement.data.len(), 3, "2 days back through today");
    let series_total: i64 = measurement.data.iter().map(|p| p.value).sum();
    assert_eq!(series_total, 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_records_sign_in_and_active_users(pool: PgPool) {
    // `user_with_scope` performs an /oauth/authorize, which records a sign-in.
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({ "keys": ["active_users"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measure = &body.as_array().unwrap()[0];
    assert_eq!(measure["key"], "active_users");
    assert_eq!(measure["total"], "1", "moddy signed in once");
    assert!(measure.get("previous_total").is_some());
    // active_users emits full RFC3339 dates (unlike the SQL day measures).
    let first_date = measure["data"][0]["date"].as_str().unwrap();
    assert!(first_date.contains('T'), "RFC3339 date, got {first_date}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dimensions_languages_groups_by_locale(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    // Seed a user with a locale + recent sign-in (moddy's login carried no
    // Accept-Language, so its locale stays NULL and is excluded).
    let bob = create_user(&pool, "bob").await;
    user::record_sign_in(&pool, bob, Some("en")).await.unwrap();

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["languages"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body.as_array().unwrap()[0]["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "only bob carries a locale");
    assert_eq!(data[0]["key"], "en");
    assert_eq!(data[0]["human_key"], "English");
    assert_eq!(data[0]["value"], "1");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dimensions_sources_falls_back_to_web(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    create_user(&pool, "bob").await;
    create_user(&pool, "carol").await;

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["sources"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body.as_array().unwrap()[0]["data"].as_array().unwrap();
    // No registration attribution yet → all three group under the local web app.
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["key"], "web");
    assert_eq!(data[0]["human_key"], "Website");
    assert_eq!(data[0]["value"], "3");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn measures_interactions_counts_recorded(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    // Two local interactions today (the five call sites all funnel here).
    metrics::record_interaction(&pool).await.unwrap();
    metrics::record_interaction(&pool).await.unwrap();

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({ "keys": ["interactions"], "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measure = &body.as_array().unwrap()[0];
    assert_eq!(measure["key"], "interactions");
    assert_eq!(measure["total"], "2", "two interactions recorded today");
    assert!(
        measure.get("previous_total").is_some(),
        "in-range measure carries previous_total"
    );
    let today = measure["data"].as_array().unwrap().last().unwrap();
    assert_eq!(today["value"], "2", "both land in today's bucket");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn media_sizes_feed_attachment_measure_and_space_usage(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    // The instance measure scopes by remote domain (local accounts have a NULL
    // domain, like Mastodon), so attribute the upload to a remote account.
    let bob = RemoteUser::new("remote.example", "bob");
    let stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let media_id = plamenu_db::id::next();
    media::create_local(
        &pool,
        NewLocalMedia::new(stored.id, media_id, "pic.jpg", "image/jpeg"),
    )
    .await
    .unwrap();
    media::set_file_sizes(&pool, media_id, 1000, Some(200))
        .await
        .unwrap();

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({
            "keys": ["instance_media_attachments"],
            "start_at": start,
            "end_at": end,
            "instance_media_attachments": { "domain": "remote.example" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measure = &body.as_array().unwrap()[0];
    assert_eq!(measure["key"], "instance_media_attachments");
    assert_eq!(measure["unit"], "bytes");
    assert_eq!(measure["total"], "1200", "file + thumbnail bytes");
    assert!(
        measure.get("human_value").is_some(),
        "byte measure carries a human_value"
    );
    assert!(
        measure.get("previous_total").is_none(),
        "instance_* measures omit previous_total"
    );

    // The same bytes show up in the space_usage media line.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["space_usage"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body.as_array().unwrap()[0]["data"].as_array().unwrap();
    let media_line = data
        .iter()
        .find(|row| row["key"] == "media")
        .expect("media line present");
    assert_eq!(media_line["value"], "1200");
    assert_eq!(media_line["unit"], "bytes");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn retention_reports_self_retention(pool: PgPool) {
    // moddy registers and signs in today → retained in its own cohort (rate 1).
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;

    let (start, end) = window();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/retention",
        Some(&token),
        Some(json!({ "start_at": start, "end_at": end, "frequency": "day" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cohorts = body.as_array().expect("cohort array");
    assert!(!cohorts.is_empty(), "at least today's cohort");
    let retained = cohorts.iter().any(|cohort| {
        cohort["data"].as_array().is_some_and(|data| {
            data.iter()
                .any(|cell| cell["value"] == "1" && cell["rate"].as_f64() == Some(1.0))
        })
    });
    assert!(retained, "moddy is retained in its own day cohort");
    assert_eq!(cohorts[0]["frequency"], "day");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn measures_tag_uses_and_accounts(pool: PgPool) {
    use plamenu_db::{status, tag};

    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    let alice = create_local_account(&pool, "alice", "alice").await;
    let bob = create_local_account(&pool, "bob", "bob").await;

    let today = OffsetDateTime::now_utc().date();
    let rust = tag::ensure(&pool, "rust").await.unwrap();
    // Alice uses #rust on two public posts, bob on one; the private one is
    // never registered.
    for (author, vis) in [
        (alice.id, "public"),
        (alice.id, "public"),
        (bob.id, "public"),
        (alice.id, "private"),
    ] {
        let s = status::create_local(
            &pool,
            status::NewLocalStatus::new(author, "<p>#rust</p>", vis, None),
        )
        .await
        .unwrap();
        tag::attach(&pool, s.id, rust).await.unwrap();
        tag::record_uses(&pool, s.id, today).await.unwrap();
    }

    let (start, end) = window();

    // tag_uses: three public uses today, dated with a full RFC3339 timestamp
    // like Mastodon's tag measures (not the bare-date SQL measures).
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({
            "keys": ["tag_uses"],
            "start_at": start,
            "end_at": end,
            "tag_uses": { "id": rust.to_string() },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let measure = &body.as_array().unwrap()[0];
    assert_eq!(measure["key"], "tag_uses");
    assert_eq!(measure["total"], "3");
    assert!(
        measure.get("previous_total").is_some(),
        "in-range measure carries previous_total"
    );
    let today_point = measure["data"].as_array().unwrap().last().unwrap();
    assert_eq!(
        today_point["value"], "3",
        "all three land in today's bucket"
    );
    assert!(
        today_point["date"].as_str().unwrap().contains('T'),
        "tag measures emit full RFC3339 dates"
    );

    // tag_accounts: two distinct accounts today.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({
            "keys": ["tag_accounts"],
            "start_at": start,
            "end_at": end,
            "tag_accounts": { "id": rust.to_string() },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let measure = &body.as_array().unwrap()[0];
    assert_eq!(measure["key"], "tag_accounts");
    assert_eq!(measure["total"], "2", "two distinct accounts");
    assert_eq!(
        measure["data"].as_array().unwrap().last().unwrap()["value"],
        "2"
    );
}

// --- Metrics request cardinality/window admission ----------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn measures_reject_reversed_and_century_windows(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;

    // end_at before start_at.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({
            "keys": ["new_users"],
            "start_at": "2025-01-02T00:00:00Z",
            "end_at": "2025-01-01T00:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "reversed window");

    // A 125-year span — the query would scan an unbounded range.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/measures",
        Some(&token),
        Some(json!({
            "keys": ["new_users"],
            "start_at": "1900-01-01T00:00:00Z",
            "end_at": "2025-01-01T00:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "century window");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dimensions_reject_oversized_keys_and_limit(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;
    let (start, end) = window();

    // 25 keys > MAX_METRICS_KEYS (24).
    let many: Vec<String> = (0..25).map(|i| format!("languages{i}")).collect();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": many, "start_at": start, "end_at": end })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "too many keys");

    // A non-positive top-N limit.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["languages"], "start_at": start, "end_at": end, "limit": 0 })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "non-positive limit"
    );

    // A pathologically large top-N limit.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/dimensions",
        Some(&token),
        Some(json!({ "keys": ["languages"], "start_at": start, "end_at": end, "limit": 100_000 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "oversized limit");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn retention_rejects_multi_year_day_span(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin.id).await;

    // Two years of daily cohorts → O(days²) cross product; over MAX_RETENTION_DAYS.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/retention",
        Some(&token),
        Some(json!({
            "start_at": "2023-01-01T00:00:00Z",
            "end_at": "2025-01-01T00:00:00Z",
            "frequency": "day",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "retention day span capped"
    );
}
