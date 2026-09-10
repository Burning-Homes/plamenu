//! Locked accounts and follow requests: the `locked` flag end to end
//! (`update_credentials`, actor document), inbound follows held for review,
//! and the authorize / reject endpoints with their federated responses.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::delivery;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, job, user};
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
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "follow-requests",
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

/// Form-urlencoded PATCH (the Rails-style body clients also send).
async fn patch_form(app: Router, uri: &str, bearer: &str, body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Fetches an `ActivityPub` document (correct Accept header).
async fn get_ap(app: Router, path: &str) -> Value {
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

fn follow_activity(bob: &RemoteUser, marker: u32) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/{marker}", bob.actor.id),
        "type": "Follow",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice",
    })
}

/// Signs `body` as a POST and sends it through the router.
async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, path, &bytes, std::time::SystemTime::now());
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

#[sqlx::test(migrations = "../db/migrations")]
async fn locked_flag_updates_entities_and_actor_document(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // Unlocked by default.
    let actor = get_ap(app(), "/users/alice").await;
    assert_eq!(actor["manuallyApprovesFollowers"], false);

    // JSON body, boolean value.
    let (status, entity) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "locked": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity:?}");
    assert_eq!(entity["locked"], true);
    assert_eq!(entity["source"]["follow_requests_count"], 0);
    let actor = get_ap(app(), "/users/alice").await;
    assert_eq!(actor["manuallyApprovesFollowers"], true);

    // Form body, string value — and the flag survives unrelated updates.
    let (status, entity) = patch_form(
        app(),
        "/api/v1/accounts/update_credentials",
        &token,
        "display_name=Alice+Locked",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entity["locked"], true);
    let (status, entity) = patch_form(
        app(),
        "/api/v1/accounts/update_credentials",
        &token,
        "locked=false",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entity["locked"], false);
    let actor = get_ap(app(), "/users/alice").await;
    assert_eq!(actor["manuallyApprovesFollowers"], false);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_follow_to_locked_account_waits_for_authorization(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let (status, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "locked": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let status = post_signed(
        app(),
        "/users/alice/inbox",
        &follow_activity(&bob, 1),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Pending, no Accept queued, follower count untouched.
    let stored_bob = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, stored_bob.id, alice.id)
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(delivery::run_due(&state).await, 0);
    assert_eq!(follow::count_followers(&pool, alice.id).await.unwrap(), 0);

    // The follow_request notification and listing appear.
    let (_, notifications) = api(app(), "GET", "/api/v1/notifications", Some(&token), None).await;
    assert_eq!(notifications[0]["type"], "follow_request");
    assert_eq!(notifications[0]["account"]["id"], stored_bob.id.to_string());
    let (_, requests) = api(app(), "GET", "/api/v1/follow_requests", Some(&token), None).await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["id"], stored_bob.id.to_string());
    let (_, me) = api(
        app(),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(me["source"]["follow_requests_count"], 1);
    let (_, rels) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={}", stored_bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(rels[0]["requested_by"], true);
    assert_eq!(rels[0]["followed_by"], false);

    // A re-sent Follow refreshes the request URI without re-notifying.
    let status = post_signed(
        app(),
        "/users/alice/inbox",
        &follow_activity(&bob, 2),
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let edge = follow::find(&pool, stored_bob.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge.pending);
    assert_eq!(
        edge.uri.as_deref(),
        Some(format!("{}/follows/2", bob.actor.id).as_str())
    );
    let (_, notifications) = api(app(), "GET", "/api/v1/notifications", Some(&token), None).await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);
    assert_eq!(delivery::run_due(&state).await, 0);

    // Authorize: the follow becomes real and the Accept goes out.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/follow_requests/{}/authorize", stored_bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["followed_by"], true);
    assert_eq!(rel["requested_by"], false);
    assert_eq!(
        follow::pending_state(&pool, stored_bob.id, alice.id)
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(follow::count_followers(&pool, alice.id).await.unwrap(), 1);
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    assert_eq!(sent[0].activity["type"], "Accept");
    assert_eq!(
        sent[0].activity["actor"],
        "https://plamenu.test/users/alice"
    );
    assert_eq!(sent[0].activity["object"]["type"], "Follow");
    assert_eq!(
        sent[0].activity["object"]["id"],
        format!("{}/follows/2", bob.actor.id)
    );
    assert_eq!(sent[0].activity["object"]["actor"], bob.actor.id);

    // The follow_request notification became a follow one.
    let (_, notifications) = api(app(), "GET", "/api/v1/notifications", Some(&token), None).await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);
    assert_eq!(notifications[0]["type"], "follow");
    let (_, requests) = api(app(), "GET", "/api/v1/follow_requests", Some(&token), None).await;
    assert!(requests.as_array().unwrap().is_empty());

    // Authorizing again (or rejecting) is Mastodon's 404.
    for action in ["authorize", "reject"] {
        let (status, body) = api(
            app(),
            "POST",
            &format!("/api/v1/follow_requests/{}/{action}", stored_bob.id),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "Record not found");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rejecting_a_follow_request_federates_reject(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "locked": true })),
    )
    .await;
    post_signed(
        app(),
        "/users/alice/inbox",
        &follow_activity(&bob, 7),
        &bob.signer(),
    )
    .await;
    let stored_bob = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();

    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/follow_requests/{}/reject", stored_bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["requested_by"], false);
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );

    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent[0].activity["type"], "Reject");
    assert_eq!(sent[0].activity["object"]["type"], "Follow");
    assert_eq!(
        sent[0].activity["object"]["id"],
        format!("{}/follows/7", bob.actor.id)
    );

    // The request notification disappeared with the request.
    let (_, notifications) = api(app(), "GET", "/api/v1/notifications", Some(&token), None).await;
    assert!(notifications.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn repeat_follow_when_already_following_is_re_accepted(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    // Bob follows while alice is unlocked: instant Accept.
    post_signed(
        app(),
        "/users/alice/inbox",
        &follow_activity(&bob, 1),
        &bob.signer(),
    )
    .await;
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);

    // Alice locks (directly in the db — the API path would also fan out an
    // Update(Actor) to bob, muddying the delivery assertions below); bob's
    // re-sent Follow fast-forwards to a fresh Accept instead of degrading
    // the existing follow to a request.
    plamenu_db::account::update_local_profile(
        &pool,
        alice.id,
        plamenu_db::account::ProfileUpdate {
            locked: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    post_signed(
        app(),
        "/users/alice/inbox",
        &follow_activity(&bob, 2),
        &bob.signer(),
    )
    .await;
    let stored_bob = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, stored_bob.id, alice.id)
            .await
            .unwrap(),
        Some(false)
    );
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].activity["type"], "Accept");
    assert_eq!(
        sent[1].activity["object"]["id"],
        format!("{}/follows/2", bob.actor.id)
    );

    // No second notification for the repeat.
    let (_, notifications) = api(app(), "GET", "/api/v1/notifications", Some(&token), None).await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);
    assert_eq!(notifications[0]["type"], "follow");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_follow_of_locked_account_needs_authorization(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&alice_token),
        Some(json!({ "locked": true })),
    )
    .await;

    // Carol's follow turns into a request — no optimistic `following`.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["following"], false);
    assert_eq!(rel["requested"], true);
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications[0]["type"], "follow_request");

    // Withdrawing the request clears it (and the notification).
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unfollow", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["requested"], false);
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert!(notifications.as_array().unwrap().is_empty());

    // Re-request and authorize: carol now follows.
    api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/follow_requests/{}/authorize", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["followed_by"], true);
    assert_eq!(
        follow::pending_state(&pool, carol.id, alice.id)
            .await
            .unwrap(),
        Some(false)
    );
    let (_, rels) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={}", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(rels[0]["following"], true);
    assert_eq!(rels[0]["requested"], false);
    // Alice's notification matured into a follow.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);
    assert_eq!(notifications[0]["type"], "follow");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_request_endpoints_require_auth(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let app = || test_app(pool.clone());

    let (status, _) = api(app(), "GET", "/api/v1/follow_requests", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for action in ["authorize", "reject"] {
        let (status, _) = api(
            app(),
            "POST",
            &format!("/api/v1/follow_requests/{}/{action}", alice.id),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // Unknown requester ids are a 404 even when authenticated.
    let (_, token) = user_with_token(&pool, "carol").await;
    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/follow_requests/999/authorize",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}
