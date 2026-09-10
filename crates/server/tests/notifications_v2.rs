//! Grouped notifications (`/api/v2/notifications`) and the edit-driven
//! notification types (`update`, `quoted_update`): grouping, the dedup
//! payload shape, pagination windows, group dismissal/clearing, unread
//! group counts, the per-group accounts listing and `expand_accounts`.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let (status, app) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "notifications-v2",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let client_id = app["client_id"].as_str().unwrap().to_owned();
    let client_secret = app["client_secret"].as_str().unwrap().to_owned();
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
    let (_, token) = api(
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
    (account, token["access_token"].as_str().unwrap().to_owned())
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
    let (status, _, value) = api_with_headers(app, method, uri, bearer, body).await;
    (status, value)
}

async fn api_with_headers(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
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
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// Posts a status and returns its entity.
async fn post_status(pool: &PgPool, token: &str, body: Value) -> Value {
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    entity
}

async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, path, &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Seeds alice with a post favourited by bob, carol and dave (one group)
/// and a mention from bob (ungrouped). Returns
/// (alice, alice's token, the post id, [bob, carol, dave] account ids).
async fn seed_grouped(pool: &PgPool) -> (Account, String, String, [String; 3]) {
    let (alice, alice_token) = user_with_token(pool, "alice").await;
    let post = post_status(pool, &alice_token, json!({ "status": "group me" })).await;
    let post_id = post["id"].as_str().unwrap().to_owned();
    let mut faver_ids = Vec::new();
    let mut bob_token = String::new();
    for username in ["bob", "carol", "dave"] {
        let (account, token) = user_with_token(pool, username).await;
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/statuses/{post_id}/favourite"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        faver_ids.push(account.id.to_string());
        if username == "bob" {
            bob_token = token;
        }
    }
    // The mention is the newest notification, above the favourite group.
    post_status(pool, &bob_token, json!({ "status": "@alice hello" })).await;
    (
        alice,
        alice_token,
        post_id,
        [
            faver_ids[0].clone(),
            faver_ids[1].clone(),
            faver_ids[2].clone(),
        ],
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn grouped_index_collapses_favourites_into_one_group(pool: PgPool) {
    let (_alice, alice_token, post_id, [bob_id, carol_id, dave_id]) = seed_grouped(&pool).await;

    let (status, headers, body) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let groups = body["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "mention + one favourite group: {body}");

    // Newest first: bob's mention (ungrouped), then the favourite group.
    let mention = &groups[0];
    assert_eq!(mention["type"], "mention");
    assert_eq!(mention["notifications_count"], 1);
    assert!(
        mention["group_key"]
            .as_str()
            .unwrap()
            .starts_with("ungrouped-")
    );
    assert_eq!(mention["sample_account_ids"], json!([bob_id]));

    let favs = &groups[1];
    assert_eq!(favs["type"], "favourite");
    assert_eq!(favs["notifications_count"], 3);
    let group_key = favs["group_key"].as_str().unwrap();
    assert!(
        group_key.starts_with(&format!("favourite-{post_id}-")),
        "Mastodon-style key: {group_key}"
    );
    // Sample senders are newest-first and capped at 8.
    assert_eq!(
        favs["sample_account_ids"],
        json!([dave_id, carol_id, bob_id])
    );
    assert_eq!(favs["status_id"], json!(post_id));
    // Page fields: strings, anchored to the group's notification ids.
    assert!(favs["most_recent_notification_id"].is_number());
    assert_eq!(
        favs["page_max_id"].as_str().unwrap(),
        favs["most_recent_notification_id"].to_string()
    );
    assert!(favs["page_min_id"].is_string());
    assert!(favs["latest_page_notification_at"].is_string());

    // Dedup payload: each account once, each status once.
    let account_ids: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        account_ids,
        [bob_id.as_str(), dave_id.as_str(), carol_id.as_str()]
    );
    let status_ids: Vec<&str> = body["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(status_ids.len(), 2, "mention status + favourited status");
    assert!(status_ids.contains(&post_id.as_str()));
    // No partial accounts without expand_accounts=partial_avatars.
    assert!(body.get("partial_accounts").is_none());

    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    assert!(link.contains("/api/v2/notifications?"), "{link}");
    assert!(
        link.contains("rel=\"next\"") && link.contains("rel=\"prev\""),
        "{link}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn grouped_types_param_limits_which_kinds_group(pool: PgPool) {
    let (_alice, alice_token, _post_id, _favers) = seed_grouped(&pool).await;

    // Only reblogs group: the three favourites fall apart into singles.
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications?grouped_types%5B%5D=reblog",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let groups = body["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 4, "mention + three ungrouped favourites");
    assert!(groups.iter().skip(1).all(|g| g["type"] == "favourite"
        && g["notifications_count"] == 1
        && g["group_key"].as_str().unwrap().starts_with("ungrouped-")));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn grouped_pagination_walks_one_group_per_page(pool: PgPool) {
    let (_alice, alice_token, _post_id, _favers) = seed_grouped(&pool).await;

    let (_, first_page) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications?limit=1",
        Some(&alice_token),
        None,
    )
    .await;
    let groups = first_page["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["type"], "mention");
    let mention_id: i64 = groups[0]["page_max_id"].as_str().unwrap().parse().unwrap();

    // The next page by max_id holds the favourite group — and although the
    // page is below the mention, the group still aggregates all members.
    let (_, second_page) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications?limit=1&max_id={mention_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    let groups = second_page["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["type"], "favourite");
    assert_eq!(groups[0]["notifications_count"], 3);

    // Walking upward from below returns the same group page.
    let oldest: i64 = groups[0]["page_min_id"].as_str().unwrap().parse().unwrap();
    let (_, upward) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications?limit=1&min_id={}", oldest - 1),
        Some(&alice_token),
        None,
    )
    .await;
    let groups = upward["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["type"], "favourite");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn show_dismiss_and_clear_groups(pool: PgPool) {
    let (_alice, alice_token, _post_id, _favers) = seed_grouped(&pool).await;

    let (_, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let groups = index["notification_groups"].as_array().unwrap();
    let mention_key = groups[0]["group_key"].as_str().unwrap();
    let fav_key = groups[1]["group_key"].as_str().unwrap();

    // Show: full totals, but no page fields outside a paginated listing.
    let (status, shown) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications/{fav_key}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let group = &shown["notification_groups"][0];
    assert_eq!(group["group_key"].as_str().unwrap(), fav_key);
    assert_eq!(group["notifications_count"], 3);
    assert!(group.get("page_min_id").is_none());
    assert!(group.get("page_max_id").is_none());
    assert_eq!(shown["accounts"].as_array().unwrap().len(), 3);

    // Show by a synthetic ungrouped key resolves by notification id.
    let (status, shown) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications/{mention_key}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown["notification_groups"][0]["type"], "mention");

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/favourite-1-2",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Dismissing the group removes every member; unknown keys are a no-op.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/notifications/{fav_key}/dismiss"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/notifications/favourite-1-2/dismiss",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, v1) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(v1.as_array().unwrap().len(), 1, "only the mention is left");

    // v2 clear wipes the rest.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/notifications/clear",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(index["notification_groups"], json!([]));
    assert_eq!(index["accounts"], json!([]));
    assert_eq!(index["statuses"], json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unread_count_counts_groups_not_notifications(pool: PgPool) {
    let (_alice, alice_token, _post_id, _favers) = seed_grouped(&pool).await;

    // Four notifications, two groups.
    let (_, v1_count) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(v1_count["count"], 4);
    let (_, v2_count) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(v2_count["count"], 2);

    // Type narrowing applies.
    let (_, narrowed) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/unread_count?types%5B%5D=mention",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(narrowed["count"], 1);

    // Reading everything (marker at the newest notification) zeroes it.
    let (_, v1) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?limit=1",
        Some(&alice_token),
        None,
    )
    .await;
    let newest_id = v1[0]["id"].as_str().unwrap();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/markers",
        Some(&alice_token),
        Some(json!({ "notifications": { "last_read_id": newest_id } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, v2_count) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(v2_count["count"], 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_accounts_lists_every_sender_paginated(pool: PgPool) {
    let (_alice, alice_token, _post_id, [bob_id, carol_id, dave_id]) = seed_grouped(&pool).await;

    let (_, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let groups = index["notification_groups"].as_array().unwrap();
    let mention_key = groups[0]["group_key"].as_str().unwrap();
    let fav_key = groups[1]["group_key"].as_str().unwrap();

    let (status, headers, accounts) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications/{fav_key}/accounts?limit=2"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = accounts
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [dave_id.as_str(), carol_id.as_str()]);
    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    assert!(
        link.contains("rel=\"next\""),
        "full page links next: {link}"
    );
    let next_max: i64 = link
        .split("max_id=")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let (_, rest) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications/{fav_key}/accounts?limit=2&max_id={next_max}"),
        Some(&alice_token),
        None,
    )
    .await;
    let ids: Vec<&str> = rest
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [bob_id.as_str()]);

    // Synthetic ungrouped keys list nothing, like Mastodon.
    let (status, accounts) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/notifications/{mention_key}/accounts"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accounts, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn expand_accounts_partial_avatars_splits_the_account_list(pool: PgPool) {
    let (_alice, alice_token, _post_id, [bob_id, carol_id, dave_id]) = seed_grouped(&pool).await;

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications?expand_accounts=partial_avatars",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Full entities: each group's most recent sender only.
    let full: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(full, [bob_id.as_str(), dave_id.as_str()]);
    // The remaining samples arrive as partial entities.
    let partial = body["partial_accounts"].as_array().unwrap();
    let partial_ids: Vec<&str> = partial.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(partial_ids, [carol_id.as_str()]);
    assert!(partial[0].get("avatar").is_some());
    assert!(
        partial[0].get("display_name").is_none(),
        "partial entities carry the reduced field set"
    );

    // Anything else is Mastodon's invalid-parameter 400.
    let (status, error) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications?expand_accounts=bogus",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error["error"].as_str().unwrap().contains("expand_accounts"),
        "{error}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v1_notifications_carry_group_keys(pool: PgPool) {
    let (_alice, alice_token, post_id, _favers) = seed_grouped(&pool).await;

    let (_, v1) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let items = v1.as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert!(
        items[0]["group_key"]
            .as_str()
            .unwrap()
            .starts_with("ungrouped-"),
        "mentions never group"
    );
    let fav_keys: Vec<&str> = items
        .iter()
        .filter(|n| n["type"] == "favourite")
        .map(|n| n["group_key"].as_str().unwrap())
        .collect();
    assert_eq!(fav_keys.len(), 3);
    assert!(fav_keys.iter().all(|k| k == &fav_keys[0]));
    assert!(fav_keys[0].starts_with(&format!("favourite-{post_id}-")));
}

// ---------------------------------------------------------------------------
// Edit-driven notifications: `update` and `quoted_update`

#[sqlx::test(migrations = "../db/migrations")]
async fn editing_a_status_notifies_rebloggers_and_quoters(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;

    let post = post_status(&pool, &alice_token, json!({ "status": "v1 of this post" })).await;
    let post_id = post["id"].as_str().unwrap();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/reblog"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let quote = post_status(
        &pool,
        &carol_token,
        json!({ "status": "look at this", "quoted_status_id": post_id }),
    )
    .await;

    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/statuses/{post_id}"),
        Some(&alice_token),
        Some(json!({ "status": "v2 of this post" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bob (the booster) hears about the edit, targeting the edited post.
    let (_, bob_notifications) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=update",
        Some(&bob_token),
        None,
    )
    .await;
    let items = bob_notifications.as_array().unwrap();
    assert_eq!(items.len(), 1, "{bob_notifications}");
    assert_eq!(items[0]["type"], "update");
    assert_eq!(items[0]["status"]["id"], json!(post_id));
    assert_eq!(
        items[0]["status"]["content"].as_str().unwrap(),
        "<p>v2 of this post</p>"
    );

    // Carol (the quoter) gets quoted_update targeting her own quote post.
    let (_, carol_notifications) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=quoted_update",
        Some(&carol_token),
        None,
    )
    .await;
    let items = carol_notifications.as_array().unwrap();
    assert_eq!(items.len(), 1, "{carol_notifications}");
    assert_eq!(items[0]["type"], "quoted_update");
    assert_eq!(items[0]["status"]["id"], quote["id"]);

    // Alice edited her own post; she never notifies herself.
    let (_, alice_notifications) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=update&types%5B%5D=quoted_update",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(alice_notifications, json!([]));

    // A no-change edit is a no-op: nobody hears about it again.
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/statuses/{post_id}"),
        Some(&alice_token),
        Some(json!({ "status": "v2 of this post" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, bob_notifications) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=update",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(bob_notifications.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_edit_notifies_local_rebloggers(pool: PgPool) {
    let remote_bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&remote_bob]);
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;

    let note_uri = "https://remote.example/users/bob/statuses/42";
    let note = |content: &str| {
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": remote_bob.actor.id,
            "content": content,
            "published": "2026-06-10T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{note_uri}/activity"),
            "type": "Create",
            "actor": remote_bob.actor.id,
            "object": note("<p>original</p>"),
        }),
        &remote_bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    // Alice boosts the remote post.
    let stored = plamenu_db::status::find_by_uri(&pool, note_uri)
        .await
        .unwrap()
        .unwrap();
    let (status, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{}/reblog", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bob edits; alice (a booster) gets an `update` notification.
    let mut edited = note("<p>edited</p>");
    edited["updated"] = json!("2026-06-10T13:00:00Z");
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/1"),
        "type": "Update",
        "actor": remote_bob.actor.id,
        "object": edited,
    });
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &update,
        &remote_bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    let (_, notifications) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=update",
        Some(&alice_token),
        None,
    )
    .await;
    let items = notifications.as_array().unwrap();
    assert_eq!(items.len(), 1, "{notifications}");
    assert_eq!(items[0]["status"]["id"], json!(stored.id.to_string()));

    // Redelivering the same Update changes nothing — no duplicate.
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &update,
        &remote_bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let (_, notifications) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=update",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);
}
