//! Server-level coverage for soft-deleted thread stubs (the deferred
//! follow-ups of the 2026-07-23 thread-order/stub work). The DB-level
//! faithfulness tests live in `crates/db/src/status.rs`; these prove the
//! *served* surfaces: the AP object URL answers `410`, the context API serves
//! the placeholder entity, an inbound remote `Delete` with a local reply
//! leaves a stub, the web thread renders the tombstone card, and the Note
//! builder's defence-in-depth branch never serves a stub's remains.

mod common;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, oauth, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

const PLACEHOLDER: &str = "<p><em>ℹ️ deleted status ℹ️</em></p>";

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
            name: "thread-stubs",
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
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (code, value)
}

/// Root ← middle ← leaf, all by `alice`; the middle is deleted through the
/// ordinary API delete, which stubs because a reply hangs off it.
async fn stubbed_middle(pool: &PgPool, token: &str) -> (String, String, String) {
    let app = || test_app(pool.clone());
    let (_, root) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": "the root"})),
    )
    .await;
    let root_id = root["id"].as_str().unwrap().to_owned();
    let (_, mid) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": "the doomed middle", "in_reply_to_id": root_id})),
    )
    .await;
    let mid_id = mid["id"].as_str().unwrap().to_owned();
    let (_, leaf) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": "the leaf", "in_reply_to_id": mid_id})),
    )
    .await;
    let leaf_id = leaf["id"].as_str().unwrap().to_owned();

    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{mid_id}"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    (root_id, mid_id, leaf_id)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_ap_object_of_a_stub_answers_410(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (_, mid_id, _) = stubbed_middle(&pool, &token).await;

    let request = Request::builder()
        .uri(format!("/users/alice/statuses/{mid_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::GONE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["type"], "Tombstone", "{doc}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn context_serves_the_placeholder_for_a_deleted_middle(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (root_id, mid_id, leaf_id) = stubbed_middle(&pool, &token).await;

    let (code, context) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let ancestors = context["ancestors"].as_array().unwrap();
    assert_eq!(ancestors.len(), 2, "root + placeholder middle: {context}");
    assert_eq!(ancestors[0]["id"], root_id.as_str());
    assert_eq!(ancestors[1]["id"], mid_id.as_str());
    assert_eq!(ancestors[1]["deleted"], true);
    assert_eq!(ancestors[1]["content"], PLACEHOLDER);
    assert!(
        !context.to_string().contains("doomed"),
        "the deleted words must be gone from the thread: {context}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_inbound_delete_with_a_local_reply_leaves_a_stub(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let note_uri = format!("{}/statuses/1", bob.actor.id);

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>remote words</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let remote = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();

    // A local reply hangs off it before the Delete arrives.
    let (_reply, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "a local reply",
            visibility: "public",
            in_reply_to_id: Some(remote.id),
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let delete = json!({
        "type": "Delete",
        "actor": bob.actor.id,
        "object": { "id": note_uri, "type": "Tombstone" },
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

    // The row survives as a stub — content stripped, threading kept — instead
    // of hard-deleting and orphaning the local reply.
    let stubbed = status::find_by_uri(&pool, &note_uri).await.unwrap();
    let stubbed = stubbed.expect("a replied-to post must stub, not vanish");
    assert!(
        status::is_deleted(&pool, stubbed.id).await.unwrap(),
        "the surviving row must be marked deleted"
    );
    assert!(
        stubbed.content.is_empty(),
        "stub content must be stripped: {:?}",
        stubbed.content
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_web_thread_renders_a_tombstone_card(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (_, _, leaf_id) = stubbed_middle(&pool, &token).await;

    let request = Request::builder()
        .uri(format!("/@alice/{leaf_id}"))
        .body(Body::empty())
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        body.contains("status--tombstone"),
        "the stub must render the dedicated tombstone card"
    );
    assert!(
        !body.contains("doomed"),
        "the deleted words must not reach the page"
    );
    assert!(
        !body.contains(PLACEHOLDER),
        "the tombstone card replaces the API placeholder text, not repeats it"
    );
}

/// The Note builder's defence-in-depth branch: even when a caller hands a stub
/// straight to the builder (as a collection missing its STUBFILTER would), the
/// wire object is a placeholder — no author words, no attachments, threading
/// kept.
#[sqlx::test(migrations = "../db/migrations")]
async fn the_note_builder_serves_a_placeholder_for_a_stub(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let (root_id, mid_id, _) = stubbed_middle(&pool, &token).await;
    let state = test_state_with(pool.clone(), Arc::<StubFederation>::default());

    let stub_row = status::find_by_id(&pool, mid_id.parse().unwrap())
        .await
        .unwrap()
        .expect("the stub row is reachable by single fetch");
    let note = plamenu::note::note_for_status(&state, &stub_row, &alice)
        .await
        .unwrap();
    assert_eq!(note["type"], "Note");
    assert_eq!(note["content"], PLACEHOLDER, "{note}");
    assert_eq!(
        note["inReplyTo"],
        format!("https://{TEST_DOMAIN}/users/alice/statuses/{root_id}"),
        "threading is the stub's whole purpose: {note}"
    );
    assert!(
        note["attachment"].as_array().is_none_or(Vec::is_empty),
        "{note}"
    );
    assert!(!note.to_string().contains("doomed"), "{note}");
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
