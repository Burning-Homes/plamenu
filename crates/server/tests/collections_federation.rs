//! FEP-7aa9 account collections over `ActivityPub`: the consent handshake in
//! both directions — a remote owner featuring our local account
//! (`FeatureRequest` → `Accept`/`Reject` + the `FeatureAuthorization` stamp we
//! serve), our local collection featuring a remote account
//! (`Accept`/`Reject` of the request we sent, then `Delete` to revoke), and
//! ingesting a broadcast `FeaturedCollection`.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, account, collection, notification, oauth, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A local user with a read/write access token (via the OAuth tables directly).
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
            name: "collections-federation-tests",
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

const ALICE_URI: &str = "https://plamenu.test/users/alice";

async fn post_signed(app: Router, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let sig = signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", sig.host)
        .header("date", sig.date)
        .header("digest", sig.digest)
        .header("signature", sig.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

async fn get_ap(app: Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn make_discoverable(pool: &PgPool, account_id: i64) {
    sqlx::query!(
        "UPDATE accounts SET discoverable = true WHERE id = $1",
        account_id
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A remote owner's `FeaturedCollection` document, served at `uri`.
fn remote_collection(owner: &RemoteUser, uri: &str, items: &[Value]) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": uri,
        "type": "FeaturedCollection",
        "attributedTo": owner.actor.id,
        "url": uri,
        "name": "Favourites",
        "summary": "people I like",
        "sensitive": false,
        "discoverable": true,
        "published": "2026-06-01T00:00:00Z",
        "updated": "2026-06-01T00:00:00Z",
        "totalItems": items.len(),
        "orderedItems": items,
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_feature_request_is_accepted_and_a_stamp_is_served(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    make_discoverable(&pool, alice.id).await;
    let bob = RemoteUser::new("remote.example", "bob");
    let collection_uri = format!("{}/collections/9", bob.actor.id);

    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        collection_uri.clone(),
        remote_collection(&bob, &collection_uri, &[]),
    );
    let state = test_state_with(pool.clone(), stub.clone());

    let request = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/feature_requests/1", bob.actor.id),
        "type": "FeatureRequest",
        "object": ALICE_URI,
        "instrument": collection_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    // The remote collection was fetched and stored, with alice an accepted
    // member, and alice was notified.
    let stored = collection::find_by_uri(&pool, &collection_uri)
        .await
        .unwrap()
        .expect("remote collection ingested");
    assert!(!stored.local);
    let item = collection::find_item_by_account(&pool, stored.id, alice.id)
        .await
        .unwrap()
        .expect("alice is a member");
    assert_eq!(item.state, "accepted");
    let notifs = notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(notifs.len(), 1);
    assert_eq!(notifs[0].kind, "added_to_collection");

    // The Accept goes back to bob carrying our stamp as `result`.
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let accept = &sent.last().unwrap().activity;
    assert_eq!(accept["type"], "Accept");
    assert_eq!(accept["object"], request["id"]);
    let stamp_uri = accept["result"].as_str().unwrap();
    assert_eq!(
        stamp_uri,
        format!(
            "https://plamenu.test/users/alice/feature_authorizations/{}",
            item.id
        )
    );

    // And that stamp is served as a FeatureAuthorization.
    let (code, stamp) = get_ap(test_app(pool.clone()), stamp_uri).await;
    assert_eq!(code, StatusCode::OK, "{stamp}");
    assert_eq!(stamp["type"], "FeatureAuthorization");
    assert_eq!(stamp["interactionTarget"], ALICE_URI);
    assert_eq!(stamp["interactingObject"], collection_uri.as_str());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_feature_request_is_rejected_when_not_featureable(pool: PgPool) {
    // Fresh accounts are discoverable by default (migration 0127); opt alice
    // out so she is not featureable, which is what this test exercises.
    let alice = create_local_account(&pool, "alice", "Alice").await;
    sqlx::query!(
        "UPDATE accounts SET discoverable = false WHERE id = $1",
        alice.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let bob = RemoteUser::new("remote.example", "bob");
    let collection_uri = format!("{}/collections/9", bob.actor.id);

    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        collection_uri.clone(),
        remote_collection(&bob, &collection_uri, &[]),
    );
    let state = test_state_with(pool.clone(), stub.clone());

    let request = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/feature_requests/2", bob.actor.id),
        "type": "FeatureRequest",
        "object": ALICE_URI,
        "instrument": collection_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    let stored = collection::find_by_uri(&pool, &collection_uri)
        .await
        .unwrap()
        .unwrap();
    assert!(
        collection::find_item_by_account(&pool, stored.id, alice.id)
            .await
            .unwrap()
            .is_none(),
        "a non-featureable account is not added"
    );
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    assert_eq!(sent.last().unwrap().activity["type"], "Reject");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_feature_request_accept_then_revoke(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let bob_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let state = test_state_with(pool.clone(), stub.clone());

    // alice features remote bob: the membership is pending and a FeatureRequest
    // is enqueued.
    let (code, collection) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        json!({ "name": "Pals", "account_ids": [bob_account.id.to_string()] }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{collection}");
    let collection_id: i64 = collection["collection"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let item = collection::find_item_by_account(&pool, collection_id, bob_account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.state, "pending");
    let request_uri = item.activity_uri.clone().unwrap();
    let stamp_uri = format!("{}/stamps/1", bob.actor.id);

    // bob accepts, granting his stamp.
    let accept = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/accepts/1", bob.actor.id),
        "type": "Accept",
        "actor": bob.actor.id,
        "object": request_uri,
        "result": stamp_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &accept,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let item = collection::find_item_by_account(&pool, collection_id, bob_account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.state, "accepted");
    assert_eq!(item.approval_uri.as_deref(), Some(stamp_uri.as_str()));

    // The now-authorized membership is re-distributed as Add(FeaturedItem).
    delivery::run_due(&state).await;
    assert!(
        stub.deliveries()
            .iter()
            .any(|d| d.activity["type"] == "Add" && d.activity["object"]["type"] == "FeaturedItem"),
        "an Add(FeaturedItem) was distributed"
    );

    // bob revokes by deleting the stamp.
    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{stamp_uri}#delete"),
        "type": "Delete",
        "actor": bob.actor.id,
        "object": { "id": stamp_uri, "type": "FeatureAuthorization" },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &delete,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        collection::find_item_by_account(&pool, collection_id, bob_account.id)
            .await
            .unwrap()
            .is_none(),
        "the revoked membership is removed"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_feature_request_reject(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let bob_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();

    let (_code, collection) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        json!({ "name": "Pals", "account_ids": [bob_account.id.to_string()] }),
    )
    .await;
    let collection_id: i64 = collection["collection"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let item = collection::find_item_by_account(&pool, collection_id, bob_account.id)
        .await
        .unwrap()
        .unwrap();

    let reject = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/rejects/1", bob.actor.id),
        "type": "Reject",
        "actor": bob.actor.id,
        "object": item.activity_uri.clone().unwrap(),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &reject,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let item = collection::find_item_by_account(&pool, collection_id, bob_account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.state, "rejected");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_add_featured_collection_ingests_a_remote_member(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let collection_uri = format!("{}/collections/3", bob.actor.id);
    let item_uri = format!("{collection_uri}/items/1");
    let approval_uri = format!("{}/stamps/1", carol.actor.id);
    let item = json!({
        "id": item_uri,
        "type": "FeaturedItem",
        "featuredObject": carol.actor.id,
        "featureAuthorization": approval_uri,
        "published": "2026-06-01T00:00:00Z",
    });
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);

    let add = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/adds/1", bob.actor.id),
        "type": "Add",
        "actor": bob.actor.id,
        "target": format!("{}/featured_collections", bob.actor.id),
        "object": remote_collection(&bob, &collection_uri, &[item]),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &add,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    let stored = collection::find_by_uri(&pool, &collection_uri)
        .await
        .unwrap()
        .expect("remote collection stored");
    assert!(!stored.local);
    let carol_account = account::find_by_uri(&pool, &carol.actor.id)
        .await
        .unwrap()
        .unwrap();
    let membership = collection::find_item_by_account(&pool, stored.id, carol_account.id)
        .await
        .unwrap()
        .expect("carol is a member");
    assert_eq!(membership.state, "accepted");
    assert_eq!(
        membership.approval_uri.as_deref(),
        Some(approval_uri.as_str())
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn serves_featured_collections_and_a_collection_document(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    make_discoverable(&pool, bob.id).await;

    let (code, collection) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/collections",
        Some(&token),
        json!({
            "name": "Mutuals",
            "description": "my pals",
            "discoverable": true,
            "account_ids": [bob.id.to_string()],
        }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{collection}");
    let collection_id = collection["collection"]["id"].as_str().unwrap();

    // The actor advertises its featuredCollections endpoint.
    let (code, actor) = get_ap(test_app(pool.clone()), ALICE_URI).await;
    assert_eq!(code, StatusCode::OK);
    let endpoint = actor["featuredCollections"].as_str().unwrap();
    assert_eq!(endpoint, format!("{ALICE_URI}/featured_collections"));

    // The bare endpoint is a Collection envelope pointing at the first page.
    let (code, envelope) = get_ap(test_app(pool.clone()), endpoint).await;
    assert_eq!(code, StatusCode::OK, "{envelope}");
    assert_eq!(envelope["type"], "Collection");
    assert_eq!(envelope["totalItems"], 1);
    let first = envelope["first"].as_str().unwrap();

    // The first page inlines the FeaturedCollection object.
    let (code, page) = get_ap(test_app(pool.clone()), first).await;
    assert_eq!(code, StatusCode::OK, "{page}");
    assert_eq!(page["type"], "CollectionPage");
    let item = &page["items"][0];
    assert_eq!(item["type"], "FeaturedCollection");
    assert_eq!(item["attributedTo"], ALICE_URI);
    assert_eq!(item["orderedItems"][0]["type"], "FeaturedItem");
    let collection_uri = item["id"].as_str().unwrap().to_owned();
    assert_eq!(
        collection_uri,
        format!("{ALICE_URI}/collections/{collection_id}")
    );

    // And the collection document is served at its own URL with @context.
    let (code, doc) = get_ap(test_app(pool.clone()), &collection_uri).await;
    assert_eq!(code, StatusCode::OK, "{doc}");
    assert_eq!(doc["type"], "FeaturedCollection");
    assert_eq!(doc["name"], "Mutuals");
    assert!(doc["@context"].is_array());
    // bob's FeaturedItem carries the stamp we serve for him.
    assert_eq!(
        doc["orderedItems"][0]["featuredObject"],
        "https://plamenu.test/users/bob"
    );
    assert!(
        doc["orderedItems"][0]["featureAuthorization"]
            .as_str()
            .unwrap()
            .contains("/users/bob/feature_authorizations/")
    );
    let _ = alice;
}

/// JSON API helper (bearer-authenticated).
async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
