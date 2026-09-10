//! Outbound federation: posting with fan-out, outgoing follows, the delivery
//! worker (including retry/backoff), and Accept/Reject handling.

mod common;

use std::fmt::Write;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::{actions, delivery};
use plamenu_db::{PgPool, account, follow, job};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const ALICE_URI: &str = "https://plamenu.test/users/alice";

/// Lets a remote user follow alice directly in the database (the inbound path
/// has its own tests).
async fn add_follower(pool: &PgPool, user: &RemoteUser) {
    let remote = plamenu::remote::store_remote_actor(pool, &user.actor)
        .await
        .unwrap();
    let alice = account::find_local_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(pool, remote.id, alice.id, None)
        .await
        .unwrap();
}

fn followers_digest<'a>(uris: impl Iterator<Item = &'a str>) -> String {
    let mut digest = [0u8; 32];
    for uri in uris {
        let hashed = Sha256::digest(uri.as_bytes());
        for (left, right) in digest.iter_mut().zip(hashed) {
            *left ^= right;
        }
    }
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    hex
}

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

#[sqlx::test(migrations = "../db/migrations")]
async fn post_fans_out_to_distinct_follower_inboxes(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    // Two followers on one instance (shared inbox) plus one on another.
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    for user in [&bob, &carol, &dave] {
        add_follower(&pool, user).await;
    }

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    let (status, deliveries) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "hello <world>",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(deliveries, 2, "two distinct inboxes, not three followers");
    assert_eq!(job::pending_count(&pool).await.unwrap(), 2);

    assert_eq!(delivery::run_due(&state).await, 2);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 2);
    let mut inboxes: Vec<&str> = sent.iter().map(|d| d.inbox_url.as_str()).collect();
    inboxes.sort_unstable();
    assert_eq!(
        inboxes,
        [
            "https://elsewhere.example/inbox",
            "https://remote.example/inbox"
        ]
    );
    for delivered in &sent {
        assert!(delivered.headers.is_empty());
        assert_eq!(delivered.activity["type"], "Create");
        assert_eq!(delivered.activity["actor"], ALICE_URI);
        let object = &delivered.activity["object"];
        assert_eq!(object["type"], "Note");
        // Text was escaped and wrapped.
        assert_eq!(object["content"], "<p>hello &lt;world&gt;</p>");
        assert_eq!(object["id"], format!("{ALICE_URI}/statuses/{}", status.id));
    }
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_post_delivery_carries_followers_synchronization_header(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    for user in [&bob, &carol, &dave] {
        add_follower(&pool, user).await;
    }

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "followers only",
            visibility: "private",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(delivery::run_due(&state).await, 2);
    let sent = stub.deliveries();
    let remote = sent
        .iter()
        .find(|delivery| delivery.inbox_url == "https://remote.example/inbox")
        .unwrap();
    let (_, header) = remote
        .headers
        .iter()
        .find(|(name, _)| name == "Collection-Synchronization")
        .unwrap();
    let expected_digest =
        followers_digest([bob.actor.id.as_str(), carol.actor.id.as_str()].into_iter());
    assert_eq!(
        header,
        &format!(
            "collectionId=\"https://plamenu.test/users/alice/followers\", digest=\"{expected_digest}\", url=\"https://plamenu.test/users/alice/followers_synchronization\""
        )
    );

    let elsewhere = sent
        .iter()
        .find(|delivery| delivery.inbox_url == "https://elsewhere.example/inbox")
        .unwrap();
    let (_, header) = elsewhere
        .headers
        .iter()
        .find(|(name, _)| name == "Collection-Synchronization")
        .unwrap();
    assert!(header.contains(&followers_digest([dave.actor.id.as_str()].into_iter())));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn content_warning_federates_as_summary(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    add_follower(&pool, &bob).await;
    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());

    actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "spider pics",
            visibility: "public",
            spoiler_text: "arachnophobia",
            language: Some("en"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    delivery::run_due(&state).await;

    let sent = stub.deliveries();
    let object = &sent[0].activity["object"];
    assert_eq!(object["summary"], "arachnophobia");
    assert_eq!(object["sensitive"], json!(true));
    assert_eq!(object["contentMap"], json!({"en": "<p>spider pics</p>"}));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn post_with_no_followers_enqueues_nothing(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());
    let (_, deliveries) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "into the void",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(deliveries, 0);
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn empty_post_is_rejected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());
    assert!(
        actions::post_status(
            &state,
            actions::PostParams {
                username: "alice",
                text: "  \n ",
                visibility: "public",
                in_reply_to_id: None,
                media_ids: &[],
                quoted_status_id: None,
                ..Default::default()
            },
        )
        .await
        .is_err()
    );
    assert!(
        actions::post_status(
            &state,
            actions::PostParams {
                username: "ghost",
                text: "hi",
                visibility: "public",
                in_reply_to_id: None,
                media_ids: &[],
                quoted_status_id: None,
                ..Default::default()
            },
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_deliveries_are_retried_with_backoff_then_succeed(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    add_follower(&pool, &bob).await;

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "flaky network",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // First attempt fails: the job stays queued, backed off.
    stub.set_fail_deliveries(true);
    assert_eq!(delivery::run_due(&state).await, 1);
    assert!(stub.deliveries().is_empty());
    assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
    // Backed off: not immediately due again.
    assert_eq!(delivery::run_due(&state).await, 0);

    // Once the remote recovers (and the backoff elapses), it goes through.
    stub.set_fail_deliveries(false);
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries().len(), 1);
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_remote_resolves_stores_and_enqueues(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());

    let outcome = actions::follow_remote(&state, "alice", "bob@remote.example")
        .await
        .unwrap();
    assert_eq!(outcome.target_uri, bob.actor.id);
    assert_eq!(outcome.target_inbox, bob.actor.inbox);

    // Pending until bob's server accepts.
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let remote = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, alice.id, remote.id)
            .await
            .unwrap(),
        Some(true)
    );

    // The Follow goes to bob's personal inbox, signed as alice.
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    assert_eq!(sent[0].key_id, format!("{ALICE_URI}#main-key"));
    assert_eq!(sent[0].activity["type"], "Follow");
    assert_eq!(sent[0].activity["actor"], ALICE_URI);
    assert_eq!(sent[0].activity["object"], bob.actor.id.as_str());

    // Unresolvable accounts and local targets fail cleanly.
    assert!(
        actions::follow_remote(&state, "alice", "ghost@nowhere.example")
            .await
            .is_err()
    );
    assert!(
        actions::follow_remote(&state, "alice", "alice@plamenu.test")
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accept_marks_outgoing_follow_accepted(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    actions::follow_remote(&state, "alice", "bob@remote.example")
        .await
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let follow_activity = stub.deliveries()[0].activity.clone();

    // Bob's server echoes the Follow back inside an Accept.
    let accept = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#accepts/1", bob.actor.id),
        "type": "Accept",
        "actor": bob.actor.id,
        "object": follow_activity,
    });
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &accept,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let remote = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, alice.id, remote.id)
            .await
            .unwrap(),
        Some(false)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accept_with_bare_follow_id_also_works(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    actions::follow_remote(&state, "alice", "bob@remote.example")
        .await
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let follow_id = stub.deliveries()[0].activity["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let accept = json!({
        "type": "Accept",
        "actor": bob.actor.id,
        "object": follow_id,
    });
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &accept,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let remote = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, alice.id, remote.id)
            .await
            .unwrap(),
        Some(false)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reject_removes_outgoing_follow(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    actions::follow_remote(&state, "alice", "bob@remote.example")
        .await
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let follow_activity = stub.deliveries()[0].activity.clone();

    let reject = json!({
        "type": "Reject",
        "actor": bob.actor.id,
        "object": follow_activity,
    });
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &reject,
        &bob.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let remote = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, alice.id, remote.id)
            .await
            .unwrap(),
        None
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_status_is_served_as_note(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());
    let (status, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "served note",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let uri = format!("/users/alice/statuses/{}", status.id);
    let request = Request::builder()
        .uri(&uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = test_app_with(pool.clone(), Arc::default())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let note: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(note["type"], "Note");
    assert_eq!(note["attributedTo"], ALICE_URI);
    assert_eq!(note["content"], "<p>served note</p>");
    assert_eq!(note["id"], format!("https://plamenu.test{uri}"));
    // The Note's `url` is the human web page, not the AP id.
    assert_eq!(
        note["url"],
        format!("https://plamenu.test/@alice/{}", status.id)
    );

    // Wrong user, missing status, missing AP accept header.
    let bad = |uri: String, accept: Option<&'static str>| {
        let mut builder = Request::builder().uri(uri);
        if let Some(accept) = accept {
            builder = builder.header(header::ACCEPT, accept);
        }
        builder.body(Body::empty()).unwrap()
    };
    let app = test_app_with(pool.clone(), Arc::default());
    let response = app
        .oneshot(bad(
            format!("/users/ghost/statuses/{}", status.id),
            Some("application/activity+json"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = test_app_with(pool.clone(), Arc::default())
        .oneshot(bad(
            "/users/alice/statuses/1".to_owned(),
            Some("application/activity+json"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A browser (no ActivityPub Accept) gets the human thread page, not a 406.
    let response = test_app_with(pool, Arc::default())
        .oneshot(bad(format!("/users/alice/statuses/{}", status.id), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html"),
    );
}
