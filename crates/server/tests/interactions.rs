//! M4 integration tests: favourites, boosts, replies/context, notifications,
//! visibility gating, public timelines and federated interactions.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, deliver_all_due, test_app,
    test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, custom_emoji, follow, oauth, reaction, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    // Mint the app + access token directly. The full OAuth authorization-code
    // machinery (apps → authorize → token, incl. PKCE) is exercised on its own
    // in `client_api.rs`; re-driving it here once per user only inflated every
    // interaction test with three extra router builds and HTTP round-trips.
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "interactions",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write",
    )
    .await
    .unwrap();
    (account, token)
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

#[sqlx::test(migrations = "../db/migrations")]
async fn favourite_reblog_roundtrip_with_counts_and_notifications(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (_, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "interact with me"})),
    )
    .await;
    let status_id = posted["id"].as_str().unwrap().to_owned();

    // Carol favourites and boosts.
    let (code, faved) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/favourite"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(faved["favourited"], true);
    assert_eq!(faved["favourites_count"], 1);

    let (code, boosted) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/reblog"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(boosted["reblogged"], true, "{boosted}");
    assert_eq!(boosted["content"], "");
    assert_eq!(boosted["reblog"]["id"], status_id.as_str());
    assert_eq!(boosted["reblog"]["reblogs_count"], 1);
    assert_eq!(boosted["account"]["username"], "carol");

    // The boost shows up in carol's home timeline as a wrapper.
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(home[0]["reblog"]["content"], "<p>interact with me</p>");

    // Alice got both notifications, newest first.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let kinds: Vec<&str> = notifications
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["reblog", "favourite"]);
    assert_eq!(notifications[0]["account"]["username"], "carol");
    assert_eq!(notifications[0]["status"]["id"], status_id.as_str());

    // Undo both; counters return to zero.
    api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/unreblog"),
        Some(&carol_token),
        None,
    )
    .await;
    let (_, unfaved) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/unfavourite"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(unfaved["favourited"], false);
    assert_eq!(unfaved["favourites_count"], 0);
    assert_eq!(unfaved["reblogs_count"], 0);

    // The retracted interactions take their notifications with them, so no
    // stale unread badge survives (Mastodon destroys the notification with
    // the interaction row).
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));
    let (_, unread) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(unread["count"], 0);

    // Deleting the boost row (DELETE /statuses/{boost_id}) is also an
    // unboost and clears the notification the same way.
    let (_, boosted) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/reblog"),
        Some(&carol_token),
        None,
    )
    .await;
    let boost_id = boosted["id"].as_str().unwrap().to_owned();
    let (code, deleted) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{boost_id}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    // Like Mastodon's `source_requested` DELETE rendering: `text` replaces
    // `content` on the wrapper (null — boosts have no source) and on the
    // nested target (its raw source).
    assert_eq!(deleted["text"], json!(null));
    assert!(deleted.get("content").is_none());
    assert_eq!(deleted["reblog"]["text"], "interact with me");
    assert!(deleted["reblog"].get("content").is_none());
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pleroma_emoji_reaction_api_lists_accounts_and_clears_notifications(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (_, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "react locally"})),
    )
    .await;
    let status_id = posted["id"].as_str().unwrap().to_owned();

    let (code, reacted) = api(
        app(),
        "PUT",
        &format!("/api/v1/pleroma/statuses/{status_id}/reactions/%F0%9F%94%A5"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let chips = reacted["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(chips[0]["name"], "🔥");
    assert_eq!(chips[0]["count"], 1);
    assert_eq!(chips[0]["me"], true);

    let (code, reactions) = api(
        app(),
        "GET",
        &format!("/api/v1/pleroma/statuses/{status_id}/reactions"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(reactions[0]["name"], "🔥");
    assert_eq!(reactions[0]["count"], 1);
    assert_eq!(reactions[0]["me"], true);
    assert_eq!(reactions[0]["accounts"][0]["username"], "carol");

    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications[0]["type"], "pleroma:emoji_reaction");
    assert_eq!(notifications[0]["emoji"], "🔥");
    assert_eq!(notifications[0]["account"]["username"], "carol");
    assert_eq!(notifications[0]["status"]["id"], status_id);
    assert_eq!(
        notifications[0]["status"]["content"],
        "<p>react locally</p>"
    );

    let (code, unreacted) = api(
        app(),
        "DELETE",
        &format!("/api/v1/pleroma/statuses/{status_id}/reactions/%F0%9F%94%A5"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        unreacted["pleroma"]["emoji_reactions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert!(notifications.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn replies_build_a_context_thread(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let (_, root) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "root"})),
    )
    .await;
    let root_id = root["id"].as_str().unwrap().to_owned();
    let (_, reply) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "reply", "in_reply_to_id": root_id})),
    )
    .await;
    assert_eq!(reply["in_reply_to_id"], root_id.as_str());
    // The parent author rides along (this was always null).
    assert_eq!(
        reply["in_reply_to_account_id"],
        root["account"]["id"].as_str().unwrap()
    );
    let reply_id = reply["id"].as_str().unwrap().to_owned();

    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{reply_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(context["ancestors"][0]["id"], root_id.as_str());
    assert!(context["descendants"].as_array().unwrap().is_empty());

    let (_, root_context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(root_context["descendants"][0]["id"], reply_id.as_str());

    // Replying to something invisible 404s.
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "x", "in_reply_to_id": "99999"})),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

/// A third-party client opening `/context` expresses the same authenticated
/// thread-view intent as the built-in permalink. Cold-history replies therefore
/// heal upward as well as queueing the existing downward replies crawl.
#[sqlx::test(migrations = "../db/migrations")]
async fn context_open_chases_a_history_orphan_parent(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("context-parent.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let parent_uri = format!("{}/statuses/context-parent", bob.actor.id);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        parent_uri.clone(),
        json!({
            "id": parent_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>the-context-parent</p>",
            "published": "2026-08-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
        }),
    );
    let orphan = status::upsert_remote_with_provenance(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://context-parent.example/users/bob/statuses/history-reply",
            account_id: stored_bob.id,
            content: "<p>the-context-reply</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: Some(&parent_uri),
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
        status::IngestProvenance::History,
    )
    .await
    .unwrap();
    let app = || test_app_with(pool.clone(), stub.clone());

    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/context", orphan.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    let mut parent = None;
    for _ in 0..100 {
        if let Some(row) = status::find_by_id(&pool, orphan.id).await.unwrap()
            && let Some(parent_id) = row.in_reply_to_id
        {
            parent = status::find_by_id(&pool, parent_id).await.unwrap();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let parent = parent.expect("the API context fetch adopted the history orphan");
    assert_eq!(parent.uri.as_deref(), Some(parent_uri.as_str()));
    assert_eq!(
        stub.fetches()
            .iter()
            .filter(|uri| **uri == parent_uri)
            .count(),
        1,
        "the parent is fetched once: {:?}",
        stub.fetches()
    );

    let (code, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/context", orphan.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(context["ancestors"][0]["id"], parent.id.to_string());
    assert_eq!(
        context["ancestors"][0]["content"],
        "<p>the-context-parent</p>"
    );
}

/// The thread-order preference reshapes `/context` for every client of that
/// user: `tree` (default) serves Mastodon's shape — chain ancestors,
/// depth-first descendants with the author's self-replies promoted — while
/// `flat` serves Pleroma's — the whole conversation in arrival order split at
/// the focal post, sibling branches included above it.
#[sqlx::test(migrations = "../db/migrations")]
async fn thread_order_setting_reshapes_context(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let post = |token: String, body: Value| async move {
        let (code, status) = api(app(), "POST", "/api/v1/statuses", Some(&token), Some(body)).await;
        assert_eq!(code, StatusCode::OK, "{status:?}");
        status["id"].as_str().unwrap().to_owned()
    };
    // alice: root — bob: r1 — alice: r2 (self-reply, posted after r1) —
    // carol: r1a (deep in bob's branch).
    let root = post(alice_token.clone(), json!({"status": "root"})).await;
    let r1 = post(
        bob_token.clone(),
        json!({"status": "r1", "in_reply_to_id": root}),
    )
    .await;
    let r2 = post(
        alice_token.clone(),
        json!({"status": "r2", "in_reply_to_id": root}),
    )
    .await;
    let r1a = post(
        carol_token.clone(),
        json!({"status": "r1a", "in_reply_to_id": r1}),
    )
    .await;

    let ids = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Tree (default): depth-first (r1 before its child r1a, sibling r2 after
    // the branch) and then alice's self-reply r2 promoted to the top.
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root}/context"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        ids(&context["descendants"]),
        [r2.as_str(), r1.as_str(), r1a.as_str()]
    );

    // Tree ancestors: just the reply chain, root first.
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{r1a}/context"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(ids(&context["ancestors"]), [root.as_str(), r1.as_str()]);

    // Flip alice to flat; bob keeps the default.
    let user_row = user::find_by_email(&pool, "alice@plamenu.test")
        .await
        .unwrap()
        .unwrap();
    let mut settings = user::settings_by_user_id(&pool, user_row.id)
        .await
        .unwrap()
        .unwrap();
    settings.thread_order = user::ThreadOrder::Flat;
    user::update_settings(&pool, user_row.id, settings)
        .await
        .unwrap()
        .unwrap();

    // Flat descendants: strictly arrival order, no promotion, no regrouping.
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root}/context"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        ids(&context["descendants"]),
        [r1.as_str(), r2.as_str(), r1a.as_str()]
    );

    // Flat ancestors: everything older in the conversation — including the
    // sibling branch r2, which the tree shape would never show here — and
    // nothing lost between the two halves.
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{r1a}/context"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        ids(&context["ancestors"]),
        [root.as_str(), r1.as_str(), r2.as_str()]
    );
    assert!(context["descendants"].as_array().unwrap().is_empty());

    // The setting is per-user: bob still reads the Mastodon shape.
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root}/context"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(
        ids(&context["descendants"]),
        [r2.as_str(), r1.as_str(), r1a.as_str()]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn visibility_is_enforced_for_reads_and_boosts(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let (_dave, dave_token) = user_with_token(&pool, "dave").await;
    // carol follows alice (locally, accepted).
    follow::create(&pool, carol.id, alice.id, None)
        .await
        .unwrap();
    let app = || test_app(pool.clone());

    let (_, private_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "followers only", "visibility": "private"})),
    )
    .await;
    assert_eq!(private_post["visibility"], "private");
    let private_id = private_post["id"].as_str().unwrap().to_owned();

    // The author and her follower can read it; others and anonymous cannot.
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}"),
        Some(&dave_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Followers-only posts cannot be boosted, even by followers.
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{private_id}/reblog"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // The AP object is not dereferenceable either.
    let request = Request::builder()
        .uri(format!("/users/alice/statuses/{private_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Direct visibility is accepted and even stricter: not even followers
    // see it (the full DM behavior lives in tests/direct.rs).
    let (code, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "psst", "visibility": "direct"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", posted["id"].as_str().unwrap()),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_timeline_lists_public_originals_only(pool: PgPool) {
    common::open_previews(&pool).await;
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    for (text, visibility) in [
        ("public post", "public"),
        ("unlisted post", "unlisted"),
        ("private post", "private"),
    ] {
        api(
            app(),
            "POST",
            "/api/v1/statuses",
            Some(&token),
            Some(json!({"status": text, "visibility": visibility})),
        )
        .await;
    }

    // Anonymous read of the public timeline.
    let (code, timeline) = api(app(), "GET", "/api/v1/timelines/public", None, None).await;
    assert_eq!(code, StatusCode::OK);
    let contents: Vec<&str> = timeline
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["<p>public post</p>"]);

    let (_, local_timeline) = api(
        app(),
        "GET",
        "/api/v1/timelines/public?local=true",
        None,
        None,
    )
    .await;
    assert_eq!(local_timeline.as_array().unwrap().len(), 1);
}

/// The public timelines ship Mastodon-shaped — a reply to someone else
/// is not a top-level post — and the operator can turn the firehose back on.
#[sqlx::test(migrations = "../db/migrations")]
async fn public_timeline_reply_policy_follows_the_operator_setting(pool: PgPool) {
    common::open_previews(&pool).await;
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    let (_, root) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "a thought", "visibility": "public"})),
    )
    .await;
    let root_id = root["id"].as_str().unwrap().to_owned();
    for (token, text) in [(&alice_token, "…continued"), (&bob_token, "disagree")] {
        api(
            app(),
            "POST",
            "/api/v1/statuses",
            Some(token),
            Some(json!({
                "status": text,
                "visibility": "public",
                "in_reply_to_id": root_id,
            })),
        )
        .await;
    }

    let contents = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["content"].as_str().unwrap().to_owned())
            .collect()
    };

    for uri in [
        "/api/v1/timelines/public",
        "/api/v1/timelines/public?local=true",
    ] {
        let (code, timeline) = api(app(), "GET", uri, None, None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            contents(&timeline),
            ["<p>…continued</p>", "<p>a thought</p>"],
            "{uri}: the author's own thread stays, the stranger's reply does not"
        );
    }

    // Flipping the setting restores the pre-0031 firehose.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            public_timeline_replies: true,
            ..current.as_update()
        },
    )
    .await
    .unwrap();

    for uri in [
        "/api/v1/timelines/public",
        "/api/v1/timelines/public?local=true",
    ] {
        let (_, timeline) = api(app(), "GET", uri, None, None).await;
        assert_eq!(
            contents(&timeline),
            ["<p>disagree</p>", "<p>…continued</p>", "<p>a thought</p>"],
            "{uri}: the knob brings replies back"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_visibility_is_same_instance_only_and_never_federates(pool: PgPool) {
    common::open_previews(&pool).await;
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let (_dave, dave_token) = user_with_token(&pool, "dave").await;
    // Carol follows Alice, so the local-only post belongs on Carol's home
    // timeline while remaining invisible to anonymous and federated readers.
    let carol = plamenu_db::account::find_local_by_username(&pool, "carol")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, carol.id, alice.id, None)
        .await
        .unwrap();

    let bob = RemoteUser::new("remote.example", "bob");
    let eve = RemoteUser::new("remote.example", "eve");
    let stub = StubFederation::with_users(&[&bob, &eve]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let remote_note = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: stored_bob.id,
            content: "<p>remote target</p>",
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
    let quote_remote = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({
            "status": "contained quote",
            "visibility": "local",
            "quoted_status_id": remote_note.id.to_string()
        })),
    )
    .await;
    assert_eq!(quote_remote.0, StatusCode::UNPROCESSABLE_ENTITY);

    let stored_eve = remote::store_remote_actor(&pool, &eve.actor).await.unwrap();
    follow::create(&pool, stored_eve.id, alice.id, None)
        .await
        .unwrap();

    let (code, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({
            "status": "@bob@remote.example local bulletin",
            "visibility": "local"
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    assert_eq!(posted["visibility"], "local");
    assert_eq!(posted["quote_approval"]["current_user"], "denied");
    let status_id = posted["id"].as_str().unwrap().to_owned();
    assert_eq!(
        delivery::run_due(&state).await,
        0,
        "Create must not federate"
    );
    assert!(
        stub.deliveries().is_empty(),
        "neither the remote follower nor remote mention may receive the post"
    );

    for token in [&alice_token, &carol_token, &dave_token] {
        let (code, body) = api(
            app(),
            "GET",
            &format!("/api/v1/statuses/{status_id}"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(home[0]["id"], status_id.as_str());

    let (_, federated) = api(
        app(),
        "GET",
        "/api/v1/timelines/public",
        Some(&dave_token),
        None,
    )
    .await;
    assert!(
        !federated
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == status_id),
        "federated public timeline must exclude local-only posts"
    );
    let (_, local_as_anon) = api(
        app(),
        "GET",
        "/api/v1/timelines/public?local=true",
        None,
        None,
    )
    .await;
    assert!(
        local_as_anon.as_array().unwrap().is_empty(),
        "anonymous local timeline preview must not expose local-only posts"
    );
    let (_, local_as_dave) = api(
        app(),
        "GET",
        "/api/v1/timelines/public?local=true",
        Some(&dave_token),
        None,
    )
    .await;
    assert_eq!(local_as_dave[0]["id"], status_id.as_str());

    let (code, profile) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", alice.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        profile.as_array().unwrap().is_empty(),
        "anonymous profile listing must hide local-only posts"
    );
    let (_, profile) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/statuses", alice.id),
        Some(&dave_token),
        None,
    )
    .await;
    assert_eq!(profile[0]["id"], status_id.as_str());

    let ap_request = Request::builder()
        .uri(format!("/users/alice/statuses/{status_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let ap_response = app().oneshot(ap_request).await.unwrap();
    assert_eq!(ap_response.status(), StatusCode::NOT_FOUND);

    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/reblog"),
        Some(&dave_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    let (code, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&alice_token),
        Some(json!({"status": "@bob@remote.example local bulletin edited"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{edited}");
    assert_eq!(
        delivery::run_due(&state).await,
        0,
        "Update must not federate"
    );

    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        delivery::run_due(&state).await,
        0,
        "Delete must not federate"
    );
    assert!(stub.deliveries().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn deleting_a_status_federates_a_delete(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    // bob follows alice so deletes fan out to him.
    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();

    let (_, posted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "to be deleted"})),
    )
    .await;
    let status_id: i64 = posted["id"].as_str().unwrap().parse().unwrap();
    delivery::run_due(&state).await; // flush the Create

    let (code, deleted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "DELETE",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    // Mastodon's DELETE response is rendered `source_requested`: the raw
    // source `text` replaces `content` (delete-and-redraft).
    assert_eq!(deleted["text"], "to be deleted");
    assert!(deleted.get("content").is_none());
    assert!(
        status::find_by_id(&pool, status_id)
            .await
            .unwrap()
            .is_none()
    );

    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    let delete_activity = &sent.last().unwrap().activity;
    assert_eq!(delete_activity["type"], "Delete");
    assert_eq!(delete_activity["object"]["type"], "Tombstone");
    assert_eq!(
        delete_activity["object"]["id"],
        format!("https://plamenu.test/users/alice/statuses/{status_id}")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_interactions_arrive_and_undo(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // A local post bob will interact with, via its AP uri.
    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "like and boost me",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // Inbound Like.
    let like = json!({
        "id": format!("{}#likes/1", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
    });
    assert_eq!(
        post_signed(app(), &like, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // Inbound Announce. Its `published` time becomes the boost row's
    // `created_at`, like Mastodon.
    let announce_uri = format!("{}/statuses/99/activity", bob.actor.id);
    let announce = json!({
        "id": announce_uri,
        "type": "Announce",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
        "published": "2026-02-01T00:00:00Z",
    });
    assert_eq!(
        post_signed(app(), &announce, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let boost = status::find_by_uri(&pool, &announce_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        boost.created_at,
        time::macros::datetime!(2026-02-01 00:00 UTC)
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 1);
    assert_eq!(shown["reblogs_count"], 1);

    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let kinds: Vec<&str> = notifications
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["reblog", "favourite"]);
    assert_eq!(notifications[0]["account"]["acct"], "bob@remote.example");

    // A reply from bob threads onto the local status.
    let reply = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/100", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>remote reply</p>",
            "inReplyTo": note_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    });
    assert_eq!(
        post_signed(app(), &reply, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let (_, context) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/context", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(context["descendants"][0]["content"], "<p>remote reply</p>");

    // Undo both interactions.
    let undo_like = json!({
        "type": "Undo",
        "actor": bob.actor.id,
        "object": like,
    });
    assert_eq!(
        post_signed(app(), &undo_like, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let undo_announce = json!({
        "type": "Undo",
        "actor": bob.actor.id,
        "object": announce,
    });
    assert_eq!(
        post_signed(app(), &undo_announce, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0);
    assert_eq!(shown["reblogs_count"], 0);

    // The undone interactions retract their notifications too, like
    // Mastodon — nothing left to feed a stale unread badge.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));
    let (_, unread) = api(
        app(),
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(unread["count"], 0);
    let _ = alice;
}

/// A followed remote account boosting a post we have never seen — the ordinary
/// case, since accounts boost across the whole network, not just posts already
/// on our instance. The boosted note must be fetched from its origin and the
/// boost must land in the follower's home timeline. Regression: the plain-boost
/// inbox path used to resolve the target from local storage only, silently
/// dropping every `Announce` of an unknown status — so a followed Mastodon /
/// Pleroma / Misskey account's boosts never surfaced, while only group
/// (FEP-1b12) announces, which already fetched, got through.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_boost_of_unseen_post_is_fetched_and_surfaces(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;

    // The booster we follow, and the stranger whose post they boost — a post
    // authored on a third host that our instance has never encountered.
    let booster = RemoteUser::new("remote.example", "booster");
    let author = RemoteUser::new("other.example", "author");
    let stub = StubFederation::with_actors([booster.actor.clone(), author.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // The unseen post, served only from its origin (never delivered to us).
    let note_uri = format!("{}/statuses/1", author.actor.id);
    stub.objects.lock().unwrap().insert(
        note_uri.clone(),
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": note_uri,
            "type": "Note",
            "attributedTo": author.actor.id,
            "content": "<p>a post from across the network</p>",
            "published": "2026-03-01T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );

    // alice follows the booster, so the boost belongs in her home timeline.
    let booster_account = remote::store_remote_actor(&pool, &booster.actor)
        .await
        .unwrap();
    follow::create(
        &pool,
        alice.id,
        booster_account.id,
        Some("https://plamenu.test/users/alice#follows/1"),
    )
    .await
    .unwrap();

    // The booster announces the unseen post.
    let announce_uri = format!("{}/statuses/1/activity", booster.actor.id);
    let announce = json!({
        "id": announce_uri,
        "type": "Announce",
        "actor": booster.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [booster.actor.followers_url().unwrap()],
        "object": note_uri,
        "published": "2026-03-02T00:00:00Z",
    });
    assert_eq!(
        post_signed(app(), &announce, &booster.signer()).await,
        StatusCode::ACCEPTED
    );

    // The note was fetched from its origin and ingested.
    assert!(
        stub.fetches().contains(&note_uri),
        "the unseen boosted note must be fetched from its origin: {:?}",
        stub.fetches()
    );
    let ingested = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("boosted note ingested");
    assert_eq!(ingested.content, "<p>a post from across the network</p>");

    // The boost row exists and wraps the fetched note.
    let boost = status::find_by_uri(&pool, &announce_uri)
        .await
        .unwrap()
        .expect("boost row created");
    assert_eq!(boost.reblog_of_id, Some(ingested.id));

    // And it surfaces in the follower's home timeline as a boost wrapper.
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(home.as_array().unwrap().len(), 1, "{home}");
    assert_eq!(home[0]["account"]["acct"], "booster@remote.example");
    assert_eq!(
        home[0]["reblog"]["content"],
        "<p>a post from across the network</p>"
    );
    assert_eq!(home[0]["reblog"]["account"]["acct"], "author@other.example");
}

/// Pleroma's `Undo` carries only the original activity's id — a bare string
/// `object`, no embedded object to dispatch on (Mastodon embeds it). Each
/// retraction must find the activity by its stored uri: the boost row's uri,
/// the favourite's `Like` uri, the reaction's `EmojiReact` uri.
#[sqlx::test(migrations = "../db/migrations")]
async fn bare_object_uri_undo_retracts_like_boost_and_reaction(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "undo me by uri",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // bob likes, boosts and reacts, each activity under its own id.
    let like_uri = format!("{}#likes/1", bob.actor.id);
    let announce_uri = format!("{}/statuses/99/activity", bob.actor.id);
    let react_uri = format!("{}#reactions/1", bob.actor.id);
    let interactions = [
        json!({
            "id": like_uri,
            "type": "Like",
            "actor": bob.actor.id,
            "object": note_uri,
        }),
        json!({
            "id": announce_uri,
            "type": "Announce",
            "actor": bob.actor.id,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": note_uri,
        }),
        json!({
            "id": react_uri,
            "type": "EmojiReact",
            "actor": bob.actor.id,
            "content": "🔥",
            "object": note_uri,
        }),
    ];
    for activity in &interactions {
        assert_eq!(
            post_signed(app(), activity, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 1);
    assert_eq!(shown["reblogs_count"], 1);
    assert_eq!(shown["pleroma"]["emoji_reactions"][0]["name"], "🔥");

    // Each Undo names its activity's id as a bare string.
    for (n, undone_uri) in [&like_uri, &announce_uri, &react_uri].iter().enumerate() {
        let undo = json!({
            "id": format!("{}#undo/{n}", bob.actor.id),
            "type": "Undo",
            "actor": bob.actor.id,
            "object": undone_uri,
        });
        assert_eq!(
            post_signed(app(), &undo, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0);
    assert_eq!(shown["reblogs_count"], 0);
    assert_eq!(shown["pleroma"]["emoji_reactions"], json!([]));
    assert!(
        status::find_by_uri(&pool, &announce_uri)
            .await
            .unwrap()
            .is_none(),
        "the boost row is gone"
    );

    // The retractions took their notifications with them.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]));
}

/// Favourite, unfavourite, favourite — three activities a sender hands to its
/// delivery queue at once, so the `Undo` of the first `Like` routinely arrives
/// after the second `Like`. The retraction names an activity that is no longer
/// the one holding the favourite, so it must withdraw nothing: the sender does
/// still hold it, and the author's notification still stands.
#[sqlx::test(migrations = "../db/migrations")]
async fn an_overtaken_undo_does_not_withdraw_a_refreshed_favourite(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "flip flop",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);
    let like = |n: u32| {
        json!({
            "id": format!("{}#likes/{n}", bob.actor.id),
            "type": "Like",
            "actor": bob.actor.id,
            "object": note_uri,
        })
    };
    let favourites_count = async |token: &str| {
        let (_, shown) = api(
            app(),
            "GET",
            &format!("/api/v1/statuses/{}", stored.id),
            Some(token),
            None,
        )
        .await;
        shown["favourites_count"].as_i64().unwrap()
    };

    // Both Likes land before either retraction; the second refreshes the row.
    for n in [1, 2] {
        assert_eq!(
            post_signed(app(), &like(n), &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }
    assert_eq!(favourites_count(&alice_token).await, 1);

    // The overtaken Undo names the *first* Like.
    let undo = json!({
        "id": format!("{}#undo/1", bob.actor.id),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": like(1),
    });
    assert_eq!(
        post_signed(app(), &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        favourites_count(&alice_token).await,
        1,
        "the favourite the sender still holds survives the stale Undo"
    );
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        notifications.as_array().unwrap().len(),
        1,
        "and so does the notification announcing it: {notifications}"
    );

    // The Undo of the Like that *is* holding it still retracts, once.
    let undo = json!({
        "id": format!("{}#undo/2", bob.actor.id),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": like(2),
    });
    assert_eq!(
        post_signed(app(), &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(favourites_count(&alice_token).await, 0);
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]), "the notification goes with it");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_favourite_and_boost_federate_out(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    // A remote status from bob in the db.
    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/7",
            account_id: remote.id,
            content: "<p>bob's post</p>",
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

    // Favourite it via the API: a Like must be queued for bob's inbox.
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{}/favourite", remote_status.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    deliver_all_due(&state).await;
    let like_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(like_delivery.activity["type"], "Like");
    assert_eq!(like_delivery.inbox_url, bob.actor.inbox);
    assert_eq!(
        like_delivery.activity["object"],
        "https://remote.example/users/bob/statuses/7"
    );

    // Boost it: an Announce goes to followers + bob.
    let (code, boosted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{}/reblog", remote_status.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(boosted["reblog"]["content"], "<p>bob's post</p>");
    deliver_all_due(&state).await;
    let announce_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(announce_delivery.activity["type"], "Announce");
    assert_eq!(
        announce_delivery.activity["object"],
        "https://remote.example/users/bob/statuses/7"
    );

    // Unfavourite federates an Undo(Like).
    api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{}/unfavourite", remote_status.id),
        Some(&token),
        None,
    )
    .await;
    deliver_all_due(&state).await;
    let undo_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(undo_delivery.activity["type"], "Undo");
    assert_eq!(undo_delivery.activity["object"]["type"], "Like");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_emoji_reaction_federates_to_remote_author(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/8",
            account_id: remote.id,
            content: "<p>reactable</p>",
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

    let (code, reacted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PUT",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/%F0%9F%94%A5",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        reacted["pleroma"]["emoji_reactions"][0]["account_ids"][0],
        alice.id.to_string()
    );
    deliver_all_due(&state).await;
    let react_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(react_delivery.inbox_url, bob.actor.inbox);
    assert_eq!(react_delivery.activity["type"], "EmojiReact");
    assert_eq!(react_delivery.activity["content"], "🔥");
    assert_eq!(
        react_delivery.activity["object"],
        "https://remote.example/users/bob/statuses/8"
    );
    assert_eq!(react_delivery.activity["to"][1], bob.actor.id);
    let react_uri = react_delivery.activity["id"].as_str().unwrap().to_owned();

    let (code, reactions) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/%F0%9F%94%A5",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(reactions[0]["accounts"][0]["username"], "alice");

    api(
        test_app_with(pool.clone(), stub.clone()),
        "DELETE",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/%F0%9F%94%A5",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    deliver_all_due(&state).await;
    let undo_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(undo_delivery.activity["type"], "Undo");
    assert_eq!(undo_delivery.activity["object"]["type"], "EmojiReact");
    assert_eq!(undo_delivery.activity["object"]["id"], react_uri);

    plamenu_db::custom_emoji::create_local(&pool, "party", "party.png", "image/png", 0, None)
        .await
        .unwrap();
    let (code, custom) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PUT",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/:party:",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(custom["pleroma"]["emoji_reactions"][0]["name"], "party");
    assert_eq!(
        custom["pleroma"]["emoji_reactions"][0]["url"],
        "https://plamenu.test/media/party.png"
    );
    // Pleroma parity: the same chips are mirrored at the top level, where
    // Phanpy (and other Pleroma-aware clients) read them.
    assert_eq!(
        custom["emoji_reactions"], custom["pleroma"]["emoji_reactions"],
        "top-level emoji_reactions must mirror the pleroma-nested array"
    );
    deliver_all_due(&state).await;
    let custom_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(custom_delivery.activity["type"], "EmojiReact");
    assert_eq!(custom_delivery.activity["content"], ":party:");
    assert_eq!(custom_delivery.activity["tag"][0]["name"], ":party:");
    assert_eq!(
        custom_delivery.activity["tag"][0]["icon"]["url"],
        "https://plamenu.test/media/party.png"
    );
}

/// Joining (+1) an existing remote custom-emoji reaction, Pleroma-style: the
/// qualified `shortcode@host` name resolves against the reactions already on
/// the status, the outbound `EmojiReact` reuses the remote image, and the
/// `Undo` retracts it. A remote emoji that isn't already on the post can't
/// start a reaction (mirroring Pleroma's join-only rule).
#[sqlx::test(migrations = "../db/migrations")]
async fn joining_a_remote_custom_emoji_reaction_federates(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/9",
            account_id: remote.id,
            content: "<p>already reacted</p>",
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
    // Bob's own custom-emoji reaction, as an inbound EmojiReact stored it —
    // ingest also records the emoji tag (the row the reaction chip is proxied
    // through).
    custom_emoji::upsert_remote(
        &pool,
        custom_emoji::RemoteEmojiData {
            shortcode: "blobcat",
            domain: "remote.example",
            uri: Some("https://remote.example/emojis/blobcat"),
            image_remote_url: "https://remote.example/emoji/blobcat.png",
            updated: None,
        },
    )
    .await
    .unwrap();
    let emoji = custom_emoji::lookup(&pool, &["blobcat".to_owned()], Some("remote.example"))
        .await
        .unwrap()
        .pop()
        .unwrap();
    let managed = custom_emoji::find_managed_by_id(&pool, emoji.id)
        .await
        .unwrap()
        .unwrap();
    reaction::create_custom(
        &pool,
        reaction::NewReaction {
            account_id: remote.id,
            status_id: remote_status.id,
            name: "blobcat",
            custom_emoji_url: Some("https://remote.example/emoji/blobcat.png"),
            uri: Some("https://remote.example/users/bob#reactions/1"),
        },
        emoji.id,
        managed.origin_id,
    )
    .await
    .unwrap();

    // A remote emoji not already on the post can't start a reaction.
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PUT",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/notthere@remote.example",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    // The bare shortcode of a remote emoji isn't a local emoji either.
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PUT",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/:blobcat:",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // The qualified form joins bob's existing reaction.
    let (code, joined) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PUT",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/blobcat@remote.example",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let chip = &joined["pleroma"]["emoji_reactions"][0];
    assert_eq!(chip["name"], "blobcat@remote.example");
    assert_eq!(chip["count"], 2);
    assert_eq!(chip["me"], true);
    // The remote emoji image is proxied through the instance, never hot-linked.
    let chip_url = chip["url"].as_str().unwrap();
    assert!(
        chip_url.starts_with("https://plamenu.test/media/proxy/emoji/"),
        "{chip_url}"
    );
    assert!(!chip_url.contains("remote.example"), "{chip_url}");
    assert_eq!(chip["account_ids"][1], alice.id.to_string());

    deliver_all_due(&state).await;
    let react_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(react_delivery.inbox_url, bob.actor.inbox);
    assert_eq!(react_delivery.activity["type"], "EmojiReact");
    assert_eq!(react_delivery.activity["content"], ":blobcat:");
    assert_eq!(react_delivery.activity["tag"][0]["name"], ":blobcat:");
    assert_eq!(
        react_delivery.activity["tag"][0]["icon"]["url"],
        "https://remote.example/emoji/blobcat.png"
    );
    let react_uri = react_delivery.activity["id"].as_str().unwrap().to_owned();

    // Undo by the qualified name retracts the join with the same emoji.
    let (code, unreacted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "DELETE",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/blobcat@remote.example",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        unreacted["pleroma"]["emoji_reactions"][0]["count"], 1,
        "bob's own reaction stays"
    );
    deliver_all_due(&state).await;
    let undo_delivery = stub.deliveries().last().unwrap().clone();
    assert_eq!(undo_delivery.activity["type"], "Undo");
    assert_eq!(undo_delivery.activity["object"]["id"], react_uri);
    assert_eq!(undo_delivery.activity["object"]["content"], ":blobcat:");

    // Removing it again is the usual idempotent no-op.
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "DELETE",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/blobcat@remote.example",
            remote_status.id
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

/// An interaction chased immediately by its
/// retraction is coalesced in the delivery queue — neither activity goes out
/// (once both are on the wire, the remote's concurrent inbox processing can
/// apply the `Undo` before the `Announce` and the boost sticks remotely).
/// A retraction of an already-delivered interaction still federates.
#[sqlx::test(migrations = "../db/migrations")]
async fn instant_retractions_cancel_queued_interactions(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let remote_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/11",
            account_id: remote.id,
            content: "<p>flip-flop target</p>",
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

    // Favourite/unfavourite back to back: the queued Like is cancelled and
    // no Undo replaces it.
    let path = |action: &str| format!("/api/v1/statuses/{}/{action}", remote_status.id);
    let (code, _) = api(app(), "POST", &path("favourite"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(app(), "POST", &path("unfavourite"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 0);

    // Boost/unboost back to back: same for the Announce.
    let (code, _) = api(app(), "POST", &path("reblog"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(app(), "POST", &path("unreblog"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 0);

    // React/unreact back to back: same for the EmojiReact.
    let react_path = format!(
        "/api/v1/pleroma/statuses/{}/reactions/%F0%9F%94%A5",
        remote_status.id
    );
    let (code, _) = api(app(), "PUT", &react_path, Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(app(), "DELETE", &react_path, Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(plamenu_db::job::pending_count(&pool).await.unwrap(), 0);

    assert_eq!(deliver_all_due(&state).await, 0);
    assert!(
        stub.deliveries().is_empty(),
        "a coalesced flip-flop must deliver nothing"
    );

    // Favourite again, but let the Like be delivered this time: the
    // unfavourite then federates a real Undo(Like).
    let (code, _) = api(app(), "POST", &path("favourite"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(deliver_all_due(&state).await, 1);
    assert_eq!(stub.deliveries().last().unwrap().activity["type"], "Like");

    let (code, _) = api(app(), "POST", &path("unfavourite"), Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(deliver_all_due(&state).await, 1);
    let undo = stub.deliveries().last().unwrap().clone();
    assert_eq!(undo.activity["type"], "Undo");
    assert_eq!(undo.activity["object"]["type"], "Like");
}

fn sample_png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        48,
        image::Rgb([200, 100, 50]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

/// Builds a `multipart/form-data` body with a file and a description.
fn multipart_upload(file: &[u8], description: &str) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "plamenu-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"t.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; \
             name=\"description\"\r\n\r\n{description}\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn media_upload_attach_serve_and_federate(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();
    let app = || test_app_with(pool.clone(), stub.clone());

    // Upload (multipart, like real clients).
    let (content_type, body) = multipart_upload(&sample_png_bytes(), "a test image");
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/media")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let response = build_router_state(&state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(uploaded["type"], "image");
    assert_eq!(uploaded["description"], "a test image");
    assert_eq!(uploaded["meta"]["original"]["width"], 64);
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let media_url = uploaded["url"].as_str().unwrap().to_owned();
    // The default full-media setting is `passthrough`: the PNG is stored as
    // uploaded (metadata stripped), not re-encoded.
    assert!(
        std::path::Path::new(&media_url)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("png")),
        "{media_url}"
    );

    // The stored file is served with the right content type.
    let path = media_url.strip_prefix("https://plamenu.test").unwrap();
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    let response = build_router_state(&state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");

    // Update the alt text.
    let (code, updated) = api(
        app(),
        "PUT",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        Some(json!({"description": "better alt text"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(updated["description"], "better alt text");

    // A media-only post (no text) attaches it.
    let (code, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    assert_eq!(posted["media_attachments"][0]["url"], media_url.as_str());
    assert_eq!(
        posted["media_attachments"][0]["description"],
        "better alt text"
    );

    // Reusing attached media must fail.
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "again", "media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // The federated Create carries the attachment as a Document.
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let create = &sent.last().unwrap().activity;
    assert_eq!(create["type"], "Create");
    assert_eq!(create["object"]["attachment"][0]["type"], "Document");
    assert_eq!(create["object"]["attachment"][0]["mediaType"], "image/png");
    assert_eq!(create["object"]["attachment"][0]["url"], media_url.as_str());
}

fn build_router_state(state: &plamenu::AppState) -> Router {
    plamenu::build_router(state.clone())
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_content_is_sanitized_and_attachments_stored(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/200", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>safe</p><script>alert('xss')</script>\
                        <img src=\"https://evil.example/t.png\">\
                        <a href=\"javascript:boom()\">link</a>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/png",
                    "url": "https://remote.example/files/cat.png",
                    "name": "a remote cat",
                },
                { "type": "Document", "url": "http://insecure.example/no.png" },
            ],
        },
    });
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &format!("{}/statuses/200", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.content.contains("script"), "{}", stored.content);
    assert!(!stored.content.contains("img"), "{}", stored.content);
    assert!(
        !stored.content.contains("javascript:"),
        "{}",
        stored.content
    );
    assert!(stored.content.contains("<p>safe</p>"));

    // The https attachment was kept; the http one dropped.
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    let attachments = shown["media_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    // Both url and remote_url are proxied through the instance, never the origin.
    for key in ["url", "remote_url"] {
        let url = attachments[0][key].as_str().unwrap();
        assert!(
            url.starts_with("https://plamenu.test/media/proxy/attachment/"),
            "{key} = {url}"
        );
        assert!(!url.contains("remote.example"), "{key} leaks origin: {url}");
    }
    assert_eq!(attachments[0]["description"], "a remote cat");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mentions_resolve_render_notify_and_deliver(pool: PgPool) {
    common::open_previews(&pool).await;
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, _carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let (code, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "hi @carol and @bob@remote.example, check #Plamenu!"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");

    // HTML carries mention + hashtag anchors; plain @/# text is gone.
    let content = posted["content"].as_str().unwrap();
    assert!(content.contains(r#"class="u-url mention""#), "{content}");
    assert!(content.contains(r#"class="mention hashtag""#), "{content}");
    assert!(content.contains("/tags/plamenu"), "{content}");

    // Entity arrays.
    let mentions = posted["mentions"].as_array().unwrap();
    let mut accts: Vec<&str> = mentions
        .iter()
        .map(|m| m["acct"].as_str().unwrap())
        .collect();
    accts.sort_unstable();
    assert_eq!(accts, ["bob@remote.example", "carol"]);
    assert_eq!(posted["tags"][0]["name"], "plamenu");

    // Carol got a mention notification.
    let items = plamenu_db::notification::list(
        &pool,
        carol.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "mention");

    // Bob gets the Create even without following alice; the Note carries
    // the tag objects and bob's uri in cc.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    let create = &sent.last().unwrap().activity;
    assert_eq!(sent.last().unwrap().inbox_url, bob.actor.inbox);
    let tag_types: Vec<&str> = create["object"]["tag"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["type"].as_str().unwrap())
        .collect();
    assert!(tag_types.contains(&"Mention"));
    assert!(tag_types.contains(&"Hashtag"));
    assert!(
        create["cc"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == &json!(bob.actor.id)),
        "{:?}",
        create["cc"]
    );

    // The tag timeline lists the post (anonymously).
    let (code, timeline) = api(app(), "GET", "/api/v1/timelines/tag/PLAMENU", None, None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(timeline.as_array().unwrap().len(), 1);
    assert_eq!(timeline[0]["id"], posted["id"]);

    // Unresolvable mentions degrade to plain text.
    let (_, fallback) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "@ghost@nowhere.example hello"})),
    )
    .await;
    assert!(
        fallback["content"]
            .as_str()
            .unwrap()
            .contains("@ghost@nowhere.example")
    );
    assert!(fallback["mentions"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_mentions_notify_and_grant_private_access(pool: PgPool) {
    common::open_previews(&pool).await;
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // A followers-only Note from bob that mentions alice (who does not
    // follow bob): she must still be able to see it.
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": format!("{}/statuses/300", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>psst @alice</p>",
            "to": [format!("{}/followers", bob.actor.id)],
            "tag": [
                {"type": "Mention", "href": "https://plamenu.test/users/alice",
                 "name": "@alice@plamenu.test"},
                {"type": "Hashtag", "href": "https://remote.example/tags/secret",
                 "name": "#Secret"},
            ],
        },
    });
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &format!("{}/statuses/300", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.visibility, "private");

    // Mention notification arrived.
    let items = plamenu_db::notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(items[0].kind, "mention");

    // Alice can read it; anonymous viewers cannot.
    let (code, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(shown["mentions"][0]["acct"], "alice");
    let (code, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // The inbound hashtag is queryable but, being non-public, not listed.
    let (_, timeline) = api(app(), "GET", "/api/v1/timelines/tag/secret", None, None).await;
    assert!(timeline.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_remote_mentions_are_stored_silently(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("other.example", "carol");
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // A public Note from bob mentioning carol, whom we have never seen:
    // her actor must be fetched so the entity's `mentions` carry her (and
    // the web client can keep her mention anchor in-app).
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": format!("{}/statuses/301", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": format!("<p>hi <a href=\"{}\" class=\"u-url mention\">@carol</a></p>", carol.actor.id),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "tag": [
                {"type": "Mention", "href": carol.actor.id,
                 "name": "@carol@other.example"},
            ],
        },
    });
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // Carol's actor was fetched and stored.
    let stored_carol = plamenu_db::account::find_by_uri(&pool, &carol.actor.id)
        .await
        .unwrap()
        .expect("mentioned actor stored");
    assert_eq!(stored_carol.username, "carol");

    // The status entity lists her as a mention, silently (no notification
    // exists — she is not local).
    let stored = status::find_by_uri(&pool, &format!("{}/statuses/301", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let (code, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(shown["mentions"][0]["acct"], "carol@other.example");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_emoji_reactions_surface_to_clients(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "react to me",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // bob reacts with a Unicode emoji; dave with the same one (count 2).
    let unicode_react = |actor: &str, n: u8| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor}#reactions/{n}"),
            "type": "EmojiReact",
            "actor": actor,
            "content": "😀",
            "object": note_uri,
        })
    };
    assert_eq!(
        post_signed(app(), &unicode_react(&bob.actor.id, 1), &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &unicode_react(&dave.actor.id, 1), &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    // bob also reacts with a custom emoji carried in the activity's tag.
    let custom_react = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#reactions/2", bob.actor.id),
        "type": "EmojiReact",
        "actor": bob.actor.id,
        "content": ":blobcat:",
        "object": note_uri,
        "tag": [{
            "type": "Emoji",
            "name": ":blobcat:",
            "icon": { "type": "Image", "url": "https://remote.example/emoji/blobcat.png" },
        }],
    });
    assert_eq!(
        post_signed(app(), &custom_react, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // The status now carries pleroma.emoji_reactions: 😀 (×2) then
    // :blobcat:, the latter qualified Pleroma-style by its image's host.
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 2);
    assert_eq!(reactions[0]["name"], "😀");
    assert_eq!(reactions[0]["count"], 2);
    assert_eq!(reactions[0]["me"], false);
    assert!(reactions[0].get("url").is_none(), "unicode carries no url");
    assert_eq!(reactions[1]["name"], "blobcat@remote.example");
    assert_eq!(reactions[1]["count"], 1);
    // The remote emoji image is proxied through the instance, never hot-linked.
    let url = reactions[1]["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://plamenu.test/media/proxy/emoji/"),
        "{url}"
    );
    assert!(!url.contains("remote.example"), "{url}");

    // Alice was notified for each reaction, with the reacted emoji.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let reaction_notifs: Vec<&Value> = notifications
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "pleroma:emoji_reaction")
        .collect();
    assert_eq!(reaction_notifs.len(), 3);
    let emojis: Vec<&str> = reaction_notifs
        .iter()
        .filter_map(|n| n["emoji"].as_str())
        .collect();
    assert!(emojis.contains(&"😀"));
    assert!(emojis.contains(&":blobcat:"));

    // bob withdraws the Unicode reaction: count drops to 1, the custom one stays.
    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#undo/1", bob.actor.id),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": {
            "id": format!("{}#reactions/1", bob.actor.id),
            "type": "EmojiReact",
            "actor": bob.actor.id,
            "content": "😀",
            "object": note_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions[0]["name"], "😀");
    assert_eq!(reactions[0]["count"], 1);

    // The undone reaction's notification was cleared; :blobcat:'s survives.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let remaining: Vec<&str> = notifications
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "pleroma:emoji_reaction")
        .filter_map(|n| n["emoji"].as_str())
        .collect();
    assert_eq!(remaining, [":blobcat:", "😀"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_join_of_local_custom_emoji_uses_local_origin(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "join my local reaction",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    custom_emoji::create_local(&pool, "plamote", "plamote.png", "image/png", 1, None)
        .await
        .unwrap()
        .unwrap();

    let (code, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/pleroma/statuses/{}/reactions/plamote", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Pleroma-family peers echo our Emoji tag and public media URL when one
    // of their users joins the existing reaction.
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);
    let react = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#reactions/1", bob.actor.id),
        "type": "EmojiReact",
        "actor": bob.actor.id,
        "content": ":plamote:",
        "object": note_uri,
        "tag": [{
            "type": "Emoji",
            "name": ":plamote:",
            "icon": { "type": "Image", "url": "https://plamenu.test/media/plamote.png" },
        }],
    });
    assert_eq!(
        post_signed(app(), &react, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["name"], "plamote");
    assert_eq!(reactions[0]["count"], 2);
    assert_eq!(reactions[0]["me"], true);
    assert!(
        custom_emoji::lookup(&pool, &["plamote".to_owned()], Some("remote.example"))
            .await
            .unwrap()
            .is_empty(),
        "the echoed local emoji must not be registered as remote"
    );
}

/// Pleroma delivers the `EmojiReact`'s `Emoji` tag with a *bare* `name` (no
/// colons) while `content` stays colon-wrapped. The reaction must still pick
/// up the tag's image, record `custom_emoji_url`, and render its group under
/// the qualified `shortcode@domain` name — addressable by that name too.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_reaction_with_bare_emoji_tag_name_keeps_its_image(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "react to me, Pleroma-style",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    let react = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#reactions/1", bob.actor.id),
        "type": "EmojiReact",
        "actor": bob.actor.id,
        "content": ":blobcat:",
        "object": note_uri,
        "tag": [{
            "type": "Emoji",
            "name": "blobcat",
            "icon": { "type": "Image", "url": "https://remote.example/emoji/blobcat.png" },
        }],
    });
    assert_eq!(
        post_signed(app(), &react, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["name"], "blobcat@remote.example");
    // The remote emoji image is proxied through the instance, never hot-linked.
    let url = reactions[0]["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://plamenu.test/media/proxy/emoji/"),
        "{url}"
    );
    assert!(!url.contains("remote.example"), "{url}");

    // The reactors list accepts the qualified name clients echo back.
    let (status, listed) = api(
        app(),
        "GET",
        &format!(
            "/api/v1/pleroma/statuses/{}/reactions/blobcat@remote.example",
            stored.id
        ),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let groups = listed.as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "blobcat@remote.example");
    assert_eq!(groups[0]["accounts"].as_array().unwrap().len(), 1);
}

/// The notification kinds alice currently has, newest first.
async fn notification_kinds(pool: &PgPool, account_id: i64) -> Vec<String> {
    plamenu_db::notification::list(
        pool,
        account_id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        50,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|n| n.kind)
    .collect()
}

/// A peer replaying a backlog (as Pleroma has been seen doing for a whole
/// day) must be a no-op: every redelivered activity — Like, Announce (same
/// or fresh id), Create with mentions, `EmojiReact` — already produced its
/// rows and side effects the first time, and must not notify again nor
/// duplicate interaction rows.
#[sqlx::test(migrations = "../db/migrations")]
async fn redelivered_activities_do_not_renotify_or_duplicate(pool: PgPool) {
    let (alice, _alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "interact with me",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    let like = json!({
        "id": format!("{}#likes/1", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
    });
    let announce = json!({
        "id": format!("{}/statuses/99/activity", bob.actor.id),
        "type": "Announce",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
    });
    let mention_reply = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/100", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hi @alice</p>",
            "inReplyTo": note_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "tag": [{"type": "Mention", "href": "https://plamenu.test/users/alice",
                     "name": "@alice@plamenu.test"}],
        },
    });
    let react = json!({
        "id": format!("{}#reactions/1", bob.actor.id),
        "type": "EmojiReact",
        "actor": bob.actor.id,
        "content": "😀",
        "object": note_uri,
    });

    // First delivery, then the full backlog replayed twice more.
    for _ in 0..3 {
        for activity in [&like, &announce, &mention_reply, &react] {
            assert_eq!(
                post_signed(app(), activity, &bob.signer()).await,
                StatusCode::ACCEPTED
            );
        }
    }

    let mut kinds = notification_kinds(&pool, alice.id).await;
    kinds.sort();
    assert_eq!(
        kinds,
        ["favourite", "mention", "pleroma:emoji_reaction", "reblog"],
        "each interaction notifies exactly once"
    );

    // An Announce redelivered under a fresh activity id is still the same
    // boost — no new row, no new notification (Mastodon dedupes on the
    // (account, reblogged-status) pair).
    let renamed_announce = json!({
        "id": format!("{}/statuses/99/activity-replayed", bob.actor.id),
        "type": "Announce",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
    });
    assert_eq!(
        post_signed(app(), &renamed_announce, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        notification_kinds(&pool, alice.id).await.len(),
        4,
        "renamed Announce replay must not re-notify"
    );
    assert!(
        status::find_by_uri(&pool, renamed_announce["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none(),
        "renamed Announce replay must not store a second boost"
    );

    // The redelivered Create did not duplicate the reply either.
    let reply = status::find_by_uri(&pool, &format!("{}/statuses/100", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.content, "<p>hi @alice</p>");
}

/// Deduplication must never eat new information: a genuine edit (`Update`
/// with changed content), then a second one, both apply — while redelivering
/// the original `Create` or an already-applied `Update` in between stays a
/// no-op and never rolls the content back or re-notifies.
#[sqlx::test(migrations = "../db/migrations")]
async fn redelivery_dedup_still_applies_successive_edits(pool: PgPool) {
    let (alice, _alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let note_uri = format!("{}/statuses/200", bob.actor.id);
    let note = |content: &str, updated: Option<&str>| {
        let mut object = json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": content,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "tag": [{"type": "Mention", "href": "https://plamenu.test/users/alice",
                     "name": "@alice@plamenu.test"}],
        });
        if let Some(updated) = updated {
            object["updated"] = json!(updated);
        }
        object
    };
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note("<p>v1 @alice</p>", None),
    });
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "<p>v1 @alice</p>");

    // First edit applies.
    let update = |content: &str, updated: &str| {
        json!({
            "id": format!("{note_uri}#updates/{updated}"),
            "type": "Update",
            "actor": bob.actor.id,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": note(content, Some(updated)),
        })
    };
    let first_edit = update("<p>v2 @alice</p>", "2026-07-03T10:00:00Z");
    assert_eq!(
        post_signed(app(), &first_edit, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let edited = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.content, "<p>v2 @alice</p>");
    let first_edited_at = edited.edited_at.unwrap();

    // Replaying the original Create must not roll the edit back (the note
    // is already stored: redelivery is dropped whole) nor re-notify.
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    // Replaying the already-applied Update changes nothing either.
    assert_eq!(
        post_signed(app(), &first_edit, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after_replay = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_replay.content, "<p>v2 @alice</p>");
    assert_eq!(after_replay.edited_at, Some(first_edited_at));

    // A second genuine edit still lands — dedup must not overreact.
    let second_edit = update("<p>v3 @alice</p>", "2026-07-03T11:00:00Z");
    assert_eq!(
        post_signed(app(), &second_edit, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let final_state = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_state.content, "<p>v3 @alice</p>");
    assert_ne!(final_state.edited_at, Some(first_edited_at));

    // A replayed *stale* Update (yesterday's edit arriving after a newer
    // one already applied) must not roll the post back — Mastodon's
    // `already_updated_more_recently?` rejection.
    assert_eq!(
        post_signed(app(), &first_edit, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after_stale = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_stale.content, "<p>v3 @alice</p>");

    // Through it all, alice was mentioned-notified exactly once.
    assert_eq!(notification_kinds(&pool, alice.id).await, ["mention"]);
}

/// Repeating a local favourite/boost/react POST is idempotent: one
/// notification, one delivery, one boost row — like Mastodon's services.
#[sqlx::test(migrations = "../db/migrations")]
async fn repeated_local_interactions_notify_once(pool: PgPool) {
    let (alice, _alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let stub = StubFederation::with_actors([]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "double-tap me",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    for path in [
        format!("/api/v1/statuses/{}/favourite", stored.id),
        format!("/api/v1/statuses/{}/reblog", stored.id),
    ] {
        for _ in 0..2 {
            let (code, _) = api(app(), "POST", &path, Some(&carol_token), None).await;
            assert_eq!(code, StatusCode::OK);
        }
    }

    let mut kinds = notification_kinds(&pool, alice.id).await;
    kinds.sort();
    assert_eq!(kinds, ["favourite", "reblog"]);

    // Exactly one boost row exists for carol.
    let carol_account = plamenu_db::account::find_local_by_username(&pool, "carol")
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, carol_account.id, stored.id)
            .await
            .unwrap()
            .is_some()
    );
}

/// Sharkey federates every reaction as a `Like` carrying
/// `_misskey_reaction` (mirrored into `content`) — even its default one, to
/// any peer it doesn't recognise as Mastodon-family. It must land as an
/// emoji reaction, not a favourite, survive redelivery, and be retracted by
/// Sharkey's `Undo`, which embeds the rendered Like under the same id.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_like_with_reaction_is_an_emoji_reaction(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("sharkey.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "react to me, sharkey",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // Shaped like the Like activity Sharkey 2025.5.2 puts on the wire.
    let like_uri = "https://sharkey.example/likes/9xg000001".to_owned();
    let like = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": like_uri,
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
        "content": "🍮",
        "_misskey_reaction": "🍮",
    });
    for _ in 0..2 {
        // Redelivery must not duplicate or re-notify.
        assert_eq!(
            post_signed(app(), &like, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0, "a reaction, not a favourite");
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["name"], "🍮");
    assert_eq!(reactions[0]["count"], 1);

    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let kinds: Vec<&str> = notifications
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["pleroma:emoji_reaction"],
        "no favourite notification"
    );

    // Sharkey's Undo embeds the full rendered Like under `<like id>/undo`.
    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{like_uri}/undo"),
        "type": "Undo",
        "actor": bob.actor.id,
        "object": like,
    });
    for _ in 0..2 {
        // A repeated Undo is a no-op, never an error.
        assert_eq!(
            post_signed(app(), &undo, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert!(
        shown["pleroma"]["emoji_reactions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "undo removed the reaction"
    );
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert!(
        notifications.as_array().unwrap().is_empty(),
        "the reaction notification went with it"
    );
}

/// The Like-dialect fallbacks — a custom-emoji reaction resolved from
/// the tagged `Emoji`, a `content`-only reaction (no `_misskey_reaction`),
/// and the legacy `name`-only form — plus an `Undo` that embeds the Like
/// without a reusable id, which must fall back to `(status, emoji)` matching.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_like_reaction_fallback_shapes(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("sharkey.example", "bob");
    let dave = RemoteUser::new("sharkey.example", "dave");
    let erin = RemoteUser::new("sharkey.example", "erin");
    let stub =
        StubFederation::with_actors([bob.actor.clone(), dave.actor.clone(), erin.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "dialect zoo",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // bob: custom emoji, Misskey-style colon-wrapped tag name.
    let custom_like = json!({
        "id": "https://sharkey.example/likes/custom1",
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
        "content": ":shonk:",
        "_misskey_reaction": ":shonk:",
        "tag": [{
            "type": "Emoji",
            "id": "https://sharkey.example/emojis/shonk",
            "name": ":shonk:",
            "icon": { "type": "Image", "mediaType": "image/png", "url": "https://sharkey.example/files/shonk.png" },
        }],
    });
    // dave: `content` only (no `_misskey_reaction`).
    let content_like = json!({
        "id": "https://sharkey.example/likes/content1",
        "type": "Like",
        "actor": dave.actor.id,
        "object": note_uri,
        "content": "🦈",
    });
    // erin: legacy `name` only.
    let name_like = json!({
        "id": "https://sharkey.example/likes/name1",
        "type": "Like",
        "actor": erin.actor.id,
        "object": note_uri,
        "name": "🦈",
    });
    for (activity, signer) in [
        (&custom_like, bob.signer()),
        (&content_like, dave.signer()),
        (&name_like, erin.signer()),
    ] {
        assert_eq!(
            post_signed(app(), activity, &signer).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0);
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 2, "{reactions:?}");
    assert_eq!(reactions[0]["name"], "shonk@sharkey.example");
    assert_eq!(reactions[0]["count"], 1);
    assert!(
        reactions[0]["url"].as_str().is_some(),
        "custom emoji keeps its image"
    );
    assert_eq!(reactions[1]["name"], "🦈");
    assert_eq!(
        reactions[1]["count"], 2,
        "content and name forms both counted"
    );

    // An id-less Undo still finds dave's reaction by (status, emoji).
    let undo = json!({
        "type": "Undo",
        "actor": dave.actor.id,
        "object": {
            "type": "Like",
            "actor": dave.actor.id,
            "object": note_uri,
            "content": "🦈",
        },
    });
    assert_eq!(
        post_signed(app(), &undo, &dave.signer()).await,
        StatusCode::ACCEPTED
    );
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    let shark = reactions.iter().find(|r| r["name"] == "🦈").unwrap();
    assert_eq!(shark["count"], 1, "only dave's reaction was withdrawn");
}

/// `EmojiReaction` is the legacy Misskey-family alias of `EmojiReact` —
/// accepted on create and on both `Undo` shapes (embedded object and bare
/// activity IRI).
#[sqlx::test(migrations = "../db/migrations")]
async fn emoji_reaction_type_aliases_emoji_react(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("legacy.example", "bob");
    let dave = RemoteUser::new("legacy.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "legacy reactions",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    let react = |actor: &str, n: u8| {
        json!({
            "id": format!("{actor}#reactions/{n}"),
            "type": "EmojiReaction",
            "actor": actor,
            "content": "😀",
            "object": note_uri,
        })
    };
    let bob_react = react(&bob.actor.id, 1);
    let dave_react = react(&dave.actor.id, 1);
    assert_eq!(
        post_signed(app(), &bob_react, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &dave_react, &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions[0]["count"], 2);

    // bob undoes with the EmojiReaction embedded; dave by its bare IRI.
    let embedded_undo = json!({
        "type": "Undo",
        "actor": bob.actor.id,
        "object": bob_react,
    });
    let iri_undo = json!({
        "type": "Undo",
        "actor": dave.actor.id,
        "object": format!("{}#reactions/1", dave.actor.id),
    });
    assert_eq!(
        post_signed(app(), &embedded_undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &iri_undo, &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert!(
        shown["pleroma"]["emoji_reactions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "both undo shapes retract"
    );
}

/// Sharkey's legacy `Dislike` is a withdrawal of the actor's
/// reaction/favourite on the target — idempotent, validated against what the
/// sender actually has there, and never stored as anything.
#[sqlx::test(migrations = "../db/migrations")]
async fn dislike_withdraws_the_senders_reaction_and_favourite(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("sharkey.example", "bob");
    let dave = RemoteUser::new("sharkey.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "dislike me",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // bob favourites (plain Like) and reacts; dave reacts with the same emoji.
    let plain_like = json!({
        "id": format!("{}#likes/1", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
    });
    let react = |actor: &str| {
        json!({
            "id": format!("{actor}#reactions/1"),
            "type": "EmojiReact",
            "actor": actor,
            "content": "😀",
            "object": note_uri,
        })
    };
    assert_eq!(
        post_signed(app(), &plain_like, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &react(&bob.actor.id), &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &react(&dave.actor.id), &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let dislike = json!({
        "id": format!("{}#dislikes/1", bob.actor.id),
        "type": "Dislike",
        "actor": bob.actor.id,
        "object": note_uri,
    });
    for _ in 0..2 {
        // Idempotent: a repeat withdraws nothing further.
        assert_eq!(
            post_signed(app(), &dislike, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0, "bob's favourite withdrawn");
    let reactions = shown["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["count"], 1, "dave's reaction untouched");

    // Only dave's reaction notification remains for alice.
    let mut kinds = notification_kinds(&pool, alice.id).await;
    kinds.sort();
    assert_eq!(kinds, ["pleroma:emoji_reaction"]);

    // A Dislike of something unknown is quietly ignored.
    let stray = json!({
        "id": format!("{}#dislikes/2", bob.actor.id),
        "type": "Dislike",
        "actor": bob.actor.id,
        "object": "https://plamenu.test/users/alice/statuses/999999",
    });
    assert_eq!(
        post_signed(app(), &stray, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
}

/// A Like whose "reaction" cannot be one (smuggled text far past any
/// emoji length) degrades to a plain favourite instead of being dropped —
/// and its Undo still retracts that favourite through the fall-through path.
#[sqlx::test(migrations = "../db/migrations")]
async fn unparseable_like_reaction_degrades_to_favourite(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("sharkey.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &test_state_with(pool.clone(), stub.clone()),
        PostParams {
            username: "alice",
            text: "almost a reaction",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let note_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    let like = json!({
        "id": "https://sharkey.example/likes/weird1",
        "type": "Like",
        "actor": bob.actor.id,
        "object": note_uri,
        "_misskey_reaction": "definitely not an emoji, just a lot of text",
    });
    assert_eq!(
        post_signed(app(), &like, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 1);
    assert!(
        shown["pleroma"]["emoji_reactions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let undo = json!({
        "type": "Undo",
        "actor": bob.actor.id,
        "object": like,
    });
    assert_eq!(
        post_signed(app(), &undo, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["favourites_count"], 0);
}
