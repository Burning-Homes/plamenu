//! Streaming API tests: a real TCP server with websocket clients, the
//! LISTEN/NOTIFY hub running against the test database.

mod common;

use std::time::Duration;

use common::{create_local_account, test_state_with};
use futures_util::{SinkExt, StreamExt};
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{AppState, actions, build_router};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, block, follow, mute, oauth, status, user};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How long to wait for a message that must arrive. Generous: the event
/// crosses Postgres NOTIFY and a render while every parallel test saturates
/// the CPU with argon2/RSA work — a passing wait returns instantly anyway.
const RECV_TIMEOUT: Duration = Duration::from_mins(1);

/// Starts the app on an ephemeral port with the streaming hub listening.
/// Returns only once the LISTEN connection is up — NOTIFY has no replay, so
/// events fired before that would be silently lost.
async fn start_server(state: &AppState) -> std::net::SocketAddr {
    let _hub = plamenu::streaming::spawn(state.clone());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::timeout(RECV_TIMEOUT, async {
        while !state.streaming.is_listening() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the streaming hub never started listening");
    addr
}

/// A local account with login credentials and an access token of `scopes`,
/// minted directly (the OAuth issuance flow is covered in `client_api.rs`).
async fn user_with_token(pool: &PgPool, username: &str, scopes: &str) -> (Account, String) {
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
            name: "streaming-tests",
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
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(row.id), scopes)
        .await
        .unwrap();
    (account, token)
}

async fn connect(addr: std::net::SocketAddr, query: &str) -> Socket {
    connect_result(addr, query).await.unwrap()
}

/// Like [`connect`], but surfaces the handshake result so a rejected upgrade
/// (e.g. the connection cap's 429) can be asserted.
async fn connect_result(addr: std::net::SocketAddr, query: &str) -> Result<Socket, WsError> {
    tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/streaming{query}"))
        .await
        .map(|(socket, _)| socket)
}

/// Waits until the hub registered `count` subscriptions, so a subscription
/// race can never make an event fire before anyone listens.
async fn wait_subscribed(state: &AppState, count: usize) {
    tokio::time::timeout(RECV_TIMEOUT, async {
        while state.streaming.subscription_count() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("subscription was never registered");
}

/// The next streaming message (skipping protocol frames), parsed.
async fn next_json(socket: &mut Socket) -> Value {
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let frame = socket
                .next()
                .await
                .expect("socket closed")
                .expect("websocket error");
            if let Message::Text(text) = frame {
                return serde_json::from_str::<Value>(text.as_str()).unwrap();
            }
        }
    })
    .await
    .expect("timed out waiting for a streaming message")
}

/// The `payload` of an entity-carrying message, parsed.
fn payload_json(message: &Value) -> Value {
    serde_json::from_str(message["payload"].as_str().unwrap()).unwrap()
}

async fn subscribe(socket: &mut Socket, body: Value) {
    socket.send(Message::text(body.to_string())).await.unwrap();
}

async fn post(state: &AppState, username: &str, text: &str) -> plamenu_db::status::Status {
    post_with_visibility(state, username, text, "public").await
}

async fn reply(
    state: &AppState,
    username: &str,
    text: &str,
    in_reply_to_id: i64,
) -> plamenu_db::status::Status {
    let (stored, _) = actions::post_status(
        state,
        actions::PostParams {
            username,
            text,
            visibility: "public",
            in_reply_to_id: Some(in_reply_to_id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    stored
}

async fn post_with_visibility(
    state: &AppState,
    username: &str,
    text: &str,
    visibility: &str,
) -> plamenu_db::status::Status {
    let (stored, _) = actions::post_status(
        state,
        actions::PostParams {
            username,
            text,
            visibility,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    stored
}

#[sqlx::test(migrations = "../db/migrations")]
async fn health_endpoint_answers_without_auth(pool: PgPool) {
    let state = test_state_with(pool, std::sync::Arc::default());
    let addr = start_server(&state).await;
    let body = reqwest_free_get(addr, "/api/v1/streaming/health").await;
    assert_eq!(body, "OK");
}

/// A dependency-free HTTP GET against the running test server.
async fn reqwest_free_get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn connections_require_a_valid_token(pool: PgPool) {
    let state = test_state_with(pool, std::sync::Arc::default());
    let addr = start_server(&state).await;

    for query in ["", "?access_token=not-a-token"] {
        let error =
            tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/streaming{query}"))
                .await
                .expect_err("handshake must be refused");
        match error {
            WsError::Http(response) => assert_eq!(response.status(), 401),
            other => panic!("expected an HTTP 401 rejection, got {other:?}"),
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn token_via_websocket_subprotocol_is_accepted_and_echoed(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;

    let mut request = format!("ws://{addr}/api/v1/streaming?stream=user")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", token.parse().unwrap());
    let (_socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()["sec-websocket-protocol"]
            .to_str()
            .unwrap(),
        token
    );
    wait_subscribed(&state, 1).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn same_origin_web_session_cookie_authenticates_stream(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;

    let mut request = format!("ws://{addr}/api/v1/streaming?stream=user:notification")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "cookie",
        format!("__Host-plamenu_session={token}").parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("origin", "https://plamenu.test".parse().unwrap());
    let (_socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    wait_subscribed(&state, 1).await;

    let mut foreign = format!("ws://{addr}/api/v1/streaming?stream=user:notification")
        .into_client_request()
        .unwrap();
    foreign.headers_mut().insert(
        "cookie",
        format!("__Host-plamenu_session={token}").parse().unwrap(),
    );
    foreign
        .headers_mut()
        .insert("origin", "https://attacker.example".parse().unwrap());
    let error = tokio_tungstenite::connect_async(foreign)
        .await
        .expect_err("a foreign origin must not spend the session cookie");
    match error {
        WsError::Http(response) => assert_eq!(response.status(), 401),
        other => panic!("expected an HTTP 401 rejection, got {other:?}"),
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn user_stream_carries_own_and_followed_posts(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 1).await;

    let bobs = post(&state, "bob", "hello followers").await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["user"]));
    assert_eq!(message["event"], "update");
    let entity = payload_json(&message);
    assert_eq!(entity["id"], bobs.id.to_string());
    assert_eq!(entity["account"]["username"], "bob");

    let own = post(&state, "alice", "and my own").await;
    let message = next_json(&mut socket).await;
    assert_eq!(payload_json(&message)["id"], own.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_streams_route_by_origin_and_skip_non_followers_user_stream(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_carol, token) = user_with_token(&pool, "carol", "read").await;
    create_local_account(&pool, "bob", "bob").await;

    // carol does not follow bob; she watches her user stream and the local
    // public stream.
    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let bobs = post(&state, "bob", "to the town square").await;
    // The first message must be the public:local update — were a user-stream
    // event wrongly emitted for a non-follower, it would have arrived first.
    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["public:local"]));
    assert_eq!(message["event"], "update");
    assert_eq!(payload_json(&message)["id"], bobs.id.to_string());

    // Her own post reaches both subscriptions, user first.
    let own = post(&state, "carol", "me too").await;
    let first = next_json(&mut socket).await;
    let second = next_json(&mut socket).await;
    assert_eq!(first["stream"], json!(["user"]));
    assert_eq!(second["stream"], json!(["public:local"]));
    assert_eq!(payload_json(&second)["id"], own.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_posts_reach_followers_but_not_public_streams(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let private = post_with_visibility(&state, "bob", "followers only", "private").await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["user"]));
    assert_eq!(payload_json(&message)["id"], private.id.to_string());

    // A public control post: its public-stream event must be the next
    // public message — the private post never produced one.
    let control = post(&state, "bob", "now public").await;
    let first = next_json(&mut socket).await;
    let second = next_json(&mut socket).await;
    assert_eq!(first["stream"], json!(["user"]));
    assert_eq!(second["stream"], json!(["public"]));
    assert_eq!(payload_json(&second)["id"], control.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_only_posts_reach_home_and_local_public_streams_only(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public"}),
    )
    .await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    wait_subscribed(&state, 3).await;

    let local = post_with_visibility(&state, "bob", "same instance", "local").await;
    let home = next_json(&mut socket).await;
    let local_public = next_json(&mut socket).await;
    assert_eq!(home["stream"], json!(["user"]));
    assert_eq!(payload_json(&home)["id"], local.id.to_string());
    assert_eq!(local_public["stream"], json!(["public:local"]));
    let entity = payload_json(&local_public);
    assert_eq!(entity["id"], local.id.to_string());
    assert_eq!(entity["visibility"], "local");

    actions::delete_status(&state, &bob, local.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let home_delete = next_json(&mut socket).await;
    let local_public_delete = next_json(&mut socket).await;
    assert_eq!(home_delete["event"], "delete");
    assert_eq!(home_delete["stream"], json!(["user"]));
    assert_eq!(home_delete["payload"], local.id.to_string());
    assert_eq!(local_public_delete["event"], "delete");
    assert_eq!(local_public_delete["stream"], json!(["public:local"]));
    assert_eq!(local_public_delete["payload"], local.id.to_string());

    // A public control post: its public-stream event must be the next public
    // message, proving the local-only create/delete produced no `public` event.
    let control = post(&state, "bob", "public again").await;
    let first = next_json(&mut socket).await;
    let second = next_json(&mut socket).await;
    assert_eq!(first["stream"], json!(["user"]));
    assert_eq!(second["stream"], json!(["public"]));
    assert_eq!(payload_json(&second)["id"], control.id.to_string());
}

/// The shared streams must agree with the timeline queries, or a live
/// page and a reload of it disagree. Hashtag streams are deliberately exempt —
/// the tag *timeline* keeps replies, so suppressing them here would be the
/// same disagreement in the other direction.
#[sqlx::test(migrations = "../db/migrations")]
async fn replies_to_other_people_skip_the_public_streams(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;
    create_local_account(&pool, "bob", "bob").await;

    // Only the two streams under test: the `user` stream would interleave
    // alice's own posts and bob's mention notification, which this says
    // nothing about.
    let mut socket = connect(addr, &format!("?access_token={token}")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public"}),
    )
    .await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "hashtag", "tag": "cats"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let root = post(&state, "alice", "a thought").await;
    let first = next_json(&mut socket).await;
    assert_eq!(first["stream"], json!(["public"]));
    assert_eq!(payload_json(&first)["id"], root.id.to_string());

    // alice's own follow-up is a self-thread, which the public timeline keeps.
    let self_thread = reply(&state, "alice", "…continued", root.id).await;
    let second = next_json(&mut socket).await;
    assert_eq!(second["stream"], json!(["public"]));
    assert_eq!(payload_json(&second)["id"], self_thread.id.to_string());

    // bob answering alice still reaches the hashtag stream his post carries —
    // the tag timeline keeps replies — but not the shared public one.
    let bobs_reply = reply(&state, "bob", "disagree, about #cats", root.id).await;
    let tagged = next_json(&mut socket).await;
    assert_eq!(tagged["stream"], json!(["hashtag", "cats"]));
    assert_eq!(payload_json(&tagged)["id"], bobs_reply.id.to_string());

    // A public control post: its public-stream event must be the next message,
    // proving bob's reply produced none.
    let control = post(&state, "bob", "unrelated").await;
    let last = next_json(&mut socket).await;
    assert_eq!(last["stream"], json!(["public"]));
    assert_eq!(payload_json(&last)["id"], control.id.to_string());
}

/// The home (`user`) stream applies the per-follow reply switch,
/// or a live page and a reload of it disagree about which replies belong in
/// the feed. The exemptions and the judge-a-boost-by-its-target rule are the
/// timeline query's, asked here without re-running it.
#[sqlx::test(migrations = "../db/migrations")]
async fn user_stream_applies_the_per_follow_reply_flag(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    let stranger = create_local_account(&pool, "stranger", "stranger").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    follow::update_settings(&pool, alice.id, bob.id, None, Some(false), None, None)
        .await
        .unwrap();

    let strangers_post = post(&state, "stranger", "a thought").await;
    let strangers_comment = reply(&state, "stranger", "and more", strangers_post.id).await;
    let bobs_root = post(&state, "bob", "bob's own").await;

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 1).await;

    // Answering a stranger: filtered, so it produces no event at all.
    reply(&state, "bob", "@stranger no", strangers_post.id).await;
    // Announcing a comment: filtered by its target.
    actions::reblog_status(&state, &bob, strangers_comment.id)
        .await
        .unwrap();

    // A self-thread is exempt, and is the first event that may arrive — were
    // either of the two above wrongly streamed, it would have come first.
    let self_thread = reply(&state, "bob", "…continued", bobs_root.id).await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["user"]));
    assert_eq!(payload_json(&message)["id"], self_thread.id.to_string());

    // An announce of a top-level post is kept.
    let boost = actions::reblog_status(&state, &bob, strangers_post.id)
        .await
        .unwrap();
    let message = next_json(&mut socket).await;
    assert_eq!(payload_json(&message)["id"], boost.id.to_string());

    let _ = alice;
    let _ = stranger;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hashtag_stream_subscribes_and_unsubscribes(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_carol, token) = user_with_token(&pool, "carol", "read").await;
    create_local_account(&pool, "bob", "bob").await;

    let mut socket = connect(addr, &format!("?access_token={token}")).await;
    // Tags are normalized: subscribing "Cats" matches a #cats post.
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "hashtag", "tag": "Cats"}),
    )
    .await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let tagged = post(&state, "bob", "all about #cats").await;
    // public:local is routed before hashtag streams.
    let first = next_json(&mut socket).await;
    let second = next_json(&mut socket).await;
    assert_eq!(first["stream"], json!(["public:local"]));
    assert_eq!(second["stream"], json!(["hashtag", "cats"]));
    assert_eq!(payload_json(&second)["id"], tagged.id.to_string());

    // After unsubscribing, a #cats post yields only the public:local
    // message; the next message after it belongs to the following post.
    socket
        .send(Message::text(
            json!({"type": "unsubscribe", "stream": "hashtag", "tag": "Cats"}).to_string(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(RECV_TIMEOUT, async {
        while state.streaming.subscription_count() > 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unsubscribe never landed");

    let after = post(&state, "bob", "more #cats").await;
    let control = post(&state, "bob", "no tags here").await;
    let first = next_json(&mut socket).await;
    let second = next_json(&mut socket).await;
    assert_eq!(payload_json(&first)["id"], after.id.to_string());
    assert_eq!(
        payload_json(&second)["id"],
        control.id.to_string(),
        "an unsubscribed hashtag stream must produce no message"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_stream_to_user_and_notification_channels(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;

    let mut user_socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    let mut notification_socket = connect(
        addr,
        &format!("?access_token={token}&stream=user:notification"),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let post_row = post(&state, "alice", "favourite me").await; // own update on user stream
    let own_update = next_json(&mut user_socket).await;
    assert_eq!(own_update["event"], "update");

    actions::favourite_status(&state, &bob, post_row.id)
        .await
        .unwrap();

    let on_user = next_json(&mut user_socket).await;
    assert_eq!(on_user["stream"], json!(["user"]));
    assert_eq!(on_user["event"], "notification");
    let entity = payload_json(&on_user);
    assert_eq!(entity["type"], "favourite");
    assert_eq!(entity["account"]["username"], "bob");
    assert_eq!(entity["status"]["id"], post_row.id.to_string());

    // The dedicated notification stream got exactly the notification — the
    // earlier status update never reached it.
    let on_notifications = next_json(&mut notification_socket).await;
    assert_eq!(on_notifications["stream"], json!(["user:notification"]));
    assert_eq!(on_notifications["event"], "notification");
    assert_eq!(payload_json(&on_notifications)["type"], "favourite");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn policy_filtered_notifications_never_stream(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    // Alice filters senders she doesn't follow; bob is a stranger, so his
    // favourite is stored hidden and must not reach the live stream.
    let policy = plamenu_db::notification_policy::Policy {
        for_not_following: plamenu_db::notification_policy::Disposition::Filter,
        ..Default::default()
    };
    plamenu_db::notification_policy::upsert(&pool, alice.id, policy)
        .await
        .unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 1).await;

    let mine = post(&state, "alice", "my own post").await;
    let own = next_json(&mut socket).await;
    assert_eq!(own["event"], "update");

    actions::favourite_status(&state, &bob, mine.id)
        .await
        .unwrap();
    // The notification was stored (hidden), so the suppression below is the
    // streaming gate, not a missing row.
    let hidden = plamenu_db::notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter {
            include_filtered: true,
            ..Default::default()
        },
        10,
    )
    .await
    .unwrap();
    assert_eq!(hidden.len(), 1);
    assert!(hidden[0].filtered);

    // The next stream message is alice's own control post, not the
    // filtered notification.
    let control = post(&state, "alice", "control post").await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "update");
    assert_eq!(payload_json(&message)["id"], control.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn statuses_only_scope_gets_no_notification_events(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read:statuses").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 1).await;

    // The notification stream is out of scope entirely.
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "user:notification"}),
    )
    .await;
    let error = next_json(&mut socket).await;
    assert_eq!(
        error["error"],
        "Access token does not have the required scopes"
    );
    assert_eq!(error["status"], 401);

    let mine = post(&state, "alice", "my own post").await;
    let own = next_json(&mut socket).await;
    assert_eq!(payload_json(&own)["id"], mine.id.to_string());

    // A favourite notification is suppressed on this user stream; the next
    // message is bob's control post, not the notification.
    actions::favourite_status(&state, &bob, mine.id)
        .await
        .unwrap();
    let control = post(&state, "bob", "control post").await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "update");
    assert_eq!(payload_json(&message)["id"], control.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn blocks_and_mutes_filter_streams(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (carol, token) = user_with_token(&pool, "carol", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    let dave = create_local_account(&pool, "dave", "dave").await;
    // carol follows both, blocks bob, and mutes dave's notifications.
    follow::create(&pool, carol.id, bob.id, None).await.unwrap();
    follow::create(&pool, carol.id, dave.id, None)
        .await
        .unwrap();
    block::create(&pool, carol.id, bob.id, None).await.unwrap();
    mute::upsert(&pool, carol.id, dave.id, true, None)
        .await
        .unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    // bob is blocked, dave is muted: neither reaches carol's home or the
    // public stream she watches.
    post(&state, "bob", "from the blocked").await;
    post(&state, "dave", "from the muted").await;
    // dave's favourite of carol's own post is notification-muted.
    let own = post(&state, "carol", "my own").await;
    let first = next_json(&mut socket).await; // own update, user stream
    let second = next_json(&mut socket).await; // own update, public:local
    assert_eq!(payload_json(&first)["id"], own.id.to_string());
    assert_eq!(payload_json(&second)["id"], own.id.to_string());

    actions::favourite_status(&state, &dave, own.id)
        .await
        .unwrap();
    // Control: an unfiltered favourite arrives; dave's never did.
    let erin = create_local_account(&pool, "erin", "erin").await;
    actions::favourite_status(&state, &erin, own.id)
        .await
        .unwrap();
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "notification");
    assert_eq!(payload_json(&message)["account"]["username"], "erin");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn boosts_stream_and_unboosts_delete(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let _bob = create_local_account(&pool, "bob", "bob").await;
    let carol = create_local_account(&pool, "carol", "carol").await;
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();

    let original = post(&state, "bob", "boost-worthy").await;
    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 1).await;

    let boost = actions::reblog_status(&state, &carol, original.id)
        .await
        .unwrap();
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "update");
    let entity = payload_json(&message);
    assert_eq!(entity["id"], boost.id.to_string());
    assert_eq!(entity["account"]["username"], "carol");
    assert_eq!(entity["reblog"]["id"], original.id.to_string());

    actions::unreblog_status(&state, &carol, original.id)
        .await
        .unwrap();
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "delete");
    assert_eq!(message["payload"], boost.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edits_and_deletes_stream_to_the_audience(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "hashtag", "tag": "version"}),
    )
    .await;
    wait_subscribed(&state, 3).await;

    let item = post(&state, "bob", "first #version").await;
    for expected_stream in [
        json!(["user"]),
        json!(["public:local"]),
        json!(["hashtag", "version"]),
    ] {
        let message = next_json(&mut socket).await;
        assert_eq!(message["event"], "update");
        assert_eq!(message["stream"], expected_stream);
    }

    let edited = actions::edit_status(
        &state,
        &bob,
        item.id,
        actions::EditParams {
            text: Some("second #version"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for expected_stream in [
        json!(["user"]),
        json!(["public:local"]),
        json!(["hashtag", "version"]),
    ] {
        let message = next_json(&mut socket).await;
        assert_eq!(message["event"], "status.update");
        assert_eq!(message["stream"], expected_stream);
        let entity = payload_json(&message);
        assert_eq!(entity["id"], edited.id.to_string());
        assert!(entity["content"].as_str().unwrap().contains("second"));
    }

    actions::delete_status(&state, &bob, item.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    for expected_stream in [
        json!(["user"]),
        json!(["public:local"]),
        json!(["hashtag", "version"]),
    ] {
        let message = next_json(&mut socket).await;
        assert_eq!(message["event"], "delete");
        assert_eq!(message["stream"], expected_stream);
        assert_eq!(message["payload"], item.id.to_string());
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_messages_stream_conversation_events(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;
    let bob = create_local_account(&pool, "bob", "bob").await;

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "direct"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    let dm = post_with_visibility(&state, "bob", "@alice psst", "direct").await;

    // The conversation rows now commit inside the post's transaction, so their
    // `NOTIFY` fires at commit — ahead of the mention notification, which is
    // created after the commit. So the conversation event lands
    // on both streams first, then the notification on the user stream.
    let on_user = next_json(&mut socket).await;
    assert_eq!(on_user["event"], "conversation");
    assert_eq!(on_user["stream"], json!(["user"]));
    let entity = payload_json(&on_user);
    assert_eq!(entity["unread"], true);
    assert_eq!(entity["last_status"]["id"], dm.id.to_string());
    assert_eq!(entity["accounts"][0]["username"], "bob");

    let on_direct = next_json(&mut socket).await;
    assert_eq!(on_direct["event"], "conversation");
    assert_eq!(on_direct["stream"], json!(["direct"]));

    let notification = next_json(&mut socket).await;
    assert_eq!(notification["event"], "notification");
    assert_eq!(payload_json(&notification)["type"], "mention");

    // Deleting the DM streams a delete to the participants' streams.
    actions::delete_status(&state, &bob, dm.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "delete");
    assert_eq!(message["payload"], dm.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn subscription_errors_are_reported_in_band(pool: PgPool) {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;

    let mut socket = connect(addr, &format!("?access_token={token}")).await;

    subscribe(&mut socket, json!({"type": "subscribe", "stream": "nope"})).await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["error"], "Unknown stream type");
    assert_eq!(error["status"], 400);

    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "hashtag"}),
    )
    .await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["error"], "Missing tag name parameter");
    assert_eq!(error["status"], 400);

    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "list", "list": "1"}),
    )
    .await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["error"], "Not authorized to stream this list");
    assert_eq!(error["status"], 401);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_statuses_stream_via_the_inbox(pool: PgPool) {
    use common::RemoteUser;

    let remote = RemoteUser::new("remote.example", "zed");
    let federation = common::StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), federation);
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;

    // alice follows the remote account, so its posts hit her home stream.
    let stored_remote = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    follow::create(&pool, alice.id, stored_remote.id, None)
        .await
        .unwrap();

    let mut socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "public:remote"}),
    )
    .await;
    wait_subscribed(&state, 2).await;

    // Deliver a signed Create through the real inbox route.
    let note_uri = format!("{}/notes/1", remote.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activity", note_uri),
        "type": "Create",
        "actor": remote.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": remote.actor.id,
            "content": "<p>from afar</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    });
    deliver_signed(&state, &remote, &create).await;

    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["user"]));
    assert_eq!(message["event"], "update");
    let entity = payload_json(&message);
    assert_eq!(entity["account"]["acct"], "zed@remote.example");
    assert_eq!(entity["content"], "<p>from afar</p>");
    let message = next_json(&mut socket).await;
    assert_eq!(message["stream"], json!(["public:remote"]));

    // And the remote delete streams out too.
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#delete", note_uri),
        "type": "Delete",
        "actor": remote.actor.id,
        "object": note_uri,
    });
    deliver_signed(&state, &remote, &delete).await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "delete");
    assert_eq!(message["payload"], stored.id.to_string());
}

/// Signs `activity` as the remote user and posts it to the shared inbox
/// through the full router.
async fn deliver_signed(state: &AppState, remote: &common::RemoteUser, activity: &Value) {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let bytes = serde_json::to_vec(activity).unwrap();
    let signed = remote.signer().sign_post(
        common::TEST_DOMAIN,
        "/inbox",
        &bytes,
        std::time::SystemTime::now(),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), 202);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_stream_carries_member_statuses(pool: PgPool) {
    use plamenu_db::list;

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, token) = user_with_token(&pool, "alice", "read").await;
    let (bob, _) = user_with_token(&pool, "bob", "read").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    // An exclusive list: the member's posts move from home to the list.
    let owned = list::create(&pool, alice.id, "Friends", "list", true)
        .await
        .unwrap();
    list::add_members(&pool, owned.id, alice.id, &[bob.id])
        .await
        .unwrap()
        .unwrap();

    let mut list_socket = connect(
        addr,
        &format!("?access_token={token}&stream=list&list={}", owned.id),
    )
    .await;
    let mut home_socket = connect(addr, &format!("?access_token={token}&stream=user")).await;
    wait_subscribed(&state, 2).await;

    let stored = post(&state, "bob", "<p>for the list</p>").await;
    let message = next_json(&mut list_socket).await;
    assert_eq!(message["stream"], json!(["list", owned.id.to_string()]));
    assert_eq!(message["event"], "update");
    assert_eq!(payload_json(&message)["id"], stored.id.to_string());

    // The exclusive membership keeps bob off alice's home stream: the next
    // home event is alice's own post, not bob's.
    let own = post(&state, "alice", "<p>own post</p>").await;
    let home_message = next_json(&mut home_socket).await;
    assert_eq!(payload_json(&home_message)["id"], own.id.to_string());

    // Edits reach the list stream as status.update.
    actions::edit_status(
        &state,
        &bob,
        stored.id,
        actions::EditParams {
            text: Some("edited"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let message = next_json(&mut list_socket).await;
    assert_eq!(message["event"], "status.update");

    // Deletions send the bare id.
    actions::delete_status(&state, &bob, stored.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let message = next_json(&mut list_socket).await;
    assert_eq!(message["event"], "delete");
    assert_eq!(message["payload"], stored.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_stream_is_owner_only(pool: PgPool) {
    use plamenu_db::list;

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, _) = user_with_token(&pool, "alice", "read").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read").await;
    let owned = list::create(&pool, alice.id, "Friends", "list", false)
        .await
        .unwrap();

    // bob subscribing to alice's list gets Mastodon's in-band error.
    let mut socket = connect(addr, &format!("?access_token={bob_token}")).await;
    subscribe(
        &mut socket,
        json!({"type": "subscribe", "stream": "list", "list": owned.id.to_string()}),
    )
    .await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["error"], "Not authorized to stream this list");
    assert_eq!(error["status"], 401);

    subscribe(&mut socket, json!({"type": "subscribe", "stream": "list"})).await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["error"], "Missing list name parameter");
    assert_eq!(error["status"], 400);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn announcement_publish_and_delete_broadcast_to_user_streams(pool: PgPool) {
    use plamenu_db::announcement::{self, NewAnnouncement};

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;

    let mut socket = connect(addr, &format!("?stream=user&access_token={token}")).await;
    wait_subscribed(&state, 1).await;

    let ann = announcement::create(
        &pool,
        NewAnnouncement {
            text: "Maintenance #ops",
            scheduled_at: Some(time::OffsetDateTime::now_utc() + time::Duration::hours(1)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!ann.published);

    let published = announcement::publish(&pool, ann.id).await.unwrap().unwrap();
    plamenu::streaming::announcement_published(&state, published.id).await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "announcement");
    assert_eq!(message["stream"], json!(["user"]));
    let payload = payload_json(&message);
    assert_eq!(payload["id"], ann.id.to_string());
    assert!(payload["content"].as_str().unwrap().contains("Maintenance"));
    assert_eq!(payload["read"], false);

    assert!(announcement::delete(&pool, ann.id).await.unwrap());
    plamenu::streaming::announcement_deleted(&state, ann.id).await;
    let message = next_json(&mut socket).await;
    assert_eq!(message["event"], "announcement.delete");
    assert_eq!(message["payload"], ann.id.to_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn per_account_connection_cap_refuses_excess_sockets(pool: PgPool) {
    use plamenu::streaming::Hub;

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (_alice, token) = user_with_token(&pool, "alice", "read").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read").await;

    // Fill the per-account ceiling with live sockets.
    let mut sockets = Vec::new();
    for _ in 0..Hub::MAX_CONNECTIONS_PER_VIEWER {
        sockets.push(connect(addr, &format!("?access_token={token}")).await);
    }

    // The next connection for the same account is refused with 429...
    let error = connect_result(addr, &format!("?access_token={token}"))
        .await
        .expect_err("the per-account cap must refuse the extra socket");
    match error {
        WsError::Http(response) => assert_eq!(response.status(), 429),
        other => panic!("expected an HTTP 429 rejection, got {other:?}"),
    }

    // ...while a different account still connects.
    let _bob = connect(addr, &format!("?access_token={bob_token}")).await;

    // Closing one socket frees a slot for the capped account again. The
    // dropped socket's server task must run its cleanup (releasing the
    // admission guard) first, so retry briefly.
    sockets.pop();
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            if connect_result(addr, &format!("?access_token={token}"))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a freed slot lets the capped account reconnect");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn announcement_reaction_broadcasts_to_user_streams(pool: PgPool) {
    use plamenu_db::announcement::{self, NewAnnouncement};

    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let addr = start_server(&state).await;
    let (alice, alice_token) = user_with_token(&pool, "alice", "read").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read").await;

    // Two users on the `user` stream both hear the broadcast.
    let mut alice_socket = connect(addr, &format!("?stream=user&access_token={alice_token}")).await;
    let mut bob_socket = connect(addr, &format!("?stream=user&access_token={bob_token}")).await;
    wait_subscribed(&state, 2).await;

    let ann = announcement::create(
        &pool,
        NewAnnouncement {
            text: "React!",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    announcement::create_reaction(&pool, alice.id, ann.id, "🎉", None)
        .await
        .unwrap();
    plamenu::streaming::announcement_reaction(&state, ann.id, "🎉").await;

    for socket in [&mut alice_socket, &mut bob_socket] {
        let message = next_json(socket).await;
        assert_eq!(message["event"], "announcement.reaction");
        let payload = payload_json(&message);
        assert_eq!(payload["name"], "🎉");
        assert_eq!(payload["count"], 1);
        // The broadcast tally is anonymous, like Mastodon's worker.
        assert_eq!(payload["me"], false);
        assert_eq!(payload["announcement_id"], ann.id.to_string());
    }
}
