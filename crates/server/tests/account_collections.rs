//! Account collections (Mastodon 4.6 / FEP-7aa9): the local REST surface —
//! create/show/update/delete, membership, visibility, `in_collections`, and
//! the outbound consent flow for a remote member.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{StubFederation, create_local_account, test_app, test_app_with};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::remote;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, oauth, user};
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
            name: "collections-tests",
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
        "read write",
    )
    .await
    .unwrap();
    (account, token)
}

/// Opts a local account into discoverability (required to be featureable).
async fn make_discoverable(pool: &PgPool, account_id: i64) {
    sqlx::query!(
        "UPDATE accounts SET discoverable = true WHERE id = $1",
        account_id
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn api_on(
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

#[sqlx::test(migrations = "../db/migrations")]
async fn create_with_local_member_accepts_and_notifies(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;

    let (status, collection) = api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({
            "name": "Mutuals",
            "description": "my pals",
            "discoverable": true,
            "account_ids": [bob.id.to_string()],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    // Mastodon wraps create in a `collection` root key (`adapter: :json`).
    let collection = &collection["collection"];
    assert_eq!(collection["name"], "Mutuals");
    assert_eq!(collection["local"], true);
    assert_eq!(collection["account_id"], alice.id.to_string());
    assert_eq!(collection["item_count"], 1);
    let item = &collection["items"][0];
    assert_eq!(
        item["state"], "accepted",
        "a local member is accepted at once"
    );
    assert_eq!(item["account_id"], bob.id.to_string());

    // bob got an `added_to_collection` notification.
    let (status, notifs) = api_on(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&token), // alice's token won't see bob's; re-fetch with bob below
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _ = notifs;
    let kind = plamenu_db::notification::list(
        &pool,
        bob.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(kind.len(), 1);
    assert_eq!(kind[0].kind, "added_to_collection");
    assert_eq!(
        kind[0].collection_id,
        Some(collection["id"].as_str().unwrap().parse().unwrap())
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_accepts_rails_style_form_account_ids(pool: PgPool) {
    // Mastodon clients send `account_ids[]` form-encoded, not as a JSON array.
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;

    let form = format!("name=Pals&account_ids%5B%5D={}", bob.id);
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/collections")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let collection = &body["collection"];
    assert_eq!(collection["item_count"], 1, "{body}");
    assert_eq!(collection["items"][0]["account_id"], bob.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn index_hides_nondiscoverable_from_strangers(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({ "name": "Public", "discoverable": true })),
    )
    .await;
    api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({ "name": "Secret", "discoverable": false })),
    )
    .await;

    // The owner sees both.
    let (_s, owner_view) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/collections", alice.id),
        Some(&token),
        None,
    )
    .await;
    // Mastodon wraps the index in a `collections` root key.
    assert_eq!(owner_view["collections"].as_array().unwrap().len(), 2);

    // A stranger sees only the discoverable one.
    let (_s, stranger_view) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/collections", alice.id),
        Some(&bob_token),
        None,
    )
    .await;
    let names: Vec<&str> = stranger_view["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Public"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn in_collections_is_self_only(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (bob, bob_token) = user_with_token(&pool, "bob").await;
    make_discoverable(&pool, bob.id).await;

    // alice features bob.
    api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({ "name": "Pals", "account_ids": [bob.id.to_string()] })),
    )
    .await;

    // bob can read his own in_collections.
    let (status, mine) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/in_collections", bob.id),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["collections"].as_array().unwrap().len(), 1);

    // alice cannot read bob's — Mastodon's `index_featured_in_collections?`
    // denies with 403 (Pundit), not 404.
    let (status, _) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/in_collections", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_delete_and_item_management(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;

    let (_s, collection) = api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({ "name": "Pals", "discoverable": true })),
    )
    .await;
    let id = collection["collection"]["id"].as_str().unwrap().to_owned();

    // PATCH keeps unset attributes.
    let (status, updated) = api_on(
        test_app(pool.clone()),
        "PATCH",
        &format!("/api/v1/collections/{id}"),
        Some(&token),
        Some(json!({ "name": "Best Pals" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let updated = &updated["collection"];
    assert_eq!(updated["name"], "Best Pals");
    assert_eq!(updated["discoverable"], true, "discoverable preserved");

    // A non-owner cannot update.
    let (status, _) = api_on(
        test_app(pool.clone()),
        "PATCH",
        &format!("/api/v1/collections/{id}"),
        Some(&carol_token),
        Some(json!({ "name": "Hijacked" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Add an item, then remove it.
    let (status, item) = api_on(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/collections/{id}/items"),
        Some(&token),
        Some(json!({ "account_id": bob.id.to_string() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{item}");
    // Mastodon wraps the added item in a `collection_item` root key.
    let item = &item["collection_item"];
    assert_eq!(item["state"], "accepted");
    let item_id = item["id"].as_str().unwrap();

    let (status, _) = api_on(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/collections/{id}/items/{item_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete the collection.
    let (status, _) = api_on(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/collections/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/collections/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_member_is_pending_and_gets_a_feature_request(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = common::RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();

    let (status, collection) = api_on(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({
            "name": "Across the fediverse",
            "account_ids": [remote_account.id.to_string()],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    // A remote member stays pending until it accepts.
    assert_eq!(collection["collection"]["items"][0]["state"], "pending");

    // A FeatureRequest was enqueued to bob's inbox.
    let jobs = plamenu_db::job::pending_count(&pool).await.unwrap();
    assert!(jobs >= 1, "a feature request should be enqueued");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_member_without_feature_policy_is_rejected(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let mut bob = common::RemoteUser::new("remote.example", "bob");
    bob.actor.interaction_policy = None;
    let remote_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();

    let (status, body) = api_on(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        Some(json!({
            "name": "Across the fediverse",
            "account_ids": [remote_account.id.to_string()],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("Account can't be added to collections"),
        "{body}"
    );
}

/// Regression: the account entity's `feature_approval.current_user` must be
/// computed against the *authenticated viewer*, not a null viewer. A client
/// (e.g. Phanpy) reads this field to decide whether an account can be added to
/// a collection; when it was hardcoded to a null viewer every account came back
/// `"denied"` and the client greyed them all out. A discoverable, unlocked
/// account is featureable, so an authenticated viewer must see `"automatic"`.
#[sqlx::test(migrations = "../db/migrations")]
async fn account_feature_approval_current_user_reflects_viewer(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;

    // Authenticated: bob is featureable by alice, so `current_user` is automatic.
    let (status, body) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["feature_approval"]["current_user"], "automatic",
        "authenticated viewer must see an addable account as automatic: {body}"
    );

    // Anonymous: no viewer, so Mastodon reports `denied`.
    let (status, body) = api_on(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}", bob.id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feature_approval"]["current_user"], "denied", "{body}");
}
