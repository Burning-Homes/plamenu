//! The `ActivityPub` outbox: envelope `totalItems` (what Mastodon ingests
//! as a remote account's `statuses_count`) and the activity pages — through
//! the real router.

mod common;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_secure,
    test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::{entities, remote};
use plamenu_db::{PgPool, account_domain_block, block, follow, mention, status};
use plamenu_federation::RequestSigner;
use serde_json::Value;
use tower::ServiceExt;

const AP_JSON: &str = "application/activity+json";

async fn get_json(app: Router, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

async fn get_signed_json(
    app: Router,
    uri: &str,
    request_signer: &RequestSigner,
) -> (StatusCode, HeaderMap, Value) {
    let signed_headers = request_signer.sign_get(TEST_DOMAIN, uri, AP_JSON, SystemTime::now());
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", TEST_DOMAIN)
        .header("date", signed_headers.date)
        .header("signature", signed_headers.signature)
        .header(header::ACCEPT, AP_JSON)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// The inlined object/target ids of a page's activities, newest first.
fn item_objects(page: &Value) -> Vec<String> {
    page["orderedItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let object = &item["object"];
            object["id"]
                .as_str()
                .or_else(|| object.as_str())
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Posts through the real compose pipeline.
async fn post(pool: &PgPool, username: &str, text: &str, visibility: &str) -> status::Status {
    actions::post_status(
        &test_state_with(pool.clone(), Arc::default()),
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
    .unwrap()
    .0
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_envelope_counts_federatable_statuses_only(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;
    post(&pool, "alice", "public post", "public").await;
    post(&pool, "alice", "unlisted post", "unlisted").await;
    post(&pool, "alice", "followers only", "private").await;
    post(&pool, "alice", "local only", "local").await;
    post(&pool, "alice", "@bob psst", "direct").await;

    let outbox = format!("https://{TEST_DOMAIN}/users/alice/outbox");
    let (status, headers, body) =
        get_json(test_app(pool.clone()), "/users/alice/outbox", Some(AP_JSON)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/activity+json; charset=utf-8"
    );
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["id"], outbox.as_str());
    assert_eq!(
        body["totalItems"], 3,
        "direct and local-only posts are not counted"
    );
    assert_eq!(body["first"], format!("{outbox}?page=true"));
    assert_eq!(body["last"], format!("{outbox}?min_id=0&page=true"));
    assert!(!body.as_object().unwrap().contains_key("orderedItems"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_page_inlines_creates_and_announces(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;
    let post_public = post(&pool, "alice", "hello world", "public").await;
    post(&pool, "alice", "followers only", "private").await;
    post(&pool, "alice", "local only", "local").await;
    let bob_post = post(&pool, "bob", "boost me", "public").await;
    let state = test_state_with(pool.clone(), Arc::default());
    let boost = actions::reblog_status(&state, &alice, bob_post.id)
        .await
        .unwrap();

    let outbox = format!("https://{TEST_DOMAIN}/users/alice/outbox");
    let (status, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?page=true",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["type"], "OrderedCollectionPage");
    assert_eq!(page["id"], format!("{outbox}?page=true"));
    assert_eq!(page["partOf"], outbox.as_str());
    assert!(
        !page.as_object().unwrap().contains_key("totalItems"),
        "pages carry no totalItems, like Mastodon"
    );
    // A short page has a prev (towards newer) but no next.
    assert_eq!(
        page["prev"],
        format!("{outbox}?min_id={}&page=true", boost.id)
    );
    assert!(!page.as_object().unwrap().contains_key("next"));

    let items = page["orderedItems"].as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "the private and local-only posts are not distributable"
    );

    // Newest first: the boost as a bare-object Announce…
    let announce = &items[0];
    assert_eq!(announce["type"], "Announce");
    assert_eq!(
        announce["id"],
        format!(
            "https://{TEST_DOMAIN}/users/alice/statuses/{}/activity",
            boost.id
        )
    );
    assert_eq!(
        announce["actor"],
        format!("https://{TEST_DOMAIN}/users/alice")
    );
    assert_eq!(
        announce["object"],
        format!("https://{TEST_DOMAIN}/users/bob/statuses/{}", bob_post.id)
    );
    assert!(!announce.as_object().unwrap().contains_key("@context"));

    // …then the public post as a Create with the full Note inline.
    let create = &items[1];
    let note_id = format!(
        "https://{TEST_DOMAIN}/users/alice/statuses/{}",
        post_public.id
    );
    assert_eq!(create["type"], "Create");
    assert_eq!(create["id"], format!("{note_id}/activity"));
    assert_eq!(create["object"]["id"], note_id);
    assert_eq!(create["object"]["type"], "Note");
    assert_eq!(
        create["object"]["content"], "<p>hello world</p>",
        "full Note rendering, same as the object endpoint"
    );
    assert_eq!(create["to"], create["object"]["to"]);
    assert!(!create.as_object().unwrap().contains_key("@context"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_pages_paginate_by_keyset(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let mut ids = Vec::new();
    for i in 1..=21 {
        ids.push(
            post(&pool, "alice", &format!("post {i}"), "public")
                .await
                .id,
        );
    }

    let outbox = format!("https://{TEST_DOMAIN}/users/alice/outbox");
    let (_, _, page1) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?page=true",
        Some(AP_JSON),
    )
    .await;
    let items = page1["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 20);
    assert_eq!(
        items[0]["object"]["id"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", ids[20]),
        "newest first"
    );
    // A full page advertises the next (older) one keyed by its last item.
    assert_eq!(
        page1["next"],
        format!("{outbox}?max_id={}&page=true", ids[1])
    );

    let (_, _, page2) = get_json(
        test_app(pool.clone()),
        &format!("/users/alice/outbox?max_id={}&page=true", ids[1]),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(page2["id"], format!("{outbox}?max_id={}&page=true", ids[1]));
    let items = page2["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(!page2.as_object().unwrap().contains_key("next"));

    // The envelope's `last` link: the oldest 20, still newest-first.
    let (_, _, last) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?min_id=0&page=true",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(last["id"], format!("{outbox}?min_id=0&page=true"));
    let items = last["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 20);
    assert_eq!(
        items[0]["object"]["id"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", ids[19]),
        "min_id pages hold the rows immediately above it, newest of those first"
    );
    assert_eq!(
        items[19]["object"]["id"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", ids[0])
    );
    assert_eq!(
        last["prev"],
        format!("{outbox}?min_id={}&page=true", ids[19])
    );
}

/// A signed page GET is served viewer-aware, like Mastodon's
/// `AccountStatusesFilter` keyed on `signed_request_account`: an accepted
/// follower also receives followers-only statuses, and a mentioned requester
/// the statuses that mention them — direct messages included.
#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_pages_are_viewer_aware(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let app = || test_app_with(pool.clone(), StubFederation::with_users(&[&bob]));

    let post_public = post(&pool, "alice", "public post", "public").await;
    let post_unlisted = post(&pool, "alice", "unlisted post", "unlisted").await;
    let post_private = post(&pool, "alice", "followers only", "private").await;
    post(&pool, "alice", "local only", "local").await;
    let post_direct = post(&pool, "alice", "psst", "direct").await;
    let note = |status: &status::Status| {
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", status.id)
    };

    // A signed stranger gets the same slice as an anonymous requester.
    let (status, headers, page) =
        get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::VARY],
        "Signature",
        "pages vary by signer, like Mastodon"
    );
    assert_eq!(
        item_objects(&page),
        vec![note(&post_unlisted), note(&post_public)]
    );

    // An accepted follower also gets the followers-only statuses.
    follow::create(&pool, bob_stored.id, alice.id, None)
        .await
        .unwrap();
    let (_, _, page) = get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(
        item_objects(&page),
        vec![
            note(&post_private),
            note(&post_unlisted),
            note(&post_public)
        ]
    );

    // A mentioned requester gets the statuses that mention them, however
    // they are addressed.
    mention::attach(&pool, post_direct.id, bob_stored.id, false)
        .await
        .unwrap();
    let (_, _, page) = get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(
        item_objects(&page),
        vec![
            note(&post_direct),
            note(&post_private),
            note(&post_unlisted),
            note(&post_public)
        ]
    );
    // The `min_id` branch (the envelope's `last` link) applies the same
    // filter.
    let (_, _, last) = get_signed_json(
        app(),
        "/users/alice/outbox?min_id=0&page=true",
        &bob.signer(),
    )
    .await;
    assert_eq!(item_objects(&last).len(), 4);

    // Anonymous requests still get only the distributable slice.
    let (_, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?page=true",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(
        item_objects(&page),
        vec![note(&post_unlisted), note(&post_public)]
    );
}

/// A requester the account blocks — directly or via a personal domain block
/// — is served an empty page (Mastodon's `Status.none` for `blocked?`),
/// never a 403 that would advertise the block.
#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_page_is_empty_for_blocked_requesters(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let app = || test_app_with(pool.clone(), StubFederation::with_users(&[&bob]));
    post(&pool, "alice", "public post", "public").await;

    block::create(&pool, alice.id, bob_stored.id, None)
        .await
        .unwrap();
    let (status, _, page) =
        get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["orderedItems"], Value::Array(vec![]));
    assert!(!page.as_object().unwrap().contains_key("next"));
    assert!(
        !page.as_object().unwrap().contains_key("prev"),
        "an empty page advertises no pagination, like Mastodon"
    );

    // Same for a personal domain block of the requester's domain.
    block::delete(&pool, alice.id, bob_stored.id).await.unwrap();
    account_domain_block::create(&pool, alice.id, "remote.example")
        .await
        .unwrap();
    let (_, _, page) = get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(page["orderedItems"], Value::Array(vec![]));

    // The block is invisible to everyone else.
    let (_, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?page=true",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(item_objects(&page).len(), 1);
}

/// Boosts of an author the requester blocks (or is blocked by) are withheld
/// from their page — the federated slice of Mastodon's
/// `excluded_from_timeline_account_ids` reblog filter.
#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_page_hides_boosts_of_authors_the_viewer_blocks(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let app = || test_app_with(pool.clone(), StubFederation::with_users(&[&bob]));

    let dave_post = post(&pool, "dave", "boost me", "public").await;
    let state = test_state_with(pool.clone(), Arc::default());
    actions::reblog_status(&state, &alice, dave_post.id)
        .await
        .unwrap();
    let own_post = post(&pool, "alice", "my own post", "public").await;

    // Without a block, the signed page carries both rows.
    let (_, _, page) = get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(item_objects(&page).len(), 2);

    // Bob blocks Dave (an inbound federated Block): Alice's boost of Dave
    // disappears from Bob's page, Alice's own post stays.
    block::create(&pool, bob_stored.id, dave.id, None)
        .await
        .unwrap();
    let (_, _, page) = get_signed_json(app(), "/users/alice/outbox?page=true", &bob.signer()).await;
    assert_eq!(
        item_objects(&page),
        vec![format!(
            "https://{TEST_DOMAIN}/users/alice/statuses/{}",
            own_post.id
        )]
    );

    // Anonymous pages are unaffected.
    let (_, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/outbox?page=true",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(item_objects(&page).len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbox_requires_ap_accept_and_known_user(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;

    let (status, _, _) = get_json(test_app(pool.clone()), "/users/alice/outbox", None).await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);

    let (status, _, body) =
        get_json(test_app(pool.clone()), "/users/ghost/outbox", Some(AP_JSON)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");

    // In authorized-fetch mode the outbox needs a signature like every
    // other AP object GET.
    let (status, _, _) = get_json(
        test_app_secure(pool.clone(), Arc::default()),
        "/users/alice/outbox",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Ingesting a remote actor stores its advertised `outbox` URL, and syncing
/// the collection's `totalItems` makes the origin's authoritative total the
/// account's `statuses_count` — Mastodon's
/// `ProcessAccountService#set_fetchable_attributes!` semantics, including
/// keeping the stored baseline when a later fetch reports no numeric total.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_outbox_total_items_becomes_statuses_count(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let outbox_uri = format!("{}/outbox", bob.actor.id);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        outbox_uri.clone(),
        serde_json::json!({
            "id": outbox_uri,
            "type": "OrderedCollection",
            "totalItems": 4242,
            "first": format!("{outbox_uri}?page=true"),
        }),
    );
    let state = test_state_with(pool.clone(), stub.clone());

    let stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let urls = plamenu_db::account::collection_urls_of(&pool, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(urls.outbox_url, outbox_uri);

    // Before any sync the count is the locally-known slice (nothing).
    let counts = entities::account_counts_batch(&pool, &[stored.id])
        .await
        .unwrap();
    assert_eq!(counts[&stored.id].statuses, 0);

    remote::sync_outbox_stats(&state, stored.id, &outbox_uri)
        .await
        .unwrap();
    let counts = entities::account_counts_batch(&pool, &[stored.id])
        .await
        .unwrap();
    assert_eq!(counts[&stored.id].statuses, 4242);

    // A collection without a numeric `totalItems` keeps the baseline, like
    // Mastodon's `collection_info`.
    stub.objects.lock().unwrap().insert(
        outbox_uri.clone(),
        serde_json::json!({
            "id": outbox_uri,
            "type": "OrderedCollection",
            "totalItems": "not-a-number",
        }),
    );
    remote::sync_outbox_stats(&state, stored.id, &outbox_uri)
        .await
        .unwrap();
    let counts = entities::account_counts_batch(&pool, &[stored.id])
        .await
        .unwrap();
    assert_eq!(counts[&stored.id].statuses, 4242);
}

/// A non-https `outbox` value is dropped at ingest like the other
/// collection URLs, and never overrides local counting.
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_outbox_rejects_invalid_uris(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let mut actor = bob.actor.clone();
    actor.outbox = Some(Value::String("bear:?u=https://remote.example/x".into()));

    let stored = remote::store_remote_actor(&pool, &actor).await.unwrap();
    let urls = plamenu_db::account::collection_urls_of(&pool, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(urls.outbox_url, "");
}
