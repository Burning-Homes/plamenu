//! Admin account moderation reads (`GET /api/v1|v2/admin/accounts` and show):
//! the `AdminUser` HTTP-level gate (role bit + `admin:*` scope), the
//! `Admin::Account` entity shape, and the listing filters.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::remote;
use plamenu_db::account::{self, Account};
use plamenu_db::{PgPool, follow, role, user};
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
    // Has the admin scope but no moderation role assigned.
    let (_, token) = user_with_scope(&pool, "alice", "read write admin:read admin:write").await;
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/accounts",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no role → 403: {body}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rejects_admin_without_scope(pool: PgPool) {
    // Holds the role, but the token lacks `admin:read`.
    let (account, token) = user_with_scope(&pool, "alice", "read write").await;
    make_admin(&pool, account.id).await;
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/accounts",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "missing admin:read → 403");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn lists_accounts_with_overlay(pool: PgPool) {
    let (account, token) =
        user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, account.id).await;
    let mod_user = user::find_by_email(&pool, "moddy@plamenu.test")
        .await
        .unwrap()
        .unwrap();
    user::record_sign_in_with_ip(
        &pool,
        mod_user.id,
        Some("ka"),
        Some("198.51.100.7"),
        plamenu_db::user::SignInContext::default(),
    )
    .await
    .unwrap();
    create_local_account(&pool, "bob", "Bob").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/accounts",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().unwrap();
    assert!(list.len() >= 2, "lists every account: {body}");

    // The admin's own entry carries the moderation overlay.
    let mine = list
        .iter()
        .find(|a| a["id"].as_str() == Some(&account.id.to_string()))
        .expect("admin account present");
    assert_eq!(mine["email"], "moddy@plamenu.test");
    assert_eq!(mine["approved"], true);
    assert_eq!(mine["confirmed"], true);
    assert_eq!(mine["disabled"], false);
    assert_eq!(mine["suspended"], false);
    assert_eq!(mine["role"]["name"], "Admin");
    assert_eq!(mine["locale"], "ka");
    assert_eq!(mine["ip"], "198.51.100.7");
    assert_eq!(mine["ips"][0]["ip"], "198.51.100.7");
    assert!(
        mine["ips"][0]["used_at"]
            .as_str()
            .is_some_and(|used_at| used_at.ends_with('Z'))
    );
    // Embeds the public account entity.
    assert_eq!(mine["account"]["username"], "moddy");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn show_returns_admin_entity(pool: PgPool) {
    let (account, token) =
        user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, account.id).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/accounts/{}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], bob.id.to_string());
    assert_eq!(body["account"]["username"], "bob");
    // Bob has no local user row created → overlay nulls.
    assert_eq!(body["email"], Value::Null);

    let (missing, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/accounts/999999",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

/// The HTTP status of the `ActivityPub` actor document for `username`.
async fn actor_status(app: Router, username: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/users/{username}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// The actor document body, for asserting on the suspended serialization.
async fn actor_json(app: Router, username: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/users/{username}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn disable_action_blocks_login_and_enable_restores(pool: PgPool) {
    let (admin, atoken) =
        user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;
    let (bob, btoken) = user_with_scope(&pool, "bob", "read").await;

    // Bob's token works before the action.
    let (ok, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&btoken),
        None,
    )
    .await;
    assert_eq!(ok, StatusCode::OK);

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", bob.id),
        Some(&atoken),
        Some(json!({ "type": "disable", "text": "spam" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "action returns empty 200");

    // The disabled user's existing token stops resolving.
    let (blocked, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&btoken),
        None,
    )
    .await;
    assert_eq!(blocked, StatusCode::FORBIDDEN, "disabled token → 403");

    let (_, show) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/accounts/{}", bob.id),
        Some(&atoken),
        None,
    )
    .await;
    assert_eq!(show["disabled"], true);

    // Enable lifts it.
    let (enabled, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/enable", bob.id),
        Some(&atoken),
        None,
    )
    .await;
    assert_eq!(enabled, StatusCode::OK);
    let (restored, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&btoken),
        None,
    )
    .await;
    assert_eq!(restored, StatusCode::OK, "enabled token works again");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn suspend_hides_actor_then_unsuspend_restores(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;
    let (bob, bob_token) = user_with_scope(&pool, "bob", "read write").await;
    let post = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(bob.id, "<p>hello</p>", "public", None),
    )
    .await
    .unwrap();

    assert_eq!(
        actor_status(test_app(pool.clone()), "bob").await,
        StatusCode::OK
    );

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", bob.id),
        Some(&token),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The admin view reflects the suspension; the actor document is now gone.
    let (_, show) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/accounts/{}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(show["suspended"], true);
    assert_eq!(show["account"]["suspended"], true, "public entity flags it");
    assert_eq!(show["account"]["display_name"], "");
    assert_eq!(show["account"]["note"], "");
    assert_eq!(show["account"]["fields"], json!([]));
    // The actor document still answers — blanked, carrying `suspended: true`
    // so peers mirror the reversible suspension (Mastodon parity); the
    // profile is stripped down to the bare username.
    let actor = actor_json(test_app(pool.clone()), "bob").await;
    assert_eq!(actor["suspended"], true);
    assert_eq!(actor["name"], "bob");
    assert_eq!(actor["discoverable"], false);
    assert!(
        actor
            .get("attachment")
            .is_some_and(|a| a.as_array().is_some_and(Vec::is_empty))
    );
    let (profile, _) = api(test_app(pool.clone()), "GET", "/@bob", None, None).await;
    assert_eq!(
        profile,
        StatusCode::FORBIDDEN,
        "HTML profile is unavailable"
    );
    let (statuses, statuses_body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", bob.id),
        None,
        None,
    )
    .await;
    assert_eq!(statuses, StatusCode::OK);
    assert_eq!(
        statuses_body,
        json!([]),
        "existing posts disappear immediately"
    );
    let (login, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(login, StatusCode::FORBIDDEN, "existing tokens stop acting");
    let (_, relationships) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/relationships?id={}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(relationships, json!([]));
    let (_, relationships) = api(
        test_app(pool.clone()),
        "GET",
        &format!(
            "/api/v1/accounts/relationships?id={}&with_suspended=true",
            bob.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(relationships.as_array().map(Vec::len), Some(1));

    let (lifted, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/unsuspend", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(lifted, StatusCode::OK);
    assert_eq!(body["suspended"], false);
    let (_, restored_statuses) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", bob.id),
        None,
        None,
    )
    .await;
    assert_eq!(restored_statuses[0]["id"], post.id.to_string());
    assert_eq!(
        actor_status(test_app(pool.clone()), "bob").await,
        StatusCode::OK
    );

    // Both actions were recorded in the audit log, newest first.
    let logs = plamenu_db::admin_action_log::list(
        &pool,
        &plamenu_db::admin_action_log::LogFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let lines: Vec<_> = logs
        .iter()
        .map(|log| (log.action.as_str(), log.target_type.as_str()))
        .collect();
    assert_eq!(lines, [("unsuspend", "Account"), ("suspend", "Account")]);
    assert_eq!(logs[0].account_username, "moddy");
    assert_eq!(logs[0].human_identifier, "@bob");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn suspending_remote_actor_rejects_its_local_follows(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;
    let local = create_local_account(&pool, "alice", "Alice").await;
    let remote_user = RemoteUser::new("remote.example", "bob");
    let remote = remote::store_remote_actor(&pool, &remote_user.actor)
        .await
        .unwrap();
    follow::create(
        &pool,
        remote.id,
        local.id,
        Some("https://remote.example/follows/1"),
    )
    .await
    .unwrap();

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", remote.id),
        Some(&token),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        follow::find(&pool, remote.id, local.id)
            .await
            .unwrap()
            .is_none()
    );
    let reject = sqlx::query_scalar!(
        r#"SELECT activity AS "activity!" FROM delivery_jobs
           WHERE account_id = $1 ORDER BY id DESC LIMIT 1"#,
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reject["type"], "Reject");
    assert_eq!(reject["object"]["type"], "Follow");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn action_requires_write_scope_and_valid_type(pool: PgPool) {
    // Read-only admin scope cannot act.
    let (reader, rtoken) = user_with_scope(&pool, "reader", "read admin:read").await;
    make_admin(&pool, reader.id).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let (forbidden, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", bob.id),
        Some(&rtoken),
        Some(json!({ "type": "silence" })),
    )
    .await;
    assert_eq!(
        forbidden,
        StatusCode::FORBIDDEN,
        "missing admin:write → 403"
    );

    // A writer rejects an unknown action type.
    let (writer, wtoken) =
        user_with_scope(&pool, "writer", "read write admin:read admin:write").await;
    make_admin(&pool, writer.id).await;
    let (bad, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", bob.id),
        Some(&wtoken),
        Some(json!({ "type": "banish" })),
    )
    .await;
    assert_eq!(bad, StatusCode::UNPROCESSABLE_ENTITY, "bad type → 422");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_only_actions_forbid_userless_accounts(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;
    // A local account with no `users` row (e.g. system actor) cannot be
    // enabled/approved/rejected — Mastodon's `require_local_account!`.
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/enable", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reject_deletes_the_account(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;
    let (bob, _) = user_with_scope(&pool, "bob", "read").await;

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/reject", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (missing, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/accounts/{}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND, "the account is gone");
}

/// Promotes an account to the seeded low-position Moderator role, which carries
/// `MANAGE_USERS` but not `DELETE_USER_DATA`.
async fn make_moderator(pool: &PgPool, account_id: i64) {
    let moderator = role::find_by_name(pool, "Moderator")
        .await
        .unwrap()
        .unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(moderator.id))
            .await
            .unwrap()
    );
}

/// #48: permanent deletion now requires `DELETE_USER_DATA`. A Moderator holds
/// `MANAGE_USERS` (so the account surface is reachable) but not that bit, so
/// `DELETE /admin/accounts/{id}` is forbidden and the target survives.
#[sqlx::test(migrations = "../db/migrations")]
async fn moderator_cannot_hard_delete_without_delete_user_data(pool: PgPool) {
    let (moddy, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_moderator(&pool, moddy.id).await;
    let (bob, _) = user_with_scope(&pool, "bob", "read").await;

    let (status, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no DELETE_USER_DATA → 403");
    assert!(
        account::find_by_id(&pool, bob.id).await.unwrap().is_some(),
        "the account must survive a forbidden destroy"
    );
}

/// #48: the REST hard-delete previously had no self-check. A moderator may not
/// destroy their own account (the outrank rule denies acting on an equal role).
#[sqlx::test(migrations = "../db/migrations")]
async fn cannot_hard_delete_own_account(pool: PgPool) {
    let (admin, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, admin.id).await;

    let (status, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", admin.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "self-destroy → 403");
    assert!(
        account::find_by_id(&pool, admin.id)
            .await
            .unwrap()
            .is_some(),
        "the moderator's own account must survive"
    );
}

/// #48: role hierarchy. A low-position Moderator cannot moderate a
/// higher-ranked Admin — the previous code let anyone with `MANAGE_USERS`
/// suspend or delete an Admin/Owner.
#[sqlx::test(migrations = "../db/migrations")]
async fn moderator_cannot_moderate_a_higher_ranked_admin(pool: PgPool) {
    let (moddy, token) = user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_moderator(&pool, moddy.id).await;
    let (boss, _) = user_with_scope(&pool, "boss", "read").await;
    make_admin(&pool, boss.id).await;

    // Suspend is forbidden…
    let (suspend, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", boss.id),
        Some(&token),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(suspend, StatusCode::FORBIDDEN, "can't outrank an Admin");
    assert!(
        !account::find_by_id(&pool, boss.id)
            .await
            .unwrap()
            .unwrap()
            .suspended(),
        "the Admin must not be suspended"
    );

    // …and so is a permanent delete.
    let (destroy, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", boss.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(destroy, StatusCode::FORBIDDEN);
    assert!(account::find_by_id(&pool, boss.id).await.unwrap().is_some());
}

/// Promotes an account to the seeded Owner role (position 100) — the only
/// built-in role carrying the `administrator` permission.
async fn make_owner(pool: &PgPool, account_id: i64) {
    let owner = role::find_by_name(pool, "Owner").await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(owner.id))
            .await
            .unwrap()
    );
}

/// Assigns a bespoke role positioned *above* the owner that can manage and
/// delete users but is not itself an administrator — the one configuration in
/// which the last-administrator guard is reachable through the moderation
/// surface (it outranks the owner yet cannot administer in their place).
async fn make_superuser(pool: &PgPool, account_id: i64) {
    let super_role = role::create(
        pool,
        "Superuser",
        "#123456",
        200,
        role::permission::MANAGE_USERS | role::permission::DELETE_USER_DATA,
        false,
    )
    .await
    .unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(super_role.id))
            .await
            .unwrap()
    );
}

/// #48: the last administrator may not be permanently deleted, or the instance
/// would be left with nobody who can administer it. The guard lifts only once a
/// second active owner exists.
#[sqlx::test(migrations = "../db/migrations")]
async fn cannot_hard_delete_the_last_administrator(pool: PgPool) {
    let (boss, token) = user_with_scope(&pool, "boss", "read write admin:read admin:write").await;
    make_superuser(&pool, boss.id).await;
    let (owner, _) = user_with_scope(&pool, "owner", "read").await;
    make_owner(&pool, owner.id).await;

    let destroy = |pool: PgPool, token: String, target: i64| async move {
        api(
            test_app(pool),
            "DELETE",
            &format!("/api/v1/admin/accounts/{target}"),
            Some(&token),
            None,
        )
        .await
        .0
    };

    // The superuser outranks the owner and holds DELETE_USER_DATA, but the owner
    // is the only administrator, so the destroy is refused and the owner remains.
    let refused = destroy(pool.clone(), token.clone(), owner.id).await;
    assert_eq!(
        refused,
        StatusCode::FORBIDDEN,
        "last administrator protected"
    );
    assert!(
        account::find_by_id(&pool, owner.id)
            .await
            .unwrap()
            .is_some(),
        "the last owner must survive a forbidden destroy"
    );

    // With a second active owner, the first is no longer the last administrator.
    let (owner2, _) = user_with_scope(&pool, "owner2", "read").await;
    make_owner(&pool, owner2.id).await;
    let (suspended, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", owner.id),
        Some(&token),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(suspended, StatusCode::OK, "suspend before permanent purge");
    let allowed = destroy(pool.clone(), token, owner.id).await;
    assert_eq!(allowed, StatusCode::OK, "second owner lifts the guard");
    assert!(
        account::is_deleted(&pool, owner.id).await.unwrap(),
        "the owner becomes a reserved tombstone once another administrator remains"
    );
}

/// #48: the same lock-out protection covers the reversible verbs — a superuser
/// cannot suspend the only administrator either.
#[sqlx::test(migrations = "../db/migrations")]
async fn cannot_suspend_the_last_administrator(pool: PgPool) {
    let (boss, token) = user_with_scope(&pool, "boss", "read write admin:read admin:write").await;
    make_superuser(&pool, boss.id).await;
    let (owner, _) = user_with_scope(&pool, "owner", "read").await;
    make_owner(&pool, owner.id).await;

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", owner.id),
        Some(&token),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "last administrator protected"
    );
    assert!(
        !account::find_by_id(&pool, owner.id)
            .await
            .unwrap()
            .unwrap()
            .suspended(),
        "the last owner must not be suspended"
    );
}

/// #49: the audit log is append-only. Permanently purging a moderator must keep
/// every action they recorded, and the destroy itself is audited in the same
/// transaction as the data purge.
#[sqlx::test(migrations = "../db/migrations")]
async fn deleting_a_moderator_preserves_their_audit_history(pool: PgPool) {
    // A moderator performs a real, audited action.
    let (moddy, mtoken) =
        user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_moderator(&pool, moddy.id).await;
    let (vic, _) = user_with_scope(&pool, "vic", "read").await;
    let (suspended, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", vic.id),
        Some(&mtoken),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(suspended, StatusCode::OK);

    // An admin who outranks the moderator (and holds DELETE_USER_DATA)
    // suspends, then permanently purges them.
    let (boss, btoken) = user_with_scope(&pool, "boss", "read write admin:read admin:write").await;
    make_admin(&pool, boss.id).await;
    let (suspended, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/accounts/{}/action", moddy.id),
        Some(&btoken),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(suspended, StatusCode::OK);
    let (destroyed, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/admin/accounts/{}", moddy.id),
        Some(&btoken),
        None,
    )
    .await;
    assert_eq!(destroyed, StatusCode::OK);
    assert!(
        account::is_deleted(&pool, moddy.id).await.unwrap(),
        "the moderator is a permanent reserved tombstone"
    );

    let logs = plamenu_db::admin_action_log::list(
        &pool,
        &plamenu_db::admin_action_log::LogFilter {
            limit: 20,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The purged moderator's suspend action is preserved: the snapshotted
    // handle still displays. The reserved account tombstone keeps the actor
    // reference valid too.
    let suspend = logs
        .iter()
        .find(|log| log.action == "suspend" && log.human_identifier == "@vic")
        .expect("the deleted moderator's suspend entry is preserved");
    assert_eq!(
        suspend.account_username, "moddy",
        "the handle snapshot outlives the account"
    );
    assert_eq!(suspend.account_id, Some(moddy.id));
    assert_eq!(suspend.human_identifier, "@vic");

    // The destroy itself was recorded atomically with the purge, attributed to
    // the still-present admin.
    let destroy = logs
        .iter()
        .find(|log| log.action == "destroy")
        .expect("the destroy is audited in the same transaction as the delete");
    assert_eq!(destroy.account_username, "boss");
    assert_eq!(destroy.account_id, Some(boss.id));
    assert_eq!(destroy.human_identifier, "@moddy");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_origin_filter(pool: PgPool) {
    let (account, token) =
        user_with_scope(&pool, "moddy", "read write admin:read admin:write").await;
    make_admin(&pool, account.id).await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/admin/accounts?origin=local",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().unwrap();
    assert!(
        list.iter().all(|a| a["domain"].is_null()),
        "origin=local excludes remote accounts: {body}"
    );
}
