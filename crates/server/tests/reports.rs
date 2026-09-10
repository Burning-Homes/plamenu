//! Moderation reports: the `POST /api/v1/reports` API, `Flag` federation out
//! (signed by the instance actor), and inbound `Flag` persistence.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{build_router, delivery, remote};
use plamenu_db::account::{self, Account};
use plamenu_db::{PgPool, report, role, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

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
            "client_name": "reports",
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

/// A read-only token (no `write` scope) for the scope-enforcement test. The
/// user must already exist.
async fn read_only_token(pool: &PgPool, email: &str) -> String {
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "reports-ro",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read",
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
            ("scope", "read"),
            ("email", email),
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
    token.1["access_token"].as_str().unwrap().to_owned()
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
async fn reporting_a_remote_account_federates_a_flag_from_the_instance_actor(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    // One of bob's posts, so the report can cite it.
    let bob_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/7",
            account_id: stored_bob.id,
            content: "<p>spam</p>",
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

    let (code, body) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({
            "account_id": stored_bob.id.to_string(),
            "comment": "please review this spammer",
            "category": "spam",
            "forward": true,
            "status_ids": [bob_status.id.to_string()],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body:?}");

    // The Report entity.
    assert_eq!(body["category"], "spam");
    assert_eq!(body["comment"], "please review this spammer");
    assert_eq!(body["forwarded"], true);
    assert_eq!(body["action_taken"], false);
    assert_eq!(body["action_taken_at"], Value::Null);
    assert_eq!(body["rule_ids"], Value::Null);
    assert_eq!(body["collection_ids"], json!([]));
    assert_eq!(body["status_ids"], json!([bob_status.id.to_string()]));
    assert_eq!(body["target_account"]["id"], stored_bob.id.to_string());

    // It is stored.
    let stored = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].target_account_id, stored_bob.id);
    assert_eq!(stored[0].status_ids, vec![bob_status.id]);
    assert_eq!(stored[0].forwarded, Some(true));

    // The Flag is delivered to bob's inbox, signed by the instance actor, with
    // the account and status uris in its object.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    let flag = &sent[0];
    assert_eq!(flag.inbox_url, bob.actor.inbox);
    assert_eq!(
        flag.key_id, "https://plamenu.test/actor#main-key",
        "Flag must be signed by the instance actor, not the reporter"
    );
    assert_eq!(flag.activity["type"], "Flag");
    assert_eq!(flag.activity["actor"], "https://plamenu.test/actor");
    assert_eq!(flag.activity["content"], "please review this spammer");
    assert_eq!(
        flag.activity["object"],
        json!([
            "https://remote.example/users/bob",
            "https://remote.example/users/bob/statuses/7",
        ])
    );
    // The report id lives on our host.
    assert_eq!(
        flag.activity["id"],
        format!("https://plamenu.test/reports/{}", stored[0].id)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reporting_a_local_account_is_never_federated(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), Arc::clone(&stub));

    let (code, body) = api(
        test_app_with(pool.clone(), Arc::clone(&stub)),
        "POST",
        "/api/v1/reports",
        Some(&token),
        // forward is ignored for a local target, like Mastodon.
        Some(json!({ "account_id": carol.id.to_string(), "forward": true })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    assert_eq!(body["forwarded"], false);
    assert_eq!(body["category"], "other");

    let stored = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].target_account_id, carol.id);

    // Nothing was queued for delivery.
    assert_eq!(delivery::run_due(&state).await, 0);
    assert!(stub.deliveries().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rule_ids_force_the_violation_category(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;

    let (code, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({
            "account_id": carol.id.to_string(),
            "category": "spam",
            "rule_ids": ["3", "7"],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    assert_eq!(body["category"], "violation", "rule_ids override category");
    assert_eq!(body["rule_ids"], json!(["3", "7"]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn report_input_validations(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;
    let (dave, dave_token) = user_with_token(&pool, "dave").await;
    let app = || test_app(pool.clone());

    // A missing account_id 404s, like `Account.find(nil)`.
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({ "comment": "no target" })),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // An over-long comment is a 422 with Mastodon's wording.
    let long = "x".repeat(1001);
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({ "account_id": carol.id.to_string(), "comment": long })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Comment is too long (maximum is 1000 characters)"
    );

    // An unknown category is a 422.
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({ "account_id": carol.id.to_string(), "category": "bogus" })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Citing a status that is not the target's 404s.
    let (code, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&dave_token),
        Some(json!({ "status": "dave's post" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let dave_status_id = posted["id"].as_str().unwrap();
    let _ = dave;
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/reports",
        Some(&token),
        // Reporting carol but attaching dave's status.
        Some(json!({
            "account_id": carol.id.to_string(),
            "status_ids": [dave_status_id],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reporting_requires_the_write_scope(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "alice").await;
    let email = "alice@plamenu.test";
    let hash = hash_password("pw").unwrap();
    user::create(&pool, alice.id, Some(email), &hash)
        .await
        .unwrap();
    let read_token = read_only_token(&pool, email).await;
    let _ = alice;
    let (carol, _) = user_with_token(&pool, "carol").await;

    let (code, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/reports",
        Some(&read_token),
        Some(json!({ "account_id": carol.id.to_string() })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body:?}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_flag_stores_a_report_about_a_local_user(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    // A local status by alice, posted through the API.
    let (code, posted) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({ "status": "alice post" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let status_id: i64 = posted["id"].as_str().unwrap().parse().unwrap();

    // The remote server's instance actor sends a Flag about alice + her post.
    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    let app = test_app_with(pool.clone(), Arc::clone(&stub));

    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/reports/abc",
        "type": "Flag",
        "actor": remote_actor.actor.id,
        "content": "abusive",
        "object": [
            "https://plamenu.test/users/alice",
            format!("https://plamenu.test/users/alice/statuses/{status_id}"),
        ],
    });
    assert_eq!(
        post_signed(app, &flag, &remote_actor.signer()).await,
        StatusCode::ACCEPTED
    );

    let sender = account::find_by_uri(&pool, &remote_actor.actor.id)
        .await
        .unwrap()
        .unwrap();
    let stored = report::list_by_reporter(&pool, sender.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].target_account_id, alice.id);
    assert_eq!(stored[0].status_ids, vec![status_id]);
    assert_eq!(stored[0].comment, "abusive");
    assert_eq!(stored[0].category, "other");
    assert_eq!(stored[0].forwarded, None);
    // The report id is kept because it lives on the sender's host.
    assert_eq!(
        stored[0].uri.as_deref(),
        Some("https://remote.example/reports/abc")
    );
}

/// A hostile `Flag` that packs the same local target thousands of times into
/// its `object` array must not fan out into thousands of reports and webhooks:
/// `flag_object_uris` deduplicates before dispatch, so exactly one report is
/// filed for the account.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_flag_with_repeated_targets_files_one_report(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;

    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    let app = test_app_with(pool.clone(), Arc::clone(&stub));

    // The same local account URL, repeated far past the fan-out cap.
    let objects: Vec<String> = std::iter::repeat_n("https://plamenu.test/users/alice", 5_000)
        .map(str::to_owned)
        .collect();
    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/reports/flood",
        "type": "Flag",
        "actor": remote_actor.actor.id,
        "content": "spam spam spam",
        "object": objects,
    });
    assert_eq!(
        post_signed(app, &flag, &remote_actor.signer()).await,
        StatusCode::ACCEPTED
    );

    let sender = account::find_by_uri(&pool, &remote_actor.actor.id)
        .await
        .unwrap()
        .unwrap();
    let stored = report::list_by_reporter(&pool, sender.id).await.unwrap();
    assert_eq!(
        stored.len(),
        1,
        "the repeated target deduplicates to a single report"
    );
    assert_eq!(stored[0].target_account_id, alice.id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_flag_naming_no_local_target_stores_nothing(pool: PgPool) {
    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    let app = test_app_with(pool.clone(), Arc::clone(&stub));

    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/reports/zzz",
        "type": "Flag",
        "actor": remote_actor.actor.id,
        "content": "about nobody we host",
        "object": ["https://other.example/users/ghost"],
    });
    assert_eq!(
        post_signed(app, &flag, &remote_actor.signer()).await,
        StatusCode::ACCEPTED
    );

    let sender = account::find_by_uri(&pool, &remote_actor.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        report::list_by_reporter(&pool, sender.id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A `Flag` naming many *distinct* local accounts must not fan out
/// past the object cap. Sixty distinct local targets — over `MAX_FLAG_OBJECTS`
/// (50) — file exactly fifty reports, not sixty (or thousands).
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_flag_distinct_targets_are_capped(pool: PgPool) {
    let mut object = Vec::new();
    for i in 0..60 {
        create_local_account(&pool, &format!("u{i}"), "U").await;
        object.push(format!("https://plamenu.test/users/u{i}"));
    }

    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    let app = test_app_with(pool.clone(), Arc::clone(&stub));

    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/reports/flood",
        "type": "Flag",
        "actor": remote_actor.actor.id,
        "content": "mass report",
        "object": object,
    });
    assert_eq!(
        post_signed(app, &flag, &remote_actor.signer()).await,
        StatusCode::ACCEPTED
    );

    let sender = account::find_by_uri(&pool, &remote_actor.actor.id)
        .await
        .unwrap()
        .unwrap();
    let stored = report::list_by_reporter(&pool, sender.id).await.unwrap();
    assert_eq!(
        stored.len(),
        50,
        "the object cap bounds the report fan-out at MAX_FLAG_OBJECTS, not one per named account",
    );
}

/// Local-target resolution for an inbound `Flag` is set-based. A
/// Flag naming many distinct local URLs that resolve to nothing issues the same
/// number of queries as one — proof the former per-URI `find_local_by_username`
/// / `find_by_id` chain is gone and cannot be driven serial by a hostile array.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_flag_resolution_query_count_is_flat(pool: PgPool) {
    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    // One shared state so the sender actor and any lazy caches persist across
    // deliveries (each `build_router` clone shares the same `Arc`'d state).
    let state = test_state_with(pool.clone(), Arc::clone(&stub));
    let app = || build_router(state.clone());

    // Delivers a Flag naming `n` distinct, non-existent local user URLs, so no
    // report is filed and the counter sees only the resolution work.
    let deliver = async |n: usize, id: &str| {
        let object: Vec<String> = (0..n)
            .map(|i| format!("https://plamenu.test/users/ghost{i}"))
            .collect();
        let flag = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Flag",
            "actor": remote_actor.actor.id,
            "content": "about nobody we host",
            "object": object,
        });
        assert_eq!(
            post_signed(app(), &flag, &remote_actor.signer()).await,
            StatusCode::ACCEPTED,
        );
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(common::QueryCounter(counter.clone())),
    );

    // Warm up: store the sending actor and warm any lazy caches so the two
    // measured deliveries differ only in how many URLs they name.
    deliver(1, "https://remote.example/reports/warm").await;

    counter.store(0, Ordering::Relaxed);
    deliver(1, "https://remote.example/reports/one").await;
    let one = counter.swap(0, Ordering::Relaxed);

    deliver(50, "https://remote.example/reports/fifty").await;
    let fifty = counter.load(Ordering::Relaxed);

    println!("inbound flag resolution: 1 url -> {one} queries, 50 -> {fifty}");
    assert_eq!(
        one, fifty,
        "50 distinct hostile local URLs must not add queries over naming one",
    );
}

/// The client's `status_ids` array is deduplicated before any
/// per-status work, so citing the same status twice stores (and forwards) it
/// once rather than amplifying into duplicate lookups and object URIs.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_status_ids_are_deduplicated(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    let bob_status = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/7",
            account_id: stored_bob.id,
            content: "<p>spam</p>",
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

    let (code, body) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({
            "account_id": stored_bob.id.to_string(),
            "comment": "spammer",
            "category": "spam",
            // The same status three times: a duplicate-stuffed array.
            "status_ids": [
                bob_status.id.to_string(),
                bob_status.id.to_string(),
                bob_status.id.to_string(),
            ],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    assert_eq!(body["status_ids"], json!([bob_status.id.to_string()]));

    let stored = report::list_by_reporter(&pool, alice.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status_ids, vec![bob_status.id]);
}

/// A `status_ids` array beyond the distinct cap is rejected with
/// 422 before any per-status database lookup runs.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_status_ids_over_the_cap_are_rejected(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;

    // 51 distinct ids — one past MAX_REPORT_STATUSES (50). They need not exist:
    // the cap fires before the batched status lookup runs.
    let status_ids: Vec<String> = (1..=51).map(|i| i.to_string()).collect();
    let (code, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/reports",
        Some(&token),
        Some(json!({
            "account_id": carol.id.to_string(),
            "comment": "flood",
            "status_ids": status_ids,
        })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Report status validation is set-based. Citing many of the
/// target's own statuses issues the same number of database queries as citing
/// one — proof the former per-status `find_by_id` loop is gone. Measured at the
/// action layer so the count reflects report creation itself, not the shared
/// rate-limiter/token caches the HTTP entry point warms.
#[sqlx::test(migrations = "../db/migrations")]
async fn report_status_validation_query_count_is_flat(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone()]),
    );

    // A pool of the target's own statuses to cite.
    let mut ids = Vec::new();
    for i in 0..40 {
        let stored = status::upsert_remote(
            &pool,
            status::NewRemoteStatus {
                title: None,
                object_type: None,
                external_url: None,
                uri: &format!("https://remote.example/users/bob/statuses/{i}"),
                account_id: stored_bob.id,
                content: "<p>spam</p>",
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
        ids.push(stored.id);
    }

    let file = async |cited: &[i64]| {
        plamenu::actions::create_report(
            &state,
            &alice,
            &stored_bob,
            plamenu::actions::ReportParams {
                comment: "spam",
                category: Some("spam"),
                forward: false,
                status_ids: cited,
                rule_ids: None,
            },
        )
        .await
        .unwrap();
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(common::QueryCounter(counter.clone())),
    );

    // Warm up any lazily-loaded state so the two measured calls differ only in
    // how many statuses they cite.
    file(&ids[..1]).await;

    counter.store(0, Ordering::Relaxed);
    file(&ids[..1]).await;
    let one = counter.swap(0, Ordering::Relaxed);

    file(&ids).await;
    let many = counter.load(Ordering::Relaxed);

    println!("report status validation: 1 status -> {one} queries, 40 -> {many}");
    assert_eq!(
        one, many,
        "citing 40 of the target's statuses must not add queries over citing one",
    );
}

/// Promotes an account to the seeded Admin role (which carries
/// `MANAGE_REPORTS`).
async fn make_admin(pool: &PgPool, account_id: i64) {
    let admin = role::find_by_name(pool, "Admin").await.unwrap().unwrap();
    assert!(
        role::assign_to_account(pool, account_id, Some(admin.id))
            .await
            .unwrap()
    );
}

/// The staff notifications of `account`, as the API serves them.
async fn staff_notifications(pool: &PgPool, token: &str) -> Vec<Value> {
    let (code, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types[]=admin.report",
        Some(token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    body.as_array().unwrap().clone()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn filing_a_report_notifies_managing_staff_with_the_report_embedded(pool: PgPool) {
    let (reporter, reporter_token) = user_with_token(&pool, "alice").await;
    let (staff, staff_token) = user_with_token(&pool, "mod").await;
    make_admin(&pool, staff.id).await;
    let (target, _) = user_with_token(&pool, "spammer").await;

    let (code, filed) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/reports",
        Some(&reporter_token),
        Some(json!({ "account_id": target.id.to_string(), "comment": "spam" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{filed:?}");

    let notifications = staff_notifications(&pool, &staff_token).await;
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    let n = &notifications[0];
    assert_eq!(n["type"], "admin.report");
    assert_eq!(n["account"]["id"], reporter.id.to_string());
    // The entity embeds the filed report (Mastodon's `report`), so a client
    // can jump straight to the moderation queue entry.
    assert_eq!(n["report"]["id"], filed["id"]);
    assert_eq!(n["report"]["comment"], "spam");

    // A non-staff account — the reporter included — hears nothing.
    let (code, own) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        own.as_array().unwrap().is_empty(),
        "the reporter must not be notified about their own report: {own:?}"
    );
}

/// A staff member filing a report is not notified about it (the standard
/// self-notification guard), while other staff still are.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_reporting_staff_member_is_not_self_notified(pool: PgPool) {
    let (mod1, mod1_token) = user_with_token(&pool, "modone").await;
    let (mod2, mod2_token) = user_with_token(&pool, "modtwo").await;
    make_admin(&pool, mod1.id).await;
    make_admin(&pool, mod2.id).await;
    let (target, _) = user_with_token(&pool, "spammer").await;

    let (code, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/reports",
        Some(&mod1_token),
        Some(json!({ "account_id": target.id.to_string() })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    assert!(staff_notifications(&pool, &mod1_token).await.is_empty());
    assert_eq!(staff_notifications(&pool, &mod2_token).await.len(), 1);
    let _ = mod2;
}

/// An inbound `Flag` raises the staff notification too, and a redelivered
/// `Flag` (same report URI) does not raise it twice.
#[sqlx::test(migrations = "../db/migrations")]
async fn an_inbound_flag_notifies_staff_once(pool: PgPool) {
    let (_alice, _) = user_with_token(&pool, "alice").await;
    let (staff, staff_token) = user_with_token(&pool, "mod").await;
    make_admin(&pool, staff.id).await;

    let remote_actor = RemoteUser::new("remote.example", "actor");
    let stub = StubFederation::with_actors([remote_actor.actor.clone()]);
    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/reports/dup",
        "type": "Flag",
        "actor": remote_actor.actor.id,
        "content": "abusive",
        "object": ["https://plamenu.test/users/alice"],
    });
    for _ in 0..2 {
        let app = test_app_with(pool.clone(), Arc::clone(&stub));
        assert_eq!(
            post_signed(app, &flag, &remote_actor.signer()).await,
            StatusCode::ACCEPTED
        );
    }

    let notifications = staff_notifications(&pool, &staff_token).await;
    assert_eq!(
        notifications.len(),
        1,
        "a redelivered Flag must not notify twice: {notifications:?}"
    );
    assert_eq!(notifications[0]["type"], "admin.report");
    assert!(notifications[0]["report"]["id"].is_string());
}
