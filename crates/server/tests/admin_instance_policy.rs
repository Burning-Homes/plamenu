//! Admin instance-policy API: resource-specific admin OAuth scopes plus
//! CRUD over domain blocks/allows and access blocks.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, role, user};
use serde_json::{Value, json};
use tower::ServiceExt;

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
            "client_name": "admin-policy",
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

async fn make_role(pool: &PgPool, account_id: i64, name: &str) {
    let role = role::find_by_name(pool, name).await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(role.id))
            .await
            .unwrap()
    );
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
async fn resource_specific_admin_scopes_do_not_leak(pool: PgPool) {
    let (admin, token) = user_with_scope(
        &pool,
        "policy",
        "read write admin:read:domain_blocks admin:write:domain_blocks",
    )
    .await;
    make_role(&pool, admin.id, "Admin").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/domain_blocks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "domain_blocks scope can read: {body}"
    );

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/accounts",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "domain_blocks read scope must not read accounts"
    );

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&token),
        Some(json!({ "domain": "Bad.Example", "severity": "suspend" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "domain_blocks scope can write: {body}"
    );
    assert_eq!(body["domain"], "bad.example");
    assert_eq!(body["severity"], "suspend");

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/domain_allows",
        Some(&token),
        Some(json!({ "domain": "friend.example" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "domain_blocks write scope must not write domain_allows"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn policy_records_crud(pool: PgPool) {
    let (admin, token) =
        user_with_scope(&pool, "ownerish", "read write admin:read admin:write").await;
    make_role(&pool, admin.id, "Admin").await;

    let (status, domain_block) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&token),
        Some(json!({
            "domain": "blocked.example",
            "severity": "silence",
            "reject_media": true,
            "private_comment": "operator note",
            "public_comment": "public reason",
            "obfuscate": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{domain_block}");
    assert_eq!(domain_block["domain"], "blocked.example");
    assert_eq!(domain_block["reject_media"], true);
    assert_eq!(domain_block["digest"].as_str().unwrap().len(), 64);
    let domain_block_id = domain_block["id"].as_str().unwrap();

    let (status, updated) = api(
        test_app(pool.clone()),
        "PATCH",
        &format!("/api/v1/admin/domain_blocks/{domain_block_id}"),
        Some(&token),
        Some(json!({ "severity": "suspend", "reject_reports": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["severity"], "suspend");
    assert_eq!(updated["reject_reports"], true);

    let (status, list) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/domain_blocks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, allow) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/domain_allows",
        Some(&token),
        Some(json!({ "domain": "friend.example" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{allow}");
    assert_eq!(allow["domain"], "friend.example");

    let (status, email_block) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/email_domain_blocks",
        Some(&token),
        Some(json!({ "domain": "mailbad.example", "allow_with_approval": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{email_block}");
    assert_eq!(email_block["allow_with_approval"], true);
    assert_eq!(email_block["history"], json!([]));

    let (status, ip_block) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/ip_blocks",
        Some(&token),
        Some(json!({
            "ip": "192.0.2.0/24",
            "severity": "no_access",
            "comment": "test network",
            "expires_in": "86400"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ip_block}");
    assert_eq!(ip_block["ip"], "192.0.2.0/24");
    assert_eq!(ip_block["severity"], "no_access");
    assert!(ip_block["expires_at"].is_string());

    let (status, canonical) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/canonical_email_blocks",
        Some(&token),
        Some(json!({ "email": "First.Last+tag@Example.COM" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{canonical}");
    let canonical_id = canonical["id"].as_str().unwrap();
    let canonical_hash = canonical["canonical_email_hash"].as_str().unwrap();
    assert_eq!(canonical_hash.len(), 64);

    let (status, matches) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/admin/canonical_email_blocks/test",
        Some(&token),
        Some(json!({ "email": "firstlast@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{matches}");
    assert_eq!(matches.as_array().unwrap()[0]["id"], canonical_id);

    let (status, empty) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/admin/domain_blocks/{domain_block_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty, json!({}));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn federation_policy_requires_manage_federation(pool: PgPool) {
    let (moderator, token) = user_with_scope(
        &pool,
        "moddy",
        "read admin:read:domain_blocks admin:read:email_domain_blocks",
    )
    .await;
    make_role(&pool, moderator.id, "Moderator").await;

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/domain_blocks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "moderator lacks manage_federation"
    );

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/email_domain_blocks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderator has manage_blocks");
}
