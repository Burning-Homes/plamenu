//! Notification filtering (M11): the per-account policy
//! (`/api/v{1,2}/notifications/policy`), policy-driven filtering of incoming
//! notifications, and the filtered-notification requests
//! (`/api/v1/notifications/requests`) with accept/dismiss.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{create_local_account, test_app};
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
    let (status, app) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "notif-filtering",
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

/// Alice (recipient) and bob (a sender alice does not follow), each with a
/// token. Returns `(alice_token, bob, bob_token)`.
async fn alice_and_stranger_bob(pool: &PgPool) -> (String, Account, String) {
    let (_alice, alice_token) = user_with_token(pool, "alice").await;
    let (bob, bob_token) = user_with_token(pool, "bob").await;
    (alice_token, bob, bob_token)
}

async fn bob_mentions_alice(pool: &PgPool, bob_token: &str) {
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(bob_token),
        Some(json!({ "status": "@alice hi there" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn direct_mentions(pool: &PgPool, sender_token: &str, recipient: &str, text: &str) {
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(sender_token),
        Some(json!({
            "status": format!("@{recipient} {text}"),
            "visibility": "direct",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn policy_v2_defaults_and_partial_update(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // Plamenu intentionally defaults unsolicited private mentions to normal
    // delivery so third-party clients without notification-request support
    // still show DMs. Limited accounts keep Mastodon's filtered default.
    let (status, policy) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["for_not_following"], "accept");
    assert_eq!(policy["for_private_mentions"], "accept");
    assert_eq!(policy["for_limited_accounts"], "filter");
    assert_eq!(policy["for_bots"], "accept");
    assert_eq!(policy["summary"]["pending_requests_count"], 0);

    // A partial PATCH only touches the supplied categories.
    let (status, updated) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&token),
        Some(json!({ "for_not_following": "drop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["for_not_following"], "drop");
    assert_eq!(updated["for_private_mentions"], "accept", "untouched");

    let (_, refetched) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(refetched["for_not_following"], "drop");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn policy_v1_shape_and_update(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    let (status, policy) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["filter_not_following"], false);
    assert_eq!(policy["filter_private_mentions"], false);
    assert!(policy["summary"].is_object());

    let (status, updated) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v1/notifications/policy",
        Some(&token),
        Some(json!({ "filter_not_following": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["filter_not_following"], true);

    // v2 should now read the same category as `filter`.
    let (_, v2) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(v2["for_not_following"], "filter");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_mentions_policy_accept_filter_drop(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (bob, bob_token) = user_with_token(&pool, "bob").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let (_dave, dave_token) = user_with_token(&pool, "dave").await;

    direct_mentions(&pool, &bob_token, "alice", "accept option").await;
    let (_, visible) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(visible.as_array().unwrap().len(), 1);
    assert_eq!(visible[0]["type"], "mention");
    assert_eq!(visible[0]["status"]["visibility"], "direct");
    assert_eq!(visible[0].get("filtered"), None);
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests, json!([]), "accept creates no request");

    let (status, policy) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_private_mentions": "filter" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["for_private_mentions"], "filter");
    direct_mentions(&pool, &carol_token, "alice", "filter option").await;
    let (_, default) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        default.as_array().unwrap().len(),
        1,
        "filtered DMs stay out of the default notification list"
    );
    let (_, filtered) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(filtered.as_array().unwrap().len(), 2);
    assert_eq!(filtered[0]["filtered"], true);
    assert_eq!(filtered[0]["account"]["id"], carol.id.to_string());
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["account"]["id"], carol.id.to_string());

    let (status, policy) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_private_mentions": "drop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["for_private_mentions"], "drop");
    direct_mentions(&pool, &dave_token, "alice", "drop option").await;
    let (_, after_drop) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        after_drop.as_array().unwrap().len(),
        2,
        "drop stores no notification"
    );
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        requests.as_array().unwrap().len(),
        1,
        "drop creates no request"
    );
    assert_eq!(visible[0]["account"]["id"], bob.id.to_string());

    // The drop action is category-agnostic: switching the not-following policy
    // to drop discards a fresh public mention from a non-followed sender the
    // same way — no stored row (even filtered) and no request.
    let (status, policy) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_not_following": "drop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["for_not_following"], "drop");
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&bob_token),
        Some(json!({ "status": "@alice public hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, after_not_following_drop) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        after_not_following_drop.as_array().unwrap().len(),
        2,
        "for_not_following drop stores no notification"
    );
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        requests.as_array().unwrap().len(),
        1,
        "for_not_following drop creates no request"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn filtered_mention_hidden_then_accepted(pool: PgPool) {
    let (alice_token, bob, bob_token) = alice_and_stranger_bob(&pool).await;
    // Filter every notification from non-followed senders.
    api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_not_following": "filter" })),
    )
    .await;

    bob_mentions_alice(&pool, &bob_token).await;

    // The default index hides it; include_filtered surfaces it as `filtered`.
    let (_, default) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(default, json!([]));
    let (_, shown) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown[0]["type"], "mention");
    assert_eq!(shown[0]["filtered"], true);

    // It is rolled up into a request from bob.
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["account"]["id"], bob.id.to_string());
    assert_eq!(requests[0]["notifications_count"], "1");
    let request_id = requests[0]["id"].as_str().unwrap().to_owned();

    // `merged` is always true — Plamenu unfilters synchronously.
    let (_, merged) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests/merged",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(merged["merged"], true);

    // Accepting unfilters the past notification and clears the request.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/notifications/requests/{request_id}/accept"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, default) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(default[0]["type"], "mention", "now visible");
    assert_eq!(default[0].get("filtered"), None);
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests, json!([]));

    // A later notification from the now-accepted sender bypasses the filter.
    bob_mentions_alice(&pool, &bob_token).await;
    let (_, default) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(default.as_array().unwrap().len(), 2, "both visible");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dismiss_purges_request_and_filtered_notifications(pool: PgPool) {
    let (alice_token, _bob, bob_token) = alice_and_stranger_bob(&pool).await;
    api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_not_following": "filter" })),
    )
    .await;
    bob_mentions_alice(&pool, &bob_token).await;

    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    let request_id = requests[0]["id"].as_str().unwrap().to_owned();

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/notifications/requests/{request_id}/dismiss"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The request is gone and the filtered notification is deleted outright.
    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests, json!([]));
    let (_, shown) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown, json!([]));
}

/// Accepting a sender backfills *every* filtered DM into the recipient's
/// conversation listing — including two DMs threaded into one conversation
/// (which the batched backfill folds into a single row) alongside a separate
/// second conversation.
#[sqlx::test(migrations = "../db/migrations")]
async fn accepting_backfills_every_filtered_dm_conversation(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_private_mentions": "filter" })),
    )
    .await;

    let (status, first) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({ "status": "@alice one", "visibility": "direct" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_id = first["id"].as_str().unwrap().to_owned();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({
            "status": "@alice two",
            "visibility": "direct",
            "in_reply_to_id": first_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({ "status": "@alice three", "visibility": "direct" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The local post path files conversation rows regardless of filtering;
    // drop alice's so the accept below has genuinely missing rows to backfill
    // (the inbound case the backfill exists for).
    sqlx::query("DELETE FROM account_conversations WHERE account_id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, conversations) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(conversations, json!([]));

    let (_, requests) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    let request_id = requests[0]["id"].as_str().unwrap().to_owned();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/notifications/requests/{request_id}/accept"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, conversations) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    let listed = conversations.as_array().unwrap();
    assert_eq!(
        listed.len(),
        2,
        "one threaded conversation plus one separate"
    );
    let last_texts: Vec<&str> = listed
        .iter()
        .map(|c| c["last_status"]["content"].as_str().unwrap())
        .collect();
    assert!(
        last_texts.iter().any(|text| text.contains("two")),
        "the threaded conversation surfaces its latest DM: {last_texts:?}"
    );
    assert!(
        last_texts.iter().any(|text| text.contains("three")),
        "the separate conversation is backfilled too: {last_texts:?}"
    );
    assert!(
        listed.iter().all(|c| c["unread"] == true),
        "backfilled sender DMs arrive unread"
    );
}

/// The bulk accept/dismiss endpoints reject an oversized `id`
/// array with 422 before dispatching per-request work.
#[sqlx::test(migrations = "../db/migrations")]
async fn bulk_request_ids_over_the_cap_are_rejected(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // 101 distinct ids — one past MAX_NOTIFICATION_REQUEST_IDS (100). No real
    // requests are needed: the cap fires while parsing the input.
    let ids: Vec<i64> = (1..=101).collect();
    for uri in [
        "/api/v1/notifications/requests/accept",
        "/api/v1/notifications/requests/dismiss",
    ] {
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            uri,
            Some(&token),
            Some(json!({ "id": ids })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
    }
}
