//! Admin report moderation (`/api/v1/admin/reports` index/show + the
//! update/assign/reopen/resolve verbs): the `AdminUser` gate
//! (`MANAGE_REPORTS` + `admin:*`), the `Admin::Report` entity shape, the
//! resolved/unresolved status filter, and the action lifecycle.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::report::{self, NewReport};
use plamenu_db::{PgPool, role, rule, user};
use serde_json::{Value, json};
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

/// Promotes an account to the seeded Admin role.
async fn make_admin(pool: &PgPool, account_id: i64) {
    let admin = role::find_by_name(pool, "Admin").await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(admin.id))
            .await
            .unwrap()
    );
}

/// Files a report from `reporter` against `target` and returns its id.
async fn file_report(pool: &PgPool, reporter: i64, target: i64, category: &str) -> i64 {
    report::create(
        pool,
        NewReport {
            account_id: reporter,
            target_account_id: target,
            status_ids: &[],
            comment: "please review",
            category,
            forwarded: None,
            rule_ids: None,
            uri: None,
        },
    )
    .await
    .unwrap()
    .id
}

/// Sets up an admin token plus a reporter and a target account.
async fn fixture(pool: &PgPool) -> (String, Account, Account) {
    let (admin_account, token) =
        user_with_scope(pool, "moddy", "read write admin:read admin:write").await;
    make_admin(pool, admin_account.id).await;
    let reporter = create_local_account(pool, "alice", "Alice").await;
    let target = create_local_account(pool, "carol", "Carol").await;
    (token, reporter, target)
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
async fn rejects_user_without_role(pool: PgPool) {
    let (_, token) = user_with_scope(&pool, "alice", "read write admin:read admin:write").await;
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/reports",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no role → 403");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rejects_write_verb_without_write_scope(pool: PgPool) {
    // Holds the role and admin:read, but resolve needs admin:write.
    let (admin_account, token) = user_with_scope(&pool, "moddy", "read admin:read").await;
    make_admin(&pool, admin_account.id).await;
    let reporter = create_local_account(&pool, "alice", "Alice").await;
    let target = create_local_account(&pool, "carol", "Carol").await;
    let id = file_report(&pool, reporter.id, target.id, "spam").await;

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/reports/{id}/resolve"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "missing admin:write → 403");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn show_returns_admin_entity(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    let id = file_report(&pool, reporter.id, target.id, "spam").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/reports/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id.to_string());
    assert_eq!(body["category"], "spam");
    assert_eq!(body["action_taken"], false);
    assert_eq!(body["action_taken_at"], Value::Null);
    assert_eq!(body["assigned_account"], Value::Null);
    assert_eq!(body["action_taken_by_account"], Value::Null);
    // The parties are `Admin::Account`-serialized (carry the moderation overlay).
    assert_eq!(body["account"]["username"], "alice");
    // No `users` row backs this account, so the overlay email is null.
    assert_eq!(body["account"]["email"], Value::Null);
    assert_eq!(body["target_account"]["username"], "carol");
    assert_eq!(body["statuses"], json!([]));
    assert_eq!(body["rules"], json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn index_defaults_to_unresolved(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    let open = file_report(&pool, reporter.id, target.id, "spam").await;
    let closed = file_report(&pool, reporter.id, target.id, "other").await;
    report::resolve(&pool, closed, reporter.id).await.unwrap();

    // Default view: unresolved only.
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/reports",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [open.to_string()], "only the unresolved report");

    // resolved=true flips to the resolved set.
    let (_, resolved) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/reports?resolved=true",
        Some(&token),
        None,
    )
    .await;
    let ids: Vec<&str> = resolved
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [closed.to_string()]);

    // Both flags → every report.
    let (_, all) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/reports?resolved=true&unresolved=true",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn resolve_and_reopen_lifecycle(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    let id = file_report(&pool, reporter.id, target.id, "spam").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/reports/{id}/resolve"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["action_taken"], true);
    assert_ne!(body["action_taken_at"], Value::Null);
    // The acting moderator is recorded.
    assert_eq!(body["action_taken_by_account"]["username"], "moddy");

    let (_, reopened) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/reports/{id}/reopen"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(reopened["action_taken"], false);
    assert_eq!(reopened["action_taken_at"], Value::Null);
    assert_eq!(reopened["action_taken_by_account"], Value::Null);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn assign_and_unassign(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    let id = file_report(&pool, reporter.id, target.id, "spam").await;

    let (status, assigned) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/reports/{id}/assign_to_self"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(assigned["assigned_account"]["username"], "moddy");

    let (_, unassigned) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/reports/{id}/unassign"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(unassigned["assigned_account"], Value::Null);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_category_and_rules(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    let id = file_report(&pool, reporter.id, target.id, "other").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/admin/reports/{id}"),
        Some(&token),
        Some(json!({ "category": "violation", "rule_ids": [3, 7] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["category"], "violation");

    // The raw rule ids are persisted (stringified, like Mastodon ids).
    let stored = report::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(stored.rule_ids, Some(vec![3, 7]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn report_resolves_cited_rules(pool: PgPool) {
    let (token, reporter, target) = fixture(&pool).await;
    // Two live rules; the report cites them out of priority order.
    let first = rule::create(&pool, "No spam", "Keep it relevant", None)
        .await
        .unwrap();
    let second = rule::create(&pool, "Be kind", "", None).await.unwrap();

    let id = report::create(
        &pool,
        NewReport {
            account_id: reporter.id,
            target_account_id: target.id,
            status_ids: &[],
            comment: "please review",
            category: "violation",
            forwarded: None,
            rule_ids: Some(&[second.id, first.id]),
            uri: None,
        },
    )
    .await
    .unwrap()
    .id;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/reports/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // `rules` resolves to the cited Rule entities, ordered by priority/id
    // regardless of citation order, with stringified ids.
    let rules = body["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0]["id"], first.id.to_string());
    assert_eq!(rules[0]["text"], "No spam");
    assert_eq!(rules[0]["hint"], "Keep it relevant");
    assert_eq!(rules[0]["translations"], json!({}));
    assert_eq!(rules[1]["id"], second.id.to_string());

    // A discarded rule still resolves for a report that cited it.
    assert!(rule::delete(&pool, first.id).await.unwrap());
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/reports/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["rules"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn missing_report_is_404(pool: PgPool) {
    let (token, _, _) = fixture(&pool).await;
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/reports/999999",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
