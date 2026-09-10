//! FEP-171b / FEP-f228 conversation privacy & containers:
//! a reply's audience is copied down from its conversation root, never widened
//! — on both the compose and the ingest side — and a private/direct reply
//! inherits the parent's audience so it reaches exactly the thread. The private
//! conversation container (`contextHistory`) is served only when enabled and
//! only to an authorized requester.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    TEST_DOMAIN, create_local_account, test_app, test_app_containers, test_state_containers,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::AppState;
use plamenu::actions::{self, PostParams};
use plamenu_db::account::{self, RemoteAccountData};
use plamenu_db::{PgPool, conversation, mention, status};
use serde_json::{Value, json};
use tower::ServiceExt;

const AP_JSON: &str = "application/activity+json";

async fn get_json(app: Router, uri: &str) -> (StatusCode, HeaderMap, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::ACCEPT, AP_JSON)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

async fn post_with(
    state: &AppState,
    username: &str,
    text: &str,
    visibility: &str,
    in_reply_to_id: Option<i64>,
) -> status::Status {
    actions::post_status(
        state,
        PostParams {
            username,
            text,
            visibility,
            in_reply_to_id,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0
}

async fn post(
    pool: &PgPool,
    username: &str,
    text: &str,
    visibility: &str,
    in_reply_to_id: Option<i64>,
) -> status::Status {
    post_with(
        &test_state_with(pool.clone(), Arc::default()),
        username,
        text,
        visibility,
        in_reply_to_id,
    )
    .await
}

async fn cheap_remote(pool: &PgPool, username: &str) -> account::Account {
    let uri = format!("https://remote.example/users/{username}");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain: "remote.example",
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: &format!("{uri}#main-key"),
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

/// A local reply may not widen the audience past its conversation root.
#[sqlx::test(migrations = "../db/migrations")]
async fn compose_reply_visibility_clamped_to_root(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;

    // A followers-only root that names Bob (so he may see and reply). Bob
    // answers "public" — it is clamped to private.
    let private_root = post(&pool, "alice", "@bob followers only", "private", None).await;
    let widening = post(&pool, "bob", "shout it", "public", Some(private_root.id)).await;
    assert_eq!(widening.visibility, "private", "public reply clamped down");

    // Narrowing is honoured: a direct reply into the same thread stays direct.
    let narrowing = post(&pool, "bob", "just you", "direct", Some(private_root.id)).await;
    assert_eq!(narrowing.visibility, "direct");

    // A public root clamps nothing.
    let public_root = post(&pool, "alice", "open thread", "public", None).await;
    let public_reply = post(&pool, "bob", "hi all", "public", Some(public_root.id)).await;
    assert_eq!(public_reply.visibility, "public");
    let unlisted_reply = post(&pool, "bob", "quietly", "unlisted", Some(public_root.id)).await;
    assert_eq!(unlisted_reply.visibility, "unlisted", "narrowing allowed");
}

/// A private/direct reply inherits the parent's audience, so it reaches the
/// thread even when the author did not re-mention anyone.
#[sqlx::test(migrations = "../db/migrations")]
async fn compose_direct_reply_inherits_parent_audience(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;

    // Alice DMs Bob.
    let dm = post(&pool, "alice", "@bob lunch?", "direct", None).await;
    // Bob answers without re-mentioning Alice.
    let reply = post(&pool, "bob", "yes!", "direct", Some(dm.id)).await;

    // The reply is addressed to Alice (a silent, inherited recipient).
    // `false` = include silent recipients (the inherited audience is silent).
    let recipients = mention::for_statuses(&pool, &[reply.id], false)
        .await
        .unwrap()
        .remove(&reply.id)
        .unwrap_or_default();
    assert!(
        recipients.iter().any(|a| a.id == alice.id),
        "the reply inherits the parent author as a recipient"
    );
    assert!(
        !recipients.iter().any(|a| a.id == bob.id),
        "the reply author is not mentioned in their own reply"
    );
}

/// An inbound remote reply into a locally-rooted private conversation cannot be
/// ingested as more public than the conversation, even with a forged wider
/// `to`/`cc`.
#[sqlx::test(migrations = "../db/migrations")]
async fn ingest_reply_cannot_widen_private_conversation(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    create_local_account(&pool, "alice", "Alice").await;
    let carol = cheap_remote(&pool, "carol").await;

    let private_root = post(&pool, "alice", "@carol secret", "private", None).await;
    let root_uri = format!(
        "https://{TEST_DOMAIN}/users/alice/statuses/{}",
        private_root.id
    );

    // Carol's reply forges a public audience.
    let object = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/statuses/forged",
        "type": "Note",
        "attributedTo": carol.uri,
        "content": "<p>leak it</p>",
        "inReplyTo": root_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
    });
    let stored = plamenu::ingest::ingest_remote_note(&state, &carol, &object)
        .await
        .unwrap();
    assert_eq!(
        stored.in_reply_to_id,
        Some(private_root.id),
        "the reply threads onto the private root"
    );
    assert_eq!(
        stored.visibility, "private",
        "forged public audience clamped to the conversation's visibility"
    );
}

/// The FEP-171b container wraps a private conversation's posts as
/// `Add(Create(Note))`, attributed to the owner.
#[sqlx::test(migrations = "../db/migrations")]
async fn container_wraps_private_conversation_posts(pool: PgPool) {
    let state = test_state_containers(pool.clone(), Arc::default());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;

    let root = post_with(&state, "alice", "@bob secret", "private", None).await;
    let reply = post_with(&state, "bob", "understood", "private", Some(root.id)).await;

    let conv_id = conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();
    let conv = conversation::find(&pool, conv_id).await.unwrap().unwrap();
    let items = plamenu::containers::history_items(&state, &conv, &alice, &root, 0, 60)
        .await
        .unwrap();

    assert_eq!(items.len(), 2, "root and reply, chronological");
    let owner_uri = format!("https://{TEST_DOMAIN}/users/alice");
    let container_uri = format!("https://{TEST_DOMAIN}/contexts/{conv_id}/history");
    for (item, post) in items.iter().zip([&root, &reply]) {
        assert_eq!(item["type"], "Add");
        assert_eq!(item["actor"], owner_uri, "the owner distributes the Add");
        assert_eq!(item["target"]["type"], "OrderedCollection");
        assert_eq!(item["target"]["id"], container_uri);
        assert_eq!(item["object"]["type"], "Create");
        assert_eq!(item["object"]["object"]["type"], "Note");
        assert_eq!(item["id"], format!("{container_uri}/{}", post.id));
    }
}

/// The container is served only when enabled, and only to an authorized
/// (verified, in-audience) requester — never to an unsigned request.
#[sqlx::test(migrations = "../db/migrations")]
async fn container_route_is_gated(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let root = post(&pool, "alice", "secret", "private", None).await;
    let conv_id = conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();
    let path = format!("/contexts/{conv_id}/history");

    // Flag off: the instance runs no container.
    let (status, _, _) = get_json(test_app(pool.clone()), &path).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no container when disabled");

    // Flag on but unsigned: a private container is never served to an
    // unauthenticated request — the 404 does not reveal the thread exists.
    let (status, _, _) = get_json(test_app_containers(pool.clone()), &path).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unsigned request denied");

    // A public conversation has no container either (it uses the posts
    // collection); requesting its history 404s even with the flag on.
    let public_root = post(&pool, "alice", "open", "public", None).await;
    let public_conv = conversation::of_status(&pool, public_root.id)
        .await
        .unwrap()
        .unwrap();
    let (status, _, _) = get_json(
        test_app_containers(pool.clone()),
        &format!("/contexts/{public_conv}/history"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "public thread has no container"
    );
}

/// With the container flag on, a private root advertises `contextHistory` (and
/// no public `context`); a public root advertises the reverse.
#[sqlx::test(migrations = "../db/migrations")]
async fn context_links_follow_visibility_and_flag(pool: PgPool) {
    let state = test_state_containers(pool.clone(), Arc::default());
    create_local_account(&pool, "alice", "Alice").await;

    let private_root = post_with(&state, "alice", "secret", "private", None).await;
    let (context, history) = plamenu::note::context_links_for(&state, &pool, &private_root)
        .await
        .unwrap();
    assert!(
        context.is_none(),
        "no public posts collection for a private root"
    );
    let conv_id = conversation::of_status(&pool, private_root.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        history.as_deref(),
        Some(format!("https://{TEST_DOMAIN}/contexts/{conv_id}/history").as_str()),
        "private root advertises the container",
    );

    let public_root = post_with(&state, "alice", "open", "public", None).await;
    let (context, history) = plamenu::note::context_links_for(&state, &pool, &public_root)
        .await
        .unwrap();
    assert!(
        context.is_some(),
        "public root advertises the posts collection"
    );
    assert!(history.is_none(), "public root has no container");
}
