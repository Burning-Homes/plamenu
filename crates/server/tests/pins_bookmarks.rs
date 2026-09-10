//! Bookmarks and status pins: client API, the featured AP collection, and
//! Add/Remove federation in both directions.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, pin, status, user};
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
            "client_name": "pins-bookmarks",
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

/// Posts a status through the API and returns its entity.
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

/// Signs `body` as a POST to `path` and sends it through the router.
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

async fn ap_get(app: Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn ids_of(body: &Value) -> Vec<&str> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bookmark_lifecycle(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let posted = post_status(&pool, &token, json!({"status": "keep this"})).await;
    let post_id = posted["id"].as_str().unwrap().to_owned();
    assert_eq!(posted["bookmarked"], false);

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/bookmark"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bookmarked"], true);
    assert_eq!(body["id"], post_id.as_str());

    // Re-bookmarking is idempotent; the flag shows up on regular reads too.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/bookmark"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bookmarked"], true);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{post_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["bookmarked"], true);

    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/bookmarks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids_of(&body), [post_id.as_str()]);

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/unbookmark"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bookmarked"], false);
    // Unbookmarking again is an idempotent no-op.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/unbookmark"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/bookmarks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bookmarks_require_visibility_and_auth(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let secret = post_status(
        &pool,
        &alice_token,
        json!({"status": "followers only", "visibility": "private"}),
    )
    .await;
    let secret_id = secret["id"].as_str().unwrap();

    // Carol cannot see alice's private post, so she cannot bookmark it.
    for action in ["bookmark", "unbookmark"] {
        let (status, body) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/statuses/{secret_id}/{action}"),
            Some(&carol_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}");
        assert_eq!(body["error"], "Record not found");
    }

    // The listing needs a token.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/bookmarks",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bookmarks_listing_paginates_by_bookmark_id(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let mut post_ids = Vec::new();
    for n in 0..3 {
        let posted = post_status(&pool, &token, json!({"status": format!("post {n}")})).await;
        post_ids.push(posted["id"].as_str().unwrap().to_owned());
    }
    // Bookmark in posting order: the listing is newest bookmark first.
    for post_id in &post_ids {
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/statuses/{post_id}/bookmark"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, headers, body) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v1/bookmarks?limit=2",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids_of(&body), [post_ids[2].as_str(), post_ids[1].as_str()]);
    let link = headers[header::LINK].to_str().unwrap().to_owned();
    assert!(link.contains("rel=\"next\""), "{link}");
    assert!(link.contains("rel=\"prev\""), "{link}");

    // Follow the next link: the remaining (oldest) bookmark.
    let next = link
        .split(">; rel=\"next\"")
        .next()
        .unwrap()
        .rsplit('<')
        .next()
        .unwrap()
        .strip_prefix(&format!("https://{TEST_DOMAIN}"))
        .unwrap()
        .to_owned();
    let (_, body) = api(test_app(pool.clone()), "GET", &next, Some(&token), None).await;
    assert_eq!(ids_of(&body), [post_ids[0].as_str()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pin_lifecycle_and_listings(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let first = post_status(&pool, &token, json!({"status": "first"})).await;
    let second = post_status(&pool, &token, json!({"status": "second"})).await;
    let first_id = first["id"].as_str().unwrap().to_owned();
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_eq!(first["pinned"], false, "own statuses carry the pinned flag");

    for post_id in [&first_id, &second_id] {
        let (status, body) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/statuses/{post_id}/pin"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pinned"], true);
    }

    // Anonymous viewers get no `pinned` field at all, like Mastodon.
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{first_id}"),
        None,
        None,
    )
    .await;
    assert!(body.get("pinned").is_none());

    // pinned=true lists most recently pinned first, for anyone.
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses?pinned=true", alice.id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids_of(&body), [second_id.as_str(), first_id.as_str()]);

    // The featured AP collection inlines distributable pins as full Notes.
    let (status, featured) =
        ap_get(test_app(pool.clone()), "/users/alice/collections/featured").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(featured["type"], "OrderedCollection");
    assert_eq!(featured["totalItems"], 2);
    let items = featured["orderedItems"].as_array().unwrap();
    assert_eq!(items[0]["type"], "Note");
    assert_eq!(
        items[0]["id"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{second_id}")
    );
    assert_eq!(items[1]["content"], "<p>first</p>");

    // Unpin removes it everywhere.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{second_id}/unpin"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pinned"], false);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses?pinned=true", alice.id),
        None,
        None,
    )
    .await;
    assert_eq!(ids_of(&body), [first_id.as_str()]);
    // Unpinning a never-pinned status is an idempotent no-op.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{second_id}/unpin"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pin_validates_like_mastodon(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let post = post_status(&pool, &alice_token, json!({"status": "alice's"})).await;
    let post_id = post["id"].as_str().unwrap().to_owned();

    // Someone else's post.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/pin"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Someone else's post cannot be pinned"
    );

    // A boost.
    let (_, boost) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/reblog"),
        Some(&carol_token),
        None,
    )
    .await;
    let boost_id = boost["id"].as_str().unwrap();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{boost_id}/pin"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Validation failed: A boost cannot be pinned");

    // A direct message.
    let dm = post_status(
        &pool,
        &alice_token,
        json!({"status": "note to self", "visibility": "direct"}),
    )
    .await;
    let dm_id = dm["id"].as_str().unwrap();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{dm_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Posts that are only visible to mentioned users cannot be pinned"
    );

    // Re-pinning: Mastodon's RecordNotUnique rescue.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Duplicate record");

    // The 5-pin limit.
    for n in 0..4 {
        let extra = post_status(&pool, &alice_token, json!({"status": format!("p{n}")})).await;
        let extra_id = extra["id"].as_str().unwrap();
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            &format!("/api/v1/statuses/{extra_id}/pin"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let sixth = post_status(&pool, &alice_token, json!({"status": "one too many"})).await;
    let sixth_id = sixth["id"].as_str().unwrap();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{sixth_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: You have already pinned the maximum number of posts"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_pins_stay_visibility_gated(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let secret = post_status(
        &pool,
        &alice_token,
        json!({"status": "followers only", "visibility": "private"}),
    )
    .await;
    let secret_id = secret["id"].as_str().unwrap().to_owned();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{secret_id}/pin"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Anonymous and non-follower viewers see no pinned statuses.
    for token in [None, Some(carol_token.as_str())] {
        let (_, body) = api(
            test_app(pool.clone()),
            "GET",
            &format!("/api/v1/accounts/{}/statuses?pinned=true", alice.id),
            token,
            None,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    // A follower sees the private pin.
    follow::create(&pool, carol.id, alice.id, None)
        .await
        .unwrap();
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses?pinned=true", alice.id),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(ids_of(&body), [secret_id.as_str()]);

    // The featured collection references it as a bare IRI, never inline.
    let (_, featured) = ap_get(test_app(pool.clone()), "/users/alice/collections/featured").await;
    assert_eq!(featured["totalItems"], 1);
    assert_eq!(
        featured["orderedItems"][0],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{secret_id}")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pin_federates_add_and_remove_to_followers(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, remote_bob.id, alice.id, None)
        .await
        .unwrap();

    let post = post_status(&pool, &token, json!({"status": "pin me"})).await;
    let post_id = post["id"].as_str().unwrap();
    let state = test_state_with(pool.clone(), stub.clone());
    delivery::run_due(&state).await; // flush the Create

    let (status, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/pin"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery::run_due(&state).await, 1);
    let add = &stub.deliveries().last().unwrap().activity.clone();
    assert_eq!(add["type"], "Add");
    assert_eq!(
        add["object"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{post_id}")
    );
    assert_eq!(
        add["target"],
        format!("https://{TEST_DOMAIN}/users/alice/collections/featured")
    );
    assert!(add.get("id").is_none(), "featured Adds carry no id");

    let (status, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/unpin"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery::run_due(&state).await, 1);
    let remove = &stub.deliveries().last().unwrap().activity.clone();
    assert_eq!(remove["type"], "Remove");
    assert_eq!(
        remove["target"],
        format!("https://{TEST_DOMAIN}/users/alice/collections/featured")
    );
}

fn featured_change(bob: &RemoteUser, kind: &str, object: &Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": kind,
        "actor": bob.actor.id,
        "object": object,
        "target": format!("{}/collections/featured", bob.actor.id),
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_add_and_remove_track_remote_pins(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let note = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: &note_uri,
            account_id: remote_bob.id,
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

    let add = featured_change(&bob, "Add", &json!(note_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &add,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        pin::pinned_of(&pool, remote_bob.id, &[note.id])
            .await
            .unwrap(),
        [note.id]
    );

    // The pin shows in the account's pinned listing.
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/statuses?pinned=true", remote_bob.id),
        None,
        None,
    )
    .await;
    assert_eq!(ids_of(&body), [note.id.to_string().as_str()]);

    // An Add whose target is not the featured collection is ignored.
    let foreign_target = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Add",
        "actor": bob.actor.id,
        "object": note_uri,
        "target": format!("{}/collections/somewhere-else", bob.actor.id),
    });
    pin::delete(&pool, remote_bob.id, note.id).await.unwrap();
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &foreign_target,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        pin::pinned_of(&pool, remote_bob.id, &[note.id])
            .await
            .unwrap()
            .is_empty()
    );

    // Remove unpins.
    pin::create(&pool, remote_bob.id, note.id).await.unwrap();
    let remove = featured_change(&bob, "Remove", &json!(note_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &remove,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        pin::pinned_of(&pool, remote_bob.id, &[note.id])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_add_remove_track_remote_featured_tags(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let featured_tags_uri = format!("/api/v1/accounts/{}/featured_tags", remote_bob.id);

    // An Add of a Hashtag (cased) targeting bob's featured collection records a
    // remote featured tag, normalized lowercase.
    let hashtag = json!({
        "type": "Hashtag",
        "href": "https://remote.example/tags/rust",
        "name": "#Rust",
    });
    let add = featured_change(&bob, "Add", &hashtag);
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &add,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &featured_tags_uri,
        None,
        None,
    )
    .await;
    assert_eq!(body[0]["name"], "rust");

    // Remove unfeatures it.
    let remove = featured_change(&bob, "Remove", &hashtag);
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &remove,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &featured_tags_uri,
        None,
        None,
    )
    .await;
    assert!(body.as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_add_fetches_unknown_status_and_refuses_foreign_ones(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([bob.actor.clone(), carl.actor.clone()]);
    let unknown_uri = format!("{}/statuses/77", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        unknown_uri.clone(),
        json!({
            "id": unknown_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>fetched on demand</p>",
            "published": "2026-06-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
        }),
    );

    let add = featured_change(&bob, "Add", &json!(unknown_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &add,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let fetched = status::find_by_uri(&pool, &unknown_uri)
        .await
        .unwrap()
        .expect("the pinned status was fetched and ingested");
    let remote_bob = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.account_id, remote_bob.id);
    assert_eq!(
        pin::pinned_of(&pool, remote_bob.id, &[fetched.id])
            .await
            .unwrap(),
        [fetched.id]
    );

    // Carl cannot pin bob's status onto his own profile.
    let foreign = featured_change(&carl, "Add", &json!(unknown_uri));
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &foreign,
        &carl.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let remote_carl = plamenu_db::account::find_by_uri(&pool, &carl.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        pin::pinned_of(&pool, remote_carl.id, &[fetched.id])
            .await
            .unwrap()
            .is_empty()
    );
}
