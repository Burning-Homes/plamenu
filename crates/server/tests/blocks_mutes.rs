//! Blocking and muting: API endpoints, timeline/notification enforcement,
//! and Block federation in both directions.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, pin_emissions_off, test_app,
    test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{delivery, remote, severance};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, block, follow, job, notification, status, user};
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
            "client_name": "blocks-mutes",
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

async fn post_signed(app: Router, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

async fn relationship(app: Router, token: &str, target_id: i64) -> Value {
    let (status, body) = api(
        app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={target_id}"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body[0].clone()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn block_severs_relationship_and_hides_content(pool: PgPool) {
    common::open_previews(&pool).await;
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    // Mutual follows, a post each, and a favourite to seed a notification.
    for (token, target) in [(&alice_token, carol.id), (&carol_token, alice.id)] {
        let (status, _) = api(
            app(),
            "POST",
            &format!("/api/v1/accounts/{target}/follow"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, alice_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "hello from alice"})),
    )
    .await;
    let alice_status_id = alice_post["id"].as_str().unwrap().to_owned();
    let (_, carol_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "hello from carol"})),
    )
    .await;
    let carol_status_id = carol_post["id"].as_str().unwrap().to_owned();
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{alice_status_id}/favourite"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Alice blocks carol.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/block", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["blocking"], true);
    assert_eq!(rel["following"], false);
    assert_eq!(rel["followed_by"], false);
    assert!(!follow::exists(&pool, alice.id, carol.id).await.unwrap());
    assert!(!follow::exists(&pool, carol.id, alice.id).await.unwrap());

    // Carol sees blocked_by and cannot re-follow, favourite or even view.
    let rel = relationship(app(), &carol_token, alice.id).await;
    assert_eq!(rel["blocked_by"], true);
    assert_eq!(rel["blocking"], false);
    let (status, body) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "This action is not allowed");
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{alice_status_id}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "author blocks the viewer");
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{alice_status_id}/favourite"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, listing) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing, json!([]), "blocked viewers get an empty listing");

    // Alice can still look at carol's post deliberately, but not interact.
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{carol_status_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{carol_status_id}/reblog"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "This action is not allowed");

    // Carol's favourite notification was erased; the public timeline hides
    // carol from alice (but not from anonymous viewers).
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));
    let (_, timeline) = api(
        app(),
        "GET",
        "/api/v1/timelines/public",
        Some(&alice_token),
        None,
    )
    .await;
    let authors: Vec<&str> = timeline
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["account"]["username"].as_str().unwrap())
        .collect();
    assert_eq!(authors, ["alice"]);
    let (_, timeline) = api(app(), "GET", "/api/v1/timelines/public", None, None).await;
    assert_eq!(timeline.as_array().unwrap().len(), 2);

    // The blocks listing pages carol; unblock restores a clean slate.
    let (status, blocked) = api(app(), "GET", "/api/v1/blocks", Some(&alice_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(blocked[0]["username"], "carol");
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unblock", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["blocking"], false);
    let (_, blocked) = api(app(), "GET", "/api/v1/blocks", Some(&alice_token), None).await;
    assert_eq!(blocked, json!([]));
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{alice_status_id}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unblock restores visibility");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mute_hides_content_and_optionally_notifications(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, carol_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "carol speaks"})),
    )
    .await;
    let carol_status_id = carol_post["id"].as_str().unwrap().to_owned();
    let (_, alice_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "alice speaks"})),
    )
    .await;
    let alice_status_id = alice_post["id"].as_str().unwrap().to_owned();

    // Mute with default parameters: content and notifications both hidden.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/mute", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["muting"], true);
    assert_eq!(rel["muting_notifications"], true);
    assert_eq!(rel["muting_expires_at"], Value::Null);
    assert_eq!(rel["following"], true, "muting does not unfollow");

    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    let contents: Vec<&str> = home
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["account"]["username"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["alice"], "muted author leaves the home timeline");

    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{alice_status_id}/favourite"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        notifications,
        json!([]),
        "hide_notifications mutes the bell"
    );
    let (_, unread) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(unread["count"], 0);

    // Re-muting with notifications=false keeps content hidden but lets the
    // notification back through; a duration sets the expiry.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/mute", carol.id),
        Some(&alice_token),
        Some(json!({"notifications": false, "duration": 3600})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["muting"], true);
    assert_eq!(rel["muting_notifications"], false);
    assert!(rel["muting_expires_at"].is_string());
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications[0]["type"], "favourite");
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(home.as_array().unwrap().len(), 1, "content stays hidden");

    // The muted author's posts are still directly viewable (unlike a block).
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{carol_status_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Listing + unmute.
    let (_, muted) = api(app(), "GET", "/api/v1/mutes", Some(&alice_token), None).await;
    assert_eq!(muted[0]["username"], "carol");
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unmute", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["muting"], false);
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        home.as_array().unwrap().len(),
        2,
        "unmute restores the feed"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn domain_block_hides_domain_and_severs_relationships(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let dave = RemoteUser::new("other.example", "dave");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stored_carol = remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    let stored_dave = remote::store_remote_actor(&pool, &dave.actor)
        .await
        .unwrap();
    let stub = StubFederation::with_users(&[&bob, &carol, &dave]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    follow::create_outgoing(
        &pool,
        alice.id,
        stored_bob.id,
        "https://plamenu.test/users/alice#follows/bob",
    )
    .await
    .unwrap();
    follow::mark_accepted(&pool, alice.id, stored_bob.id)
        .await
        .unwrap();
    follow::create(
        &pool,
        stored_bob.id,
        alice.id,
        Some("https://remote.example/users/bob#follows/alice"),
    )
    .await
    .unwrap();
    follow::create_request(
        &pool,
        stored_carol.id,
        alice.id,
        Some("https://remote.example/users/carol#follows/alice"),
    )
    .await
    .unwrap();
    follow::create(&pool, stored_dave.id, alice.id, None)
        .await
        .unwrap();
    notification::create(&pool, alice.id, stored_bob.id, "follow", None)
        .await
        .unwrap();

    let bob_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: stored_bob.id,
            content: "<p>bob speaks</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    let (status, preview) = api(
        app(),
        "GET",
        "/api/v1/domain_blocks/preview?domain=REMOTE.example",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["following_count"], 1);
    assert_eq!(preview["followers_count"], 1);

    // A malformed domain is a 422 and blocks nothing.
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/domain_blocks",
        Some(&alice_token),
        Some(json!({"domain": "example com"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = api(
        app(),
        "POST",
        "/api/v1/domain_blocks",
        Some(&alice_token),
        Some(json!({"domain": "remote.example"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body, json!({}));

    // The block row and its relationship severance are decoupled (N+1
    // close-out): the request returns with the edges still present and
    // the queued job severs them — Mastodon's semantics.
    assert!(
        follow::exists(&pool, alice.id, stored_bob.id)
            .await
            .unwrap()
    );
    assert_eq!(severance::run_due(&state).await, 1);

    assert!(
        !follow::exists(&pool, alice.id, stored_bob.id)
            .await
            .unwrap()
    );
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );
    assert!(
        !follow::exists(&pool, stored_carol.id, alice.id)
            .await
            .unwrap()
    );
    assert!(
        follow::exists(&pool, stored_dave.id, alice.id)
            .await
            .unwrap(),
        "other domains are untouched"
    );

    assert_eq!(delivery::run_due(&state).await, 3);
    // The three severance deliveries have no guaranteed claim order —
    // compare as a set (asserting positions flaked under parallel tests).
    let mut kinds: Vec<String> = stub
        .deliveries()
        .iter()
        .map(|sent| sent.activity["type"].as_str().unwrap().to_owned())
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["Reject", "Reject", "Undo"]);

    let (status, domains) = api(
        app(),
        "GET",
        "/api/v1/domain_blocks",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(domains, json!(["remote.example"]));

    let rel = relationship(app(), &alice_token, stored_bob.id).await;
    assert_eq!(rel["domain_blocking"], true);
    assert_eq!(rel["following"], false);
    assert_eq!(rel["followed_by"], false);

    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));

    let (_, timeline) = api(
        app(),
        "GET",
        "/api/v1/timelines/public",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(timeline, json!([]), "blocked domains leave timelines");

    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", bob_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "deliberate status views still work");
    let (status, body) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{}/favourite", bob_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "This action is not allowed");
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let follow_activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/users/bob#follows/alice-2",
        "type": "Follow",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice",
    });
    let status = post_signed(app(), &follow_activity, &bob.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent[3].activity["type"], "Reject");

    let (status, body) = api(
        app(),
        "DELETE",
        "/api/v1/domain_blocks?domain=remote.example",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rel = relationship(app(), &alice_token, stored_bob.id).await;
    assert_eq!(rel["domain_blocking"], false);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn blocking_a_remote_account_federates_block_and_severs_follows(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    // Mutual follows: ours accepted, bob's stored with its activity uri.
    follow::create_outgoing(
        &pool,
        alice.id,
        stored_bob.id,
        "https://plamenu.test/users/alice#follows/1",
    )
    .await
    .unwrap();
    follow::mark_accepted(&pool, alice.id, stored_bob.id)
        .await
        .unwrap();
    follow::create(
        &pool,
        stored_bob.id,
        alice.id,
        Some("https://remote.example/follows/77"),
    )
    .await
    .unwrap();

    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/block", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["blocking"], true);
    assert!(
        !follow::exists(&pool, alice.id, stored_bob.id)
            .await
            .unwrap()
    );
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );

    // Three activities reach bob: Undo(Follow), Reject(Follow), Block.
    assert_eq!(delivery::run_due(&state).await, 3);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|d| d.inbox_url == bob.actor.inbox));
    assert_eq!(sent[0].activity["type"], "Undo");
    assert_eq!(sent[0].activity["object"]["type"], "Follow");
    assert_eq!(
        sent[0].activity["object"]["id"],
        "https://plamenu.test/users/alice#follows/1"
    );
    assert_eq!(sent[1].activity["type"], "Reject");
    assert_eq!(
        sent[1].activity["object"]["id"],
        "https://remote.example/follows/77"
    );
    assert_eq!(sent[1].activity["object"]["actor"], bob.actor.id);
    assert_eq!(
        sent[1].activity["object"]["object"],
        "https://plamenu.test/users/alice"
    );
    assert_eq!(sent[2].activity["type"], "Block");
    assert_eq!(
        sent[2].activity["actor"],
        "https://plamenu.test/users/alice"
    );
    assert_eq!(sent[2].activity["object"], bob.actor.id);
    let block_uri = sent[2].activity["id"].as_str().unwrap().to_owned();
    assert!(block_uri.contains("#blocks/"));

    // A new Follow from bob while blocked is auto-rejected, never stored.
    let follow_activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/follows/78",
        "type": "Follow",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice",
    });
    let status = post_signed(app(), &follow_activity, &bob.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent[3].activity["type"], "Reject");
    assert_eq!(
        sent[3].activity["object"]["id"],
        "https://remote.example/follows/78"
    );

    // Unblock federates Undo(Block) referencing the original Block id.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unblock", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["blocking"], false);
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent[4].activity["type"], "Undo");
    assert_eq!(sent[4].activity["object"]["type"], "Block");
    assert_eq!(sent[4].activity["object"]["id"], block_uri);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_block_severs_follows_and_undo_restores_visibility(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_users(&[&bob]);
    let app = || test_app_with(pool.clone(), stub.clone());

    follow::create_outgoing(
        &pool,
        alice.id,
        stored_bob.id,
        "https://plamenu.test/users/alice#follows/1",
    )
    .await
    .unwrap();
    follow::mark_accepted(&pool, alice.id, stored_bob.id)
        .await
        .unwrap();
    follow::create(&pool, stored_bob.id, alice.id, None)
        .await
        .unwrap();
    let bob_status = plamenu_db::status::upsert_remote(
        &pool,
        plamenu_db::status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: stored_bob.id,
            content: "<p>bob speaks</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();

    let block_activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/users/bob#blocks/9",
        "type": "Block",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice",
    });
    let status = post_signed(app(), &block_activity, &bob.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    assert!(
        !follow::exists(&pool, alice.id, stored_bob.id)
            .await
            .unwrap()
    );
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );
    assert!(block::exists(&pool, stored_bob.id, alice.id).await.unwrap());
    let rel = relationship(app(), &alice_token, stored_bob.id).await;
    assert_eq!(rel["blocked_by"], true);
    assert_eq!(rel["following"], false);
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", bob_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "blocker's posts are hidden");

    // Undo(Block) lifts it.
    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/users/bob#blocks/9/undo",
        "type": "Undo",
        "actor": bob.actor.id,
        "object": block_activity,
    });
    let status = post_signed(app(), &undo, &bob.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(!block::exists(&pool, stored_bob.id, alice.id).await.unwrap());
    let rel = relationship(app(), &alice_token, stored_bob.id).await;
    assert_eq!(rel["blocked_by"], false);
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", bob_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// N+1 close-out parity: the set-based severance job produces exactly the
/// rows and queued payloads of the retired per-edge pipeline
/// (`unfollow_account` / `remove_from_followers` per relationship) on mixed
/// fixtures — accepted and pending edges in both directions, an edge stored
/// without a URI (nothing to retract by reference, only deleted), and an
/// untouched edge on another domain. The expected activities are hand-written
/// against the old pipeline's wire shapes, not rebuilt with the same helpers.
#[sqlx::test(migrations = "../db/migrations")]
async fn domain_severance_batch_matches_the_per_edge_pipeline(pool: PgPool) {
    // This test compares the underlying severance activity byte-for-byte.
    // Object Integrity Proof emission is covered independently and would add
    // a deliberately time-varying signature to otherwise identical payloads.
    pin_emissions_off(&pool).await;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let cut = |name: &str| RemoteUser::new("cut.example", name);
    let (bob, carol, dan, erin, gale) = (
        cut("bob"),
        cut("carol"),
        cut("dan"),
        cut("erin"),
        cut("gale"),
    );
    let frank = RemoteUser::new("keep.example", "frank");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stored_carol = remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    let stored_dan = remote::store_remote_actor(&pool, &dan.actor).await.unwrap();
    let stored_erin = remote::store_remote_actor(&pool, &erin.actor)
        .await
        .unwrap();
    let stored_gale = remote::store_remote_actor(&pool, &gale.actor)
        .await
        .unwrap();
    let stored_frank = remote::store_remote_actor(&pool, &frank.actor)
        .await
        .unwrap();
    let stub: std::sync::Arc<StubFederation> = std::sync::Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());

    // Outgoing: accepted (bob) and still-pending (carol), each with its
    // stored activity URI.
    let alice_uri = format!("https://{TEST_DOMAIN}/users/alice");
    follow::create_outgoing(
        &pool,
        alice.id,
        stored_bob.id,
        &format!("{alice_uri}#follows/101"),
    )
    .await
    .unwrap();
    follow::mark_accepted(&pool, alice.id, stored_bob.id)
        .await
        .unwrap();
    follow::create_outgoing(
        &pool,
        alice.id,
        stored_carol.id,
        &format!("{alice_uri}#follows/102"),
    )
    .await
    .unwrap();
    // Incoming: accepted (dan), pending request (erin), and one stored
    // without a URI (gale).
    follow::create(
        &pool,
        stored_dan.id,
        alice.id,
        Some("https://cut.example/users/dan#follows/alice"),
    )
    .await
    .unwrap();
    follow::create_request(
        &pool,
        stored_erin.id,
        alice.id,
        Some("https://cut.example/users/erin#follows/alice"),
    )
    .await
    .unwrap();
    follow::create(&pool, stored_gale.id, alice.id, None)
        .await
        .unwrap();
    // Another domain, both directions: must survive.
    follow::create_outgoing(
        &pool,
        alice.id,
        stored_frank.id,
        &format!("{alice_uri}#follows/103"),
    )
    .await
    .unwrap();
    follow::create(&pool, stored_frank.id, alice.id, None)
        .await
        .unwrap();

    plamenu::actions::block_domain(&state, &alice, "cut.example")
        .await
        .unwrap();
    assert_eq!(severance::run_due(&state).await, 1);

    for severed in [stored_bob.id, stored_carol.id] {
        assert!(!follow::exists(&pool, alice.id, severed).await.unwrap());
    }
    for severed in [stored_dan.id, stored_erin.id, stored_gale.id] {
        assert!(!follow::exists(&pool, severed, alice.id).await.unwrap());
    }
    assert!(
        follow::exists(&pool, alice.id, stored_frank.id)
            .await
            .unwrap()
    );
    assert!(
        follow::exists(&pool, stored_frank.id, alice.id)
            .await
            .unwrap()
    );

    // Four retractions: gale's URI-less edge federates nothing, exactly like
    // the per-edge pipeline.
    assert_eq!(delivery::run_due(&state).await, 4);
    let sent = stub.deliveries();
    let find = |inbox: &str| {
        sent.iter()
            .find(|delivery| delivery.inbox_url == inbox)
            .unwrap_or_else(|| panic!("no delivery to {inbox}"))
            .activity
            .clone()
    };

    let context = "https://www.w3.org/ns/activitystreams";
    assert_eq!(
        find("https://cut.example/users/bob/inbox"),
        json!({
            "@context": context,
            "id": format!("{alice_uri}#follows/101/undo"),
            "type": "Undo",
            "actor": alice_uri,
            "object": {
                "@context": context,
                "id": format!("{alice_uri}#follows/101"),
                "type": "Follow",
                "actor": alice_uri,
                "object": "https://cut.example/users/bob",
            },
        }),
    );
    assert_eq!(
        find("https://cut.example/users/carol/inbox"),
        json!({
            "@context": context,
            "id": format!("{alice_uri}#follows/102/undo"),
            "type": "Undo",
            "actor": alice_uri,
            "object": {
                "@context": context,
                "id": format!("{alice_uri}#follows/102"),
                "type": "Follow",
                "actor": alice_uri,
                "object": "https://cut.example/users/carol",
            },
        }),
    );
    for (who, edge_uri) in [
        ("dan", "https://cut.example/users/dan#follows/alice"),
        ("erin", "https://cut.example/users/erin#follows/alice"),
    ] {
        let reject = find(&format!("https://cut.example/users/{who}/inbox"));
        // The Reject's own id carries a freshly-minted marker; everything
        // else is pinned.
        assert!(
            reject["id"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{alice_uri}#rejects/follows/")),
            "{who}: {reject:?}"
        );
        assert_eq!(reject["@context"], context);
        assert_eq!(reject["type"], "Reject");
        assert_eq!(reject["actor"], alice_uri.as_str());
        assert_eq!(
            reject["object"],
            json!({
                "id": edge_uri,
                "type": "Follow",
                "actor": format!("https://cut.example/users/{who}"),
                "object": alice_uri,
            }),
        );
    }
}
