//! Hashtag following: `/api/v1/tags/{id}` (show/follow/unfollow),
//! `/api/v1/followed_tags`, the `following` flag on search results, and the
//! followed-tag home-timeline injection.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
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
            name: "tags-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write follow",
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
        "read write follow",
    )
    .await
    .unwrap();
    (account, token)
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

async fn post_status(pool: &PgPool, token: &str, body: Value) -> Value {
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    entity
}

fn ids_of(page: &Value) -> Vec<String> {
    page.as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn featured_tags_rest_lifecycle(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // Feature a tag by name; the FeaturedTag entity comes back with zeroed
    // stats (no statuses yet).
    let (status, ft) = api(
        app(),
        "POST",
        "/api/v1/featured_tags",
        Some(&token),
        Some(json!({ "name": "#Rust" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ft}");
    assert_eq!(ft["name"], "rust");
    assert_eq!(ft["statuses_count"], "0");
    assert_eq!(ft["last_status_at"], Value::Null);
    assert!(ft["url"].as_str().unwrap().ends_with("/tagged/rust"));
    let featured_id = ft["id"].as_str().unwrap().to_owned();

    // A blank/invalid name is a 422.
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/featured_tags",
        Some(&token),
        Some(json!({ "name": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Posting a public status with the tag bumps the stats.
    post_status(&pool, &token, json!({ "status": "learning #rust today" })).await;
    let (status, list) = api(app(), "GET", "/api/v1/featured_tags", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["name"], "rust");
    assert_eq!(list[0]["statuses_count"], "1");
    assert_ne!(list[0]["last_status_at"], Value::Null);

    // It also rides the Profile entity.
    let (_, profile) = api(app(), "GET", "/api/v1/profile", Some(&token), None).await;
    assert_eq!(profile["featured_tags"][0]["name"], "rust");

    // Suggestions surface tags the account used but did not feature.
    post_status(&pool, &token, json!({ "status": "some #art too" })).await;
    let (_, suggestions) = api(
        app(),
        "GET",
        "/api/v1/featured_tags/suggestions",
        Some(&token),
        None,
    )
    .await;
    let names: Vec<&str> = suggestions
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["art"], "rust is already featured");

    // The public per-account endpoint mirrors it (no auth needed).
    let (status, public) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}/featured_tags", alice.id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public.as_array().unwrap().len(), 1);

    // Deleting by id empties the list and returns `{}`.
    let (status, body) = api(
        app(),
        "DELETE",
        &format!("/api/v1/featured_tags/{featured_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({}));
    let (_, list) = api(app(), "GET", "/api/v1/featured_tags", Some(&token), None).await;
    assert!(list.as_array().unwrap().is_empty());

    // Deleting an unknown featured tag is a 404.
    let (status, _) = api(
        app(),
        "DELETE",
        "/api/v1/featured_tags/999999",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn featuring_a_tag_federates_add_and_remove(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    // Bob follows alice, so featured-tag changes reach his inbox.
    follow::create(&pool, stored_bob.id, alice.id, None)
        .await
        .unwrap();

    // The `/tags/{id}/feature` alias returns the Tag entity and federates Add.
    let (status, tag) = api(
        app(),
        "POST",
        "/api/v1/tags/rust/feature",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tag}");
    assert_eq!(tag["name"], "rust");

    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].activity["type"], "Add");
    assert_eq!(sent[0].activity["object"]["type"], "Hashtag");
    assert_eq!(sent[0].activity["object"]["name"], "#rust");
    assert!(
        sent[0].activity["target"]
            .as_str()
            .unwrap()
            .ends_with("/collections/featured")
    );

    // Unfeaturing federates Remove(Hashtag).
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/tags/rust/unfeature",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].activity["type"], "Remove");
    assert_eq!(sent[1].activity["object"]["type"], "Hashtag");

    // A repeat unfeature is a no-op (no further delivery).
    let (status, _) = api(
        app(),
        "POST",
        "/api/v1/tags/rust/unfeature",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery::run_due(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn tag_follow_lifecycle(pool: PgPool) {
    common::open_previews(&pool).await;
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // `show` is public; an anonymous viewer never gets a `following` field.
    let (status, anon) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/tags/rust",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{anon}");
    assert_eq!(anon["name"], "rust");
    assert_eq!(anon["url"], "https://plamenu.test/tags/rust");
    assert!(
        anon.get("following").is_none(),
        "anonymous: no following flag"
    );

    // Authenticated, before following.
    let (status, before) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/tags/rust",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["following"], false);

    // An invalid hashtag (no letters) is 404, like Mastodon's name gate.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/tags/123",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Following requires a token.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/rust/follow",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Follow → entity reports following:true.
    let (status, followed) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/rust/follow",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{followed}");
    assert_eq!(followed["name"], "rust");
    assert_eq!(followed["following"], true);

    // Idempotent.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/RUST/follow",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Listed in followed_tags, and the show endpoint now reflects it.
    let (status, listing) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/followed_tags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let names: Vec<&str> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["rust"]);
    assert_eq!(listing[0]["following"], true);

    // Search surfaces the follow state for an authenticated viewer.
    let (status, search) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=rust&type=hashtags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{search}");
    assert_eq!(search["hashtags"][0]["name"], "rust");
    assert_eq!(search["hashtags"][0]["following"], true);

    // Unfollow → entity reports following:false, listing empties.
    let (status, unfollowed) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/rust/unfollow",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unfollowed}");
    assert_eq!(unfollowed["following"], false);

    let (_status, listing) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/followed_tags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(listing.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn tag_entity_reports_usage_history(pool: PgPool) {
    common::open_previews(&pool).await;
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    // An unknown tag: empty id, an all-zero seven-day history, and (anonymous)
    // no viewer flags.
    let (status, unknown) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/tags/rust",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unknown}");
    assert_eq!(unknown["id"], "", "an unstored tag has an empty id");
    let hist = unknown["history"].as_array().unwrap();
    assert_eq!(hist.len(), 7, "history is always seven days");
    assert_eq!(hist[0]["uses"], "0");
    assert_eq!(hist[0]["accounts"], "0");
    assert!(unknown.get("following").is_none(), "anonymous: no flags");
    assert!(unknown.get("featuring").is_none());

    // Two accounts post public #rust today; a private post does not count.
    post_status(&pool, &alice_token, json!({ "status": "learning #rust" })).await;
    post_status(&pool, &alice_token, json!({ "status": "more #rust" })).await;
    post_status(&pool, &bob_token, json!({ "status": "bob's #rust" })).await;
    post_status(
        &pool,
        &alice_token,
        json!({ "status": "secret #rust", "visibility": "private" }),
    )
    .await;

    let (status, tag) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/tags/rust",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tag}");
    assert_ne!(tag["id"], "", "a stored tag carries its id");
    assert_eq!(tag["name"], "rust");
    let hist = tag["history"].as_array().unwrap();
    assert_eq!(hist.len(), 7);
    assert_eq!(hist[0]["uses"], "3", "three public uses today");
    assert_eq!(hist[0]["accounts"], "2", "two distinct accounts");
    // `day` is the UTC-midnight Unix timestamp, serialized as a string.
    assert!(hist[0]["day"].as_str().unwrap().parse::<i64>().is_ok());
    assert_eq!(tag["following"], false);
    assert_eq!(tag["featuring"], false);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn followed_tags_paginate_by_follow_id(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    for name in ["rust", "go"] {
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/tags/{name}/follow"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // limit=1 yields the newest follow (go) plus a `next` Link header.
    let (status, headers, page) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v1/followed_tags?limit=1",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page[0]["name"], "go");
    let link = headers
        .get(header::LINK)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(link.contains("rel=\"next\""), "expected next link: {link}");
    assert!(
        link.contains("max_id="),
        "next link keys on follow id: {link}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn followed_tag_injects_posts_into_home(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    // Alice follows #rust but does not follow bob.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/rust/follow",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bob (a stranger) posts one tagged and one untagged public status.
    let tagged = post_status(
        &pool,
        &bob_token,
        json!({"status": "shipping some #rust today"}),
    )
    .await;
    let untagged = post_status(&pool, &bob_token, json!({"status": "unrelated thoughts"})).await;
    let tagged_id = tagged["id"].as_str().unwrap().to_owned();
    let untagged_id = untagged["id"].as_str().unwrap().to_owned();

    // Alice's home carries the tagged post (hashtag injection) but not the
    // untagged one.
    let (status, home) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{home}");
    let home_ids = ids_of(&home);
    assert!(
        home_ids.contains(&tagged_id),
        "tagged post injected: {home_ids:?}"
    );
    assert!(
        !home_ids.contains(&untagged_id),
        "untagged post stays out: {home_ids:?}"
    );

    // After unfollowing, the injection stops.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/tags/rust/unfollow",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_status, home) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert!(ids_of(&home).is_empty(), "no more injection after unfollow");
}

/// The actor advertises `featuredTags` and lists each featured tag as a
/// `Hashtag` in its `tag` array; the collection endpoint serves the
/// Mastodon `Collection`-of-`Hashtag` shape.
#[sqlx::test(migrations = "../db/migrations")]
async fn featured_tags_ride_the_actor_and_its_collection(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/featured_tags",
        Some(&token),
        Some(json!({ "name": "#rustlang" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let get_ap = |uri: &str| {
        let request = Request::builder()
            .uri(uri)
            .header(header::ACCEPT, "application/activity+json")
            .body(Body::empty())
            .unwrap();
        let app = test_app(pool.clone());
        async move {
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };

    let actor = get_ap("/users/alice").await;
    assert_eq!(
        actor["featuredTags"],
        "https://plamenu.test/users/alice/collections/tags"
    );
    let hashtag = actor["tag"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "Hashtag")
        .expect("featured tag rides the actor tag array");
    assert_eq!(hashtag["name"], "#rustlang");
    assert_eq!(
        hashtag["href"],
        "https://plamenu.test/@alice/tagged/rustlang"
    );

    let collection = get_ap("/users/alice/collections/tags").await;
    assert_eq!(collection["type"], "Collection");
    assert_eq!(collection["totalItems"], 1);
    assert_eq!(collection["items"][0]["type"], "Hashtag");
    assert_eq!(collection["items"][0]["name"], "#rustlang");
}

/// Discovering a remote actor whose document advertises `featuredTags`
/// backfills the profile's featured tags from that collection, and a later
/// sync drops tags the origin stopped listing (Mastodon's
/// `SynchronizeFeaturedTagsCollectionWorker` semantics).
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_featured_tags_sync_from_the_collection(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let mut actor = bob.actor.clone();
    let tags_uri = format!("{}/collections/tags", actor.id);
    actor.featured_tags = Some(Value::String(tags_uri.clone()));
    let stub = StubFederation::with_actors([actor]);
    stub.objects.lock().unwrap().insert(
        tags_uri.clone(),
        json!({
            "id": tags_uri,
            "type": "Collection",
            "totalItems": 2,
            "items": [
                { "type": "Hashtag", "href": "https://remote.example/@bob/tagged/rust", "name": "#Rust" },
                { "type": "Hashtag", "href": "https://remote.example/@bob/tagged/art", "name": "#art" },
            ],
        }),
    );
    let state = test_state_with(pool.clone(), stub.clone());

    let stored = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    remote::sync_featured_tags(&state, stored.id, &tags_uri)
        .await
        .unwrap();
    let names: Vec<String> = plamenu_db::featured_tag::list(&pool, stored.id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"rust".to_owned()), "{names:?}");
    assert!(names.contains(&"art".to_owned()), "{names:?}");

    // The origin drops one tag; the next sync removes it locally too.
    stub.objects.lock().unwrap().insert(
        tags_uri.clone(),
        json!({
            "id": tags_uri,
            "type": "Collection",
            "totalItems": 1,
            "items": [
                { "type": "Hashtag", "href": "https://remote.example/@bob/tagged/rust", "name": "#Rust" },
            ],
        }),
    );
    remote::sync_featured_tags(&state, stored.id, &tags_uri)
        .await
        .unwrap();
    let names: Vec<String> = plamenu_db::featured_tag::list(&pool, stored.id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["rust"]);
}
