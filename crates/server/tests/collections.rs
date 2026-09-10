//! Followers/following listings: the client API endpoints and the
//! `ActivityPub` collections — through the real router.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
};
use http_body_util::BodyExt;
use plamenu::remote;
use plamenu_db::account::{self, RemoteAccountData};
use plamenu_db::{PgPool, follow};
use plamenu_federation::RequestSigner;
use serde_json::Value;
use tower::ServiceExt;

const ACCEPT_AP: &str = "application/activity+json";

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
    let signed_headers = request_signer.sign_get(TEST_DOMAIN, uri, ACCEPT_AP, SystemTime::now());
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", TEST_DOMAIN)
        .header("date", signed_headers.date)
        .header("signature", signed_headers.signature)
        .header(header::ACCEPT, ACCEPT_AP)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

fn accts(body: &Value) -> Vec<&str> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["acct"].as_str().unwrap())
        .collect()
}

/// A stored remote account without the cost of real key generation.
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

#[sqlx::test(migrations = "../db/migrations")]
async fn api_followers_and_following_list_accounts(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carl = remote::store_remote_actor(&pool, &RemoteUser::new("remote.example", "carl").actor)
        .await
        .unwrap();

    follow::create(&pool, bob.id, alice.id, None).await.unwrap();
    follow::create(&pool, carl.id, alice.id, None)
        .await
        .unwrap();
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    // A pending outgoing follow is "requested", not "following".
    follow::create_outgoing(&pool, alice.id, carl.id, "https://x/follow/1")
        .await
        .unwrap();

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/followers", alice.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts(&body), ["carl@remote.example", "bob"], "newest first");

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/following", alice.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts(&body), ["bob"], "pending follow excluded");

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/followers", bob.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts(&body), ["alice"]);

    let (status, _, body) =
        get_json(test_app(pool.clone()), "/api/v1/accounts/1/followers", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hidden_collections_hide_rest_lists_and_forbid_ap_pages(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, bob.id, alice.id, None).await.unwrap();
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        alice.id
    )
    .execute(&pool)
    .await
    .unwrap();

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/followers", alice.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    let (status, _, collection) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(collection["totalItems"], 1);

    let (status, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers?page=1",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(page["error"], "Collection is hidden");
}

/// A member who set `hide_collections` is filtered out of *other* accounts'
/// follower/following lists served by the client API — the list owner (carol)
/// hides nothing, but the hidden member (dave) must not leak to anonymous
/// callers. The self/owner exceptions are covered at the query layer.
#[sqlx::test(migrations = "../db/migrations")]
async fn hidden_member_filtered_from_other_accounts_lists_api(pool: PgPool) {
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let erin = create_local_account(&pool, "erin", "Erin").await;
    // dave (hidden) and erin (visible) follow carol; carol follows only dave.
    follow::create(&pool, dave.id, carol.id, None)
        .await
        .unwrap();
    follow::create(&pool, erin.id, carol.id, None)
        .await
        .unwrap();
    follow::create(&pool, carol.id, dave.id, None)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        dave.id
    )
    .execute(&pool)
    .await
    .unwrap();

    // carol's followers: dave is hidden, erin remains.
    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/followers", carol.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts(&body), ["erin"], "hidden follower filtered for anon");

    // carol's following: her only followee is the hidden dave → empty list.
    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/accounts/{}/following", carol.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array().unwrap().is_empty(),
        "hidden followee filtered for anon"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn api_follow_lists_paginate_by_follow_id(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let mut follow_ids = Vec::new();
    for name in ["b1", "b2", "b3"] {
        let follower = create_local_account(&pool, name, name).await;
        follow_ids.push(
            follow::create(&pool, follower.id, alice.id, None)
                .await
                .unwrap(),
        );
    }

    let base = format!("/api/v1/accounts/{}/followers", alice.id);
    let (status, headers, body) =
        get_json(test_app(pool.clone()), &format!("{base}?limit=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accts(&body), ["b3", "b2"]);
    let link = headers[header::LINK].to_str().unwrap();
    let next = format!(
        "<https://{TEST_DOMAIN}{base}?limit=2&max_id={}>; rel=\"next\"",
        follow_ids[1]
    );
    let prev = format!(
        "<https://{TEST_DOMAIN}{base}?limit=2&since_id={}>; rel=\"prev\"",
        follow_ids[2]
    );
    assert_eq!(link, format!("{next}, {prev}"));

    // The advertised next page holds the remainder and no further next link.
    let (_, headers, body) = get_json(
        test_app(pool.clone()),
        &format!("{base}?limit=2&max_id={}", follow_ids[1]),
        None,
    )
    .await;
    assert_eq!(accts(&body), ["b1"]);
    assert!(
        !headers[header::LINK]
            .to_str()
            .unwrap()
            .contains("rel=\"next\"")
    );

    // since_id pages toward newer follows.
    let (_, _, body) = get_json(
        test_app(pool.clone()),
        &format!("{base}?since_id={}", follow_ids[0]),
        None,
    )
    .await;
    assert_eq!(accts(&body), ["b3", "b2"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ap_followers_collection_envelope_and_pages(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, bob.id, alice.id, None).await.unwrap();
    for i in 1..=13 {
        let follower = cheap_remote(&pool, &format!("f{i}")).await;
        follow::create(&pool, follower.id, alice.id, None)
            .await
            .unwrap();
    }

    let collection = format!("https://{TEST_DOMAIN}/users/alice/followers");
    let (status, headers, body) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/activity+json; charset=utf-8"
    );
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["id"], collection.as_str());
    assert_eq!(body["totalItems"], 14);
    assert_eq!(body["first"], format!("{collection}?page=1"));
    assert!(!body.as_object().unwrap().contains_key("orderedItems"));

    let (_, _, page1) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers?page=1",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(page1["type"], "OrderedCollectionPage");
    assert_eq!(page1["id"], format!("{collection}?page=1"));
    assert_eq!(page1["partOf"], collection.as_str());
    assert_eq!(page1["totalItems"], 14);
    assert_eq!(page1["next"], format!("{collection}?page=2"));
    assert!(!page1.as_object().unwrap().contains_key("prev"));
    let items = page1["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 12);
    assert_eq!(items[0], "https://remote.example/users/f13", "newest first");

    let (_, _, page2) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers?page=2",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(page2["prev"], format!("{collection}?page=1"));
    assert!(!page2.as_object().unwrap().contains_key("next"));
    assert_eq!(
        page2["orderedItems"],
        serde_json::json!([
            "https://remote.example/users/f1",
            format!("https://{TEST_DOMAIN}/users/bob"),
        ]),
        "local followers use their derived actor URI"
    );

    // A non-numeric page clamps to 1, like Mastodon's `to_i`.
    let (_, _, garbled) = get_json(
        test_app(pool.clone()),
        "/users/alice/followers?page=abc",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(garbled["id"], format!("{collection}?page=1"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn followers_synchronization_is_signed_and_scoped_by_requester_origin(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let bob_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let carol_account = remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    let dave_account = remote::store_remote_actor(&pool, &dave.actor)
        .await
        .unwrap();
    for follower in [&bob_account, &carol_account, &dave_account] {
        follow::create(&pool, follower.id, alice.id, None)
            .await
            .unwrap();
    }

    let unsigned = get_json(
        test_app(pool.clone()),
        "/users/alice/followers_synchronization",
        Some(ACCEPT_AP),
    )
    .await;
    assert_eq!(unsigned.0, StatusCode::UNAUTHORIZED);

    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let (status, headers, body) = get_signed_json(
        test_app_with(pool.clone(), stub),
        "/users/alice/followers_synchronization",
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "max-age=0, private"
    );
    assert_eq!(
        body["id"],
        "https://plamenu.test/users/alice/followers_synchronization"
    );
    let items: Vec<&str> = body["orderedItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect();
    assert_eq!(
        items,
        [
            "https://remote.example/users/bob",
            "https://remote.example/users/carol"
        ]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ap_following_collection_and_content_negotiation(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = cheap_remote(&pool, "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    // Pending follows are not "following" yet.
    let carl = cheap_remote(&pool, "carl").await;
    follow::create_outgoing(&pool, alice.id, carl.id, "https://x/follow/1")
        .await
        .unwrap();

    let collection = format!("https://{TEST_DOMAIN}/users/alice/following");
    let (status, _, body) = get_json(
        test_app(pool.clone()),
        "/users/alice/following",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalItems"], 1);

    let (_, _, page) = get_json(
        test_app(pool.clone()),
        "/users/alice/following?page=1",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(
        page["orderedItems"],
        serde_json::json!(["https://remote.example/users/bob"])
    );
    assert_eq!(page["partOf"], collection.as_str());

    // Like the actor document, the collections are ActivityPub-only.
    let (status, _, _) = get_json(test_app(pool.clone()), "/users/alice/followers", None).await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
    let (status, _, _) = get_json(
        test_app(pool.clone()),
        "/users/alice/following",
        Some("text/html"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        "/users/ghost/followers",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}
