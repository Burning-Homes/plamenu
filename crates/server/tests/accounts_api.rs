//! Accounts API integration tests: lookup, account statuses, follow /
//! unfollow and relationships — through the real router.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::hash_password;
use plamenu::{delivery, remote};
use plamenu_db::account::{self, Account};
use plamenu_db::{PgPool, follow, user};
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
            "client_name": "accounts-api",
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

async fn post_status(pool: &PgPool, username: &str, text: &str, visibility: &str) -> i64 {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username,
            text,
            visibility,
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    stored.id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn lookup_resolves_known_accounts_without_webfinger(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let app = || test_app(pool.clone());

    // Local, by bare username and by full acct, case-insensitively.
    for acct in ["alice", "ALICE", "alice@plamenu.test", "@alice"] {
        let (status, body) = api(
            app(),
            "GET",
            &format!("/api/v1/accounts/lookup?acct={acct}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{acct}: {body:?}");
        assert_eq!(body["id"], alice.id.to_string());
        assert_eq!(body["acct"], "alice");
    }

    // A known remote account; `skip_webfinger` is accepted and ignored.
    let (status, body) = api(
        app(),
        "GET",
        "/api/v1/accounts/lookup?acct=bob@remote.example&skip_webfinger=false",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], stored_bob.id.to_string());
    assert_eq!(body["acct"], "bob@remote.example");

    // Unknown accounts are a Mastodon-style 404, never a webfinger trip.
    let (status, body) = api(
        app(),
        "GET",
        "/api/v1/accounts/lookup?acct=nobody@remote.example",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_statuses_lists_and_filters(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let public_id = post_status(&pool, "alice", "hello public", "public").await;
    let private_id = post_status(&pool, "alice", "followers only", "private").await;
    let base = format!("/api/v1/accounts/{}/statuses", alice.id);

    // Anonymous viewers see only public/unlisted.
    let (status, body) = api(app(), "GET", &base, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [public_id.to_string()]);

    // The author sees everything, newest first.
    let (_, body) = api(app(), "GET", &base, Some(&alice_token), None).await;
    let ids: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, [private_id.to_string(), public_id.to_string()]);

    // A non-follower does not see the private post; a follower does.
    let (_, body) = api(app(), "GET", &base, Some(&carol_token), None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let carol_account = plamenu_db::account::find_local_by_username(&pool, "carol")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, carol_account.id, alice.id, None)
        .await
        .unwrap();
    let (_, body) = api(app(), "GET", &base, Some(&carol_token), None).await;
    assert_eq!(body.as_array().unwrap().len(), 2);

    // pinned=true is an empty page (no pins implemented).
    let (status, body) = api(app(), "GET", &format!("{base}?pinned=true"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    // Keyset pagination via max_id.
    let (_, body) = api(
        app(),
        "GET",
        &format!("{base}?max_id={private_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [public_id.to_string()]);

    // Unknown accounts 404.
    let (status, _) = api(app(), "GET", "/api/v1/accounts/1/statuses", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The account-statuses listing honors the viewer's timeline-order setting,
/// like the home/list/tag timelines: flipping to `received` reorders a
/// backfilled post (older publish date, newer id) from publish order to
/// ingest order.
#[sqlx::test(migrations = "../db/migrations")]
async fn account_statuses_honor_viewer_timeline_order(pool: PgPool) {
    let author = create_local_account(&pool, "author", "Author").await;
    let (viewer, viewer_token) = user_with_token(&pool, "viewer").await;
    let app = || test_app(pool.clone());

    let first = post_status(&pool, "author", "published earlier", "public").await;
    let second = post_status(&pool, "author", "published later", "public").await;
    // Backdate the newer post's publish time so it sinks under publish order
    // while keeping its higher (newer-ingest) id — the backfill divergence.
    sqlx::query("UPDATE statuses SET sort_at = now() - interval '1 year' WHERE id = $1")
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();

    let base = format!("/api/v1/accounts/{}/statuses", author.id);
    let ids = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Default order is publish order: newest publish date first, so the
    // backdated `second` sinks below `first`.
    let (status, body) = api(app(), "GET", &base, Some(&viewer_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), [first.to_string(), second.to_string()]);

    // Flip the viewer to ingest order: newest id first, so `second` leads.
    let viewer_user = user::find_by_account_id(&pool, viewer.id)
        .await
        .unwrap()
        .unwrap();
    user::update_settings(
        &pool,
        viewer_user.id,
        user::UserSettings {
            timeline_order: user::TimelineOrder::Received,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();

    let (_, body) = api(app(), "GET", &base, Some(&viewer_token), None).await;
    assert_eq!(ids(&body), [second.to_string(), first.to_string()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_statuses_reply_and_reblog_filters(pool: PgPool) {
    let (alice, _) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let original = post_status(&pool, "carol", "from carol", "public").await;
    let root = post_status(&pool, "alice", "thread root", "public").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    // A self-reply (kept by exclude_replies) and a reply to carol (dropped).
    let (self_reply, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "more thread",
            visibility: "public",
            in_reply_to_id: Some(root),
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (other_reply, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "reply to carol",
            visibility: "public",
            in_reply_to_id: Some(original),
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let alice_account = plamenu_db::account::find_by_id(&pool, alice.id)
        .await
        .unwrap()
        .unwrap();
    let boost = actions::reblog_status(&state, &alice_account, original)
        .await
        .unwrap();

    let base = format!("/api/v1/accounts/{}/statuses", alice.id);
    let collect_ids = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };

    let (_, body) = api(app(), "GET", &base, Some(&carol_token), None).await;
    assert_eq!(
        collect_ids(&body),
        [
            boost.id.to_string(),
            other_reply.id.to_string(),
            self_reply.id.to_string(),
            root.to_string(),
        ]
    );

    // exclude_replies keeps self-replies, drops replies to others.
    let (_, body) = api(
        app(),
        "GET",
        &format!("{base}?exclude_replies=true"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(
        collect_ids(&body),
        [
            boost.id.to_string(),
            self_reply.id.to_string(),
            root.to_string(),
        ]
    );

    // exclude_reblogs drops the boost.
    let (_, body) = api(
        app(),
        "GET",
        &format!("{base}?exclude_reblogs=1"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(
        collect_ids(&body),
        [
            other_reply.id.to_string(),
            self_reply.id.to_string(),
            root.to_string(),
        ]
    );

    // only_media: nothing has attachments.
    let (_, body) = api(
        app(),
        "GET",
        &format!("{base}?only_media=true"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(body, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_and_unfollow_local_account(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["id"], carol.id.to_string());
    assert_eq!(rel["following"], true);
    assert_eq!(rel["requested"], false);
    assert_eq!(
        follow::pending_state(&pool, alice.id, carol.id)
            .await
            .unwrap(),
        Some(false)
    );

    // Carol got a follow notification and sees followed_by.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(notifications[0]["type"], "follow");
    assert_eq!(notifications[0]["account"]["id"], alice.id.to_string());
    let (_, rels) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={}&id[]=999", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    let rels = rels.as_array().unwrap();
    assert_eq!(rels.len(), 1, "unknown ids are dropped");
    assert_eq!(rels[0]["followed_by"], true);
    assert_eq!(rels[0]["following"], false);

    // Re-following is idempotent (no duplicate notification).
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);

    // Unfollow; relationship reverts, repeat is a no-op.
    for _ in 0..2 {
        let (status, rel) = api(
            app(),
            "POST",
            &format!("/api/v1/accounts/{}/unfollow", carol.id),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rel["following"], false);
    }
    assert!(!follow::exists(&pool, alice.id, carol.id).await.unwrap());

    // The unfollow retracts the follow notification too (Mastodon destroys
    // it with the Follow row) — no stale unread badge for carol.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 0);

    // Self-follow is Mastodon's 403.
    let (status, body) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", alice.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "Following your own account is not allowed");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_and_unfollow_remote_account_federate(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    // Optimistic `following: true` in the response, pending in the DB.
    assert_eq!(rel["following"], true);
    assert_eq!(
        follow::pending_state(&pool, alice.id, stored_bob.id)
            .await
            .unwrap(),
        Some(true)
    );

    // The Follow goes out to bob's inbox.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    assert_eq!(sent[0].activity["type"], "Follow");
    assert_eq!(sent[0].activity["object"], bob.actor.id);
    let follow_uri = sent[0].activity["id"].as_str().unwrap().to_owned();

    // Unfollow federates Undo(Follow) carrying the original activity id.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unfollow", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["following"], false);
    assert_eq!(rel["requested"], false);
    assert!(
        !follow::exists(&pool, alice.id, stored_bob.id)
            .await
            .unwrap()
    );

    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].inbox_url, bob.actor.inbox);
    assert_eq!(sent[1].activity["type"], "Undo");
    assert_eq!(sent[1].activity["object"]["type"], "Follow");
    assert_eq!(sent[1].activity["object"]["id"], follow_uri);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remove_from_followers_severs_and_rejects(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    // Bob follows alice (as if an inbound Follow had been accepted).
    let bob_follow_uri = "https://remote.example/users/bob/follows/1";
    follow::create(&pool, stored_bob.id, alice.id, Some(bob_follow_uri))
        .await
        .unwrap();
    assert!(
        follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );

    // Alice removes bob from her followers.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/remove_from_followers", stored_bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["followed_by"], false);
    assert!(
        !follow::exists(&pool, stored_bob.id, alice.id)
            .await
            .unwrap()
    );

    // Bob's server learns via Reject(Follow) carrying his original activity id.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    assert_eq!(sent[0].activity["type"], "Reject");
    assert_eq!(sent[0].activity["object"]["type"], "Follow");
    assert_eq!(sent[0].activity["object"]["id"], bob_follow_uri);

    // Removing a non-follower is an idempotent no-op (no further delivery).
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/remove_from_followers", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery::run_due(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn familiar_followers_lists_mutual_follows(pool: PgPool) {
    // viewer follows carol and dave; carol and erin both follow target;
    // dave does not. So the target's familiar followers (for viewer) = carol.
    let (viewer, token) = user_with_token(&pool, "viewer").await;
    let viewer_id = viewer.id;
    let target = create_local_account(&pool, "target", "Target").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    let other = create_local_account(&pool, "other", "Other").await;

    follow::create(&pool, viewer_id, carol.id, None)
        .await
        .unwrap();
    follow::create(&pool, viewer_id, dave.id, None)
        .await
        .unwrap();
    follow::create(&pool, carol.id, target.id, None)
        .await
        .unwrap();
    follow::create(&pool, erin.id, target.id, None)
        .await
        .unwrap();
    // dave is followed by the viewer but does not follow the target, so dave
    // is not familiar; erin follows the target but the viewer doesn't follow
    // erin, so erin is not familiar either. Only carol qualifies.
    let app = || test_app(pool.clone());

    let uri = format!(
        "/api/v1/accounts/familiar_followers?id[]={}&id[]={}",
        target.id, other.id
    );
    let (status, body) = api(app(), "GET", &uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let results = body.as_array().unwrap();
    assert_eq!(results.len(), 2);

    // First result is the target; its familiar followers are exactly [carol].
    assert_eq!(results[0]["id"], target.id.to_string());
    let names: Vec<&str> = results[0]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(names, [carol.id.to_string()]);

    // Second (other) has no familiar followers, but still gets an entry.
    assert_eq!(results[1]["id"], other.id.to_string());
    assert!(results[1]["accounts"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn familiar_followers_unknown_id_still_gets_empty_entry(pool: PgPool) {
    // Regression: a nonexistent id must still appear with an
    // empty account list (Mastodon's presenter over `Account.where(id:)`),
    // rather than being silently dropped as it was before.
    let (_viewer, token) = user_with_token(&pool, "viewer").await;
    let target = create_local_account(&pool, "target", "Target").await;
    let app = || test_app(pool.clone());
    let uri = format!(
        "/api/v1/accounts/familiar_followers?id[]=999999&id[]={}",
        target.id
    );
    let (status, body) = api(app(), "GET", &uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let results = body.as_array().unwrap();
    // Order is preserved: the unknown id first, then the real target — both
    // with empty account lists.
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], "999999");
    assert!(results[0]["accounts"].as_array().unwrap().is_empty());
    assert_eq!(results[1]["id"], target.id.to_string());
    assert!(results[1]["accounts"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn familiar_followers_rejects_oversized_id_batch(pool: PgPool) {
    // Regression: an oversized `id[]` batch is a 422 rather
    // than a request that performs work proportional to the body.
    let (_viewer, token) = user_with_token(&pool, "viewer").await;
    let app = || test_app(pool.clone());
    let over = (1..=201)
        .map(|i| format!("id[]={i}"))
        .collect::<Vec<_>>()
        .join("&");
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/familiar_followers?{over}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Exactly at the cap is accepted (every id is unknown, so all empty).
    let at_cap = (1..=200)
        .map(|i| format!("id[]={i}"))
        .collect::<Vec<_>>()
        .join("&");
    let (status, body) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/familiar_followers?{at_cap}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 200);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn relationships_rejects_oversized_id_batch(pool: PgPool) {
    // Regression: `relationships` renders a repeated id twice,
    // so the raw entry count — duplicates included — must be capped.
    let (_viewer, token) = user_with_token(&pool, "viewer").await;
    let app = || test_app(pool.clone());
    let dupes = (0..201)
        .map(|_| "id[]=1".to_string())
        .collect::<Vec<_>>()
        .join("&");
    let (status, _) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/relationships?{dupes}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_note_sets_clears_and_rides_relationship(pool: PgPool) {
    let (_viewer, token) = user_with_token(&pool, "viewer").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let app = || test_app(pool.clone());
    let note_uri = format!("/api/v1/accounts/{}/note", bob.id);
    let rel_uri = format!("/api/v1/accounts/relationships?id[]={}", bob.id);

    // Setting a note returns the relationship carrying it.
    let (status, body) = api(
        app(),
        "POST",
        &note_uri,
        Some(&token),
        Some(json!({ "comment": "met at a conference" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["note"], "met at a conference");

    // It persists on the relationships endpoint.
    let (_, body) = api(app(), "GET", &rel_uri, Some(&token), None).await;
    assert_eq!(body[0]["note"], "met at a conference");

    // A blank comment clears it.
    let (_, body) = api(
        app(),
        "POST",
        &note_uri,
        Some(&token),
        Some(json!({ "comment": "   " })),
    )
    .await;
    assert_eq!(body["note"], "");
    let (_, body) = api(app(), "GET", &rel_uri, Some(&token), None).await;
    assert_eq!(body[0]["note"], "");

    // A note about a missing account is a 404.
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/accounts/999999/note",
        Some(&token),
        Some(json!({ "comment": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn endorse_pins_accounts_to_profile(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let app = || test_app(pool.clone());
    let rel_uri = format!("/api/v1/accounts/relationships?id[]={}", bob.id);

    // Endorsing returns the relationship with `endorsed: true`; idempotent.
    for _ in 0..2 {
        let (status, rel) = api(
            app(),
            "POST",
            &format!("/api/v1/accounts/{}/endorse", bob.id),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rel:?}");
        assert_eq!(rel["endorsed"], true);
    }
    // The `/pin` alias endorses carol too.
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/pin", carol.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // `endorsed` rides the relationship entity.
    let (_, body) = api(app(), "GET", &rel_uri, Some(&token), None).await;
    assert_eq!(body[0]["endorsed"], true);

    // GET /endorsements lists them, most recently pinned first.
    let (status, body) = api(app(), "GET", "/api/v1/endorsements", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [carol.id.to_string(), bob.id.to_string()]);

    // The public per-account endorsements list mirrors it (no auth needed).
    let (status, body) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/endorsements", alice.id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.as_array().unwrap().len(), 2);

    // Unendorsing (via the `/unpin` alias) drops bob and clears the flag.
    let (status, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/unpin", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rel["endorsed"], false);
    let (_, body) = api(app(), "GET", "/api/v1/endorsements", Some(&token), None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Endorsing a missing account is a 404.
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/accounts/999999/endorse",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_endpoints_require_auth(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let app = || test_app(pool.clone());

    // Unauthenticated GETs: familiar_followers and relationships.
    for uri in [
        "/api/v1/accounts/familiar_followers?id[]=1",
        "/api/v1/accounts/relationships?id[]=1",
    ] {
        let (status, _) = api(app(), "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }

    // Unauthenticated follow / unfollow POSTs.
    for uri in [
        format!("/api/v1/accounts/{}/follow", alice.id),
        format!("/api/v1/accounts/{}/unfollow", alice.id),
    ] {
        let (status, _) = api(app(), "POST", &uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }

    // The note POST (carries a JSON body) is gated too.
    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/note", alice.id),
        None,
        Some(json!({ "comment": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn batch_accounts_returns_requested_in_order(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;

    // Request bob, then alice, plus an unknown id; carol is omitted.
    let uri = format!(
        "/api/v1/accounts?id[]={}&id[]={}&id[]=999999999",
        bob.id, alice.id
    );
    let (status, body) = api(test_app(pool.clone()), "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    // Request order preserved; unknown id dropped, carol absent.
    assert_eq!(ids, vec![bob.id.to_string(), alice.id.to_string()]);
    assert!(!ids.contains(&carol.id.to_string().as_str()));

    // More than 40 ids is a 422, like Mastodon's batch-account limit.
    let query = (0..41)
        .map(|i| format!("id[]={i}"))
        .collect::<Vec<_>>()
        .join("&");
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts?{query}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sensitive_action_forces_warning_for_everyone_but_the_author(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let status_id = post_status(&pool, "alice", "ordinary image post", "public").await;
    account::sensitize(&pool, alice.id).await.unwrap();

    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public["sensitive"], true);

    let (_, own) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(own["sensitive"], false, "author sees their stored choice");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn silenced_local_public_post_is_demoted_to_unlisted(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    account::silence(&pool, alice.id).await.unwrap();

    let status_id = post_status(&pool, "alice", "limited post", "public").await;
    let stored = plamenu_db::status::find_by_id(&pool, status_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.visibility, "unlisted");
}
