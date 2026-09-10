//! Account-migration (`Move`) integration tests, over the full router with
//! real HTTP signatures and the network stubbed out.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::remote::store_remote_actor;
use plamenu::{delivery, migration};
use plamenu_db::{
    PgPool, account, account_alias, account_migration, account_move_job, block, follow, mute, user,
};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Signs `body` as a POST to `path` and runs it through the router.
async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let headers = signer.sign_post(TEST_DOMAIN, path, &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", headers.host)
        .header("date", headers.date)
        .header("digest", headers.digest)
        .header("signature", headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Fetches a local actor document as JSON.
async fn get_actor(app: Router, path: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("accept", "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// A `Move` activity from `source` to `target`.
fn move_activity(source: &RemoteUser, target_uri: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#moves/1", source.actor.id),
        "type": "Move",
        "actor": source.actor.id,
        "object": source.actor.id,
        "target": target_uri,
    })
}

/// Builds a remote user whose actor declares `alias` in `alsoKnownAs`.
fn aliased(domain: &str, username: &str, alias: &str) -> RemoteUser {
    let mut user = RemoteUser::new(domain, username);
    user.actor.also_known_as = Some(json!([alias]));
    user
}

/// Posts a `Move` through the inbox and then drains the replay queue.
///
/// The inbox records the redirect synchronously and *queues* the relationship
/// replay: re-following the target for every local follower federates two
/// activities each, which must not hold the sending server's request open. So
/// anything asserting on followers, blocks or mutes has to run the worker the
/// server runs.
async fn deliver_move(
    pool: &PgPool,
    stub: &std::sync::Arc<StubFederation>,
    source: &RemoteUser,
    target_uri: &str,
) -> StatusCode {
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &move_activity(source, target_uri),
        &source.signer(),
    )
    .await;
    migration::run_due(&test_state_with(pool.clone(), stub.clone())).await;
    status
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_repoints_local_followers(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    let bob_new = aliased("new.example", "bob", &bob_old.actor.id);
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    // alice follows bob's old account (an accepted follow).
    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();

    let status = deliver_move(&pool, &stub, &bob_old, &bob_new.actor.id).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The redirect is recorded and the follow re-points to the new account.
    let old = account::find_by_uri(&pool, &bob_old.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.moved_to_uri.as_deref(), Some(bob_new.actor.id.as_str()));
    let new = account::find_by_uri(&pool, &bob_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, alice.id, new.id)
            .await
            .unwrap()
            .is_some(),
        "alice now follows the new account"
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_none(),
        "the old follow is dropped"
    );

    // A federated Follow to the new inbox and an Undo(Follow) to the old.
    let state = test_state_with(pool.clone(), stub.clone());
    delivery::run_due(&state).await;
    let deliveries = stub.deliveries();
    let to_new = deliveries
        .iter()
        .find(|d| d.inbox_url == bob_new.actor.inbox)
        .expect("a delivery to the new inbox");
    assert_eq!(to_new.activity["type"], "Follow");
    assert_eq!(to_new.activity["object"], bob_new.actor.id);
    let to_old = deliveries
        .iter()
        .find(|d| d.inbox_url == bob_old.actor.inbox)
        .expect("a delivery to the old inbox");
    assert_eq!(to_old.activity["type"], "Undo");
    assert_eq!(to_old.activity["object"]["type"], "Follow");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_requires_alias_back_reference(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    // bob_new does NOT list bob_old as an alias — a hijack attempt.
    let bob_new = RemoteUser::new("new.example", "bob");
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();

    let status = deliver_move(&pool, &stub, &bob_old, &bob_new.actor.id).await;
    assert_eq!(status, StatusCode::ACCEPTED); // accepted, but ignored

    let old = account::find_by_uri(&pool, &bob_old.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        old.moved_to_uri, None,
        "no redirect without the back-reference"
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_some(),
        "the original follow is untouched"
    );
    assert!(stub.deliveries().is_empty());
}

/// The inbox must not do the follower re-pointing itself: that work is
/// unbounded in the moving account's popularity, and doing it inline held the
/// sending server's request open until it timed out and redelivered the same
/// `Move` (the 2026-07 bench audit's finding, in git history). What the
/// request owes is the redirect and a queued replay.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_queues_the_replay_instead_of_running_it_inline(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    let bob_new = aliased("new.example", "bob", &bob_old.actor.id);
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();

    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &move_activity(&bob_old, &bob_new.actor.id),
        &bob_old.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Recorded synchronously: clients see the new location straight away.
    let old = account::find_by_uri(&pool, &bob_old.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.moved_to_uri.as_deref(), Some(bob_new.actor.id.as_str()));
    // Queued, not done.
    assert_eq!(
        account_move_job::pending_for_source(&pool, old.id)
            .await
            .unwrap(),
        1
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_some(),
        "the request itself re-points nothing"
    );

    // A redelivery of the same Move does not queue a second replay.
    let status = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &move_activity(&bob_old, &bob_new.actor.id),
        &bob_old.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        account_move_job::pending_for_source(&pool, old.id)
            .await
            .unwrap(),
        1
    );

    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(migration::run_due(&state).await, 1);
    let new = account::find_by_uri(&pool, &bob_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, alice.id, new.id)
            .await
            .unwrap()
            .is_some(),
        "the worker does the re-pointing"
    );
    // Settled jobs are removed, so the queue drains rather than spinning.
    assert_eq!(
        account_move_job::pending_for_source(&pool, old.id)
            .await
            .unwrap(),
        0
    );
    assert_eq!(migration::run_due(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_carries_local_blocks_and_mutes_over(pool: PgPool) {
    let blocker = create_local_account(&pool, "blocker", "Blocker").await;
    let muter = create_local_account(&pool, "muter", "Muter").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    let bob_new = aliased("new.example", "bob", &bob_old.actor.id);
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    block::create(&pool, blocker.id, old.id, None)
        .await
        .unwrap();
    mute::upsert(&pool, muter.id, old.id, true, None)
        .await
        .unwrap();

    let status = deliver_move(&pool, &stub, &bob_old, &bob_new.actor.id).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let new = account::find_by_uri(&pool, &bob_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        block::exists(&pool, blocker.id, new.id).await.unwrap(),
        "the block follows the account to its new home"
    );
    assert!(
        mute::find_active(&pool, muter.id, new.id)
            .await
            .unwrap()
            .is_some_and(|m| m.hide_notifications),
        "the mute (and its notification setting) carries over"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_is_idempotent(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    let bob_new = aliased("new.example", "bob", &bob_old.actor.id);
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();

    for _ in 0..2 {
        let status = deliver_move(&pool, &stub, &bob_old, &bob_new.actor.id).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    // Exactly one follow edge to the new account, none to the old.
    let new = account::find_by_uri(&pool, &bob_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, alice.id, new.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_none()
    );
}

/// An actor document carries `movedTo` as well as the `Move` activity, and any
/// refresh of the source actor — a signature verification is enough — records
/// the redirect first. That must not make the real activity look like a replay:
/// the redirect is a fact about the account, the follower migration is the work.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_still_repoints_after_the_actor_advertised_the_redirect(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob_old = RemoteUser::new("remote.example", "bob");
    let bob_new = aliased("new.example", "bob", &bob_old.actor.id);
    let stub = StubFederation::with_actors([bob_old.actor.clone(), bob_new.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();
    // The actor refresh that beat the activity to it.
    account::set_moved_to(&pool, old.id, Some(&bob_new.actor.id))
        .await
        .unwrap();

    let status = deliver_move(&pool, &stub, &bob_old, &bob_new.actor.id).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let new = account::find_by_uri(&pool, &bob_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, alice.id, new.id)
            .await
            .unwrap()
            .is_some(),
        "the follower is moved even though the redirect was already known"
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_none(),
        "the old follow is dropped"
    );
}

/// Somebody moving *onto* this server: the destination is a local account, so
/// the followers here belong on that row — not on a remote duplicate of an
/// account we host ourselves.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_move_onto_this_server_repoints_to_the_local_account(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let carol_uri = format!("https://{TEST_DOMAIN}/users/carol");
    let bob_old = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob_old.actor.clone()]);

    let old = store_remote_actor(&pool, &bob_old.actor).await.unwrap();
    follow::create(&pool, alice.id, old.id, Some("https://remote.example/f/1"))
        .await
        .unwrap();

    // Without carol claiming bob as an alias, the move is refused — the same
    // anti-hijack rule, read from where a local account keeps its aliases.
    assert_eq!(
        deliver_move(&pool, &stub, &bob_old, &carol_uri).await,
        StatusCode::ACCEPTED
    );
    assert!(
        follow::find(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_none(),
        "a move onto an account that has not claimed the origin is ignored"
    );

    account_alias::add(&pool, carol.id, "bob@remote.example", &bob_old.actor.id)
        .await
        .unwrap();
    assert_eq!(
        deliver_move(&pool, &stub, &bob_old, &carol_uri).await,
        StatusCode::ACCEPTED
    );

    assert!(
        follow::find(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_some(),
        "the follow re-points at the local destination account"
    );
    assert!(
        follow::find(&pool, alice.id, old.id)
            .await
            .unwrap()
            .is_none(),
        "the old follow is dropped"
    );
    assert!(
        account::find_by_uri(&pool, &carol_uri)
            .await
            .unwrap()
            .is_none(),
        "no remote duplicate of a local account is stored"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_migrate_repoints_followers_and_publishes_redirect(pool: PgPool) {
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let dave_uri = format!("https://{TEST_DOMAIN}/users/dave");
    let dave_new = aliased("new.example", "dave", &dave_uri);
    let stub = StubFederation::with_users(&[&dave_new]);

    // A remote follower (federated) and a local follower.
    let eve = RemoteUser::new("remote.example", "eve");
    let eve_acct = store_remote_actor(&pool, &eve.actor).await.unwrap();
    follow::create(
        &pool,
        eve_acct.id,
        dave.id,
        Some("https://remote.example/f/eve"),
    )
    .await
    .unwrap();
    let frank = create_local_account(&pool, "frank", "Frank").await;
    follow::create(&pool, frank.id, dave.id, None)
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), stub.clone());
    let outcome = migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap();
    assert_eq!(outcome.target_uri, dave_new.actor.id);
    // Telling the remote followers is synchronous; re-pointing the local ones
    // is the queued replay, for the same reason it is on the inbound side.
    migration::run_due(&state).await;

    // The redirect is recorded and published on the actor document.
    let dave_after = account::find_by_id(&pool, dave.id).await.unwrap().unwrap();
    assert_eq!(
        dave_after.moved_to_uri.as_deref(),
        Some(dave_new.actor.id.as_str())
    );
    let doc = get_actor(test_app_with(pool.clone(), stub.clone()), "/users/dave").await;
    assert_eq!(doc["movedTo"], dave_new.actor.id);

    // The local follower is re-pointed; the remote follower is told.
    let new = account::find_by_uri(&pool, &dave_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        follow::find(&pool, frank.id, new.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        follow::find(&pool, frank.id, dave.id)
            .await
            .unwrap()
            .is_none()
    );

    delivery::run_due(&state).await;
    let eve_inbox = eve.actor.shared_inbox_or_empty();
    let eve_types: Vec<String> = stub
        .deliveries()
        .iter()
        .filter(|d| d.inbox_url == eve_inbox)
        .map(|d| d.activity["type"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(eve_types.contains(&"Update".to_owned()), "{eve_types:?}");
    assert!(eve_types.contains(&"Move".to_owned()), "{eve_types:?}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_migrate_rejects_target_without_alias(pool: PgPool) {
    create_local_account(&pool, "dave", "Dave").await;
    // dave_new does not list dave as an alias.
    let dave_new = RemoteUser::new("new.example", "dave");
    let stub = StubFederation::with_users(&[&dave_new]);

    let state = test_state_with(pool.clone(), stub);
    // The refusal is typed now; the API/CLI rendering of it is unchanged.
    let err = migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            plamenu::migration::Failure::Invalid(plamenu::migration::Invalid::NotAnAlias)
        ),
        "{err:?}"
    );
    let dave = account::find_local_by_username(&pool, "dave")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dave.moved_to_uri, None);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn alias_is_declared_and_published_on_the_actor(pool: PgPool) {
    create_local_account(&pool, "dave", "Dave").await;
    let other = RemoteUser::new("new.example", "dave");
    let stub = StubFederation::with_users(&[&other]);
    let state = test_state_with(pool.clone(), stub.clone());

    let uri = migration::add_local_alias(&state, "dave", "dave@new.example")
        .await
        .unwrap();
    assert_eq!(uri, other.actor.id);
    assert_eq!(
        migration::list_local_aliases(&state, "dave").await.unwrap(),
        vec![other.actor.id.clone()]
    );

    let doc = get_actor(test_app_with(pool.clone(), stub), "/users/dave").await;
    assert_eq!(doc["alsoKnownAs"], json!([other.actor.id]));

    // Removal clears it again.
    assert!(
        migration::remove_local_alias(&state, "dave", &other.actor.id)
            .await
            .unwrap()
    );
    assert!(
        migration::list_local_aliases(&state, "dave")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn alias_rows_keep_the_typed_acct_for_display(pool: PgPool) {
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let other = RemoteUser::new("new.example", "dave");
    let stub = StubFederation::with_users(&[&other]);
    let state = test_state_with(pool.clone(), stub);

    migration::add_local_alias(&state, "dave", "@dave@new.example")
        .await
        .unwrap();
    // Idempotent — a re-add keeps one row.
    migration::add_local_alias(&state, "dave", "dave@new.example")
        .await
        .unwrap();

    let rows = account_alias::list(&pool, dave.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].acct, "dave@new.example", "stored without the @");
    assert_eq!(rows[0].uri, other.actor.id);

    migration::remove_local_alias(&state, "dave", &other.actor.id)
        .await
        .unwrap();
    assert!(
        account_alias::list(&pool, dave.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_migrate_records_history_and_enforces_cooldown(pool: PgPool) {
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let dave_uri = format!("https://{TEST_DOMAIN}/users/dave");
    let dave_new = aliased("new.example", "dave", &dave_uri);
    let stub = StubFederation::with_users(&[&dave_new]);

    let eve = RemoteUser::new("remote.example", "eve");
    let eve_acct = store_remote_actor(&pool, &eve.actor).await.unwrap();
    follow::create(
        &pool,
        eve_acct.id,
        dave.id,
        Some("https://remote.example/f/eve"),
    )
    .await
    .unwrap();

    let state = test_state_with(pool.clone(), stub);
    migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap();

    let history = account_migration::list(&pool, dave.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].target_acct, "dave@new.example");
    assert_eq!(history[0].followers_count, 1);
    let new = account::find_by_uri(&pool, &dave_new.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history[0].target_account_id, Some(new.id));

    // A second move inside the 30-day window is refused up front.
    let err = migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap_err();
    match plamenu::error::ApiError::from(err) {
        plamenu::error::ApiError::Unprocessable(message) => {
            assert!(message.contains("30 days"), "{message}");
        }
        other => panic!("expected Unprocessable, got {other:?}"),
    }
    assert_eq!(
        account_migration::list(&pool, dave.id).await.unwrap().len(),
        1
    );
}

// ---- Web settings pages ------------------------------------------------

struct Page {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

async fn send_page(app: &Router, request: Request<Body>) -> Page {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Page {
        status,
        location,
        // Localized copy carries Fluent's directional isolates around
        // interpolated values; strip them so assertions read as the page does.
        body: String::from_utf8_lossy(&bytes).replace(['\u{2068}', '\u{2069}'], ""),
    }
}

/// A Russian-preferring GET; these accounts store no interface locale, so the
/// header decides.
async fn get_page_in_russian(app: &Router, uri: &str, cookie: &str) -> Page {
    let request = Request::builder()
        .uri(uri)
        .header(axum::http::header::COOKIE, cookie)
        .header(axum::http::header::ACCEPT_LANGUAGE, "ru")
        .body(Body::empty())
        .unwrap();
    send_page(app, request).await
}

/// A Russian-preferring form POST.
async fn post_form_in_russian(
    app: &Router,
    uri: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(axum::http::header::COOKIE, cookie)
        .header(axum::http::header::ACCEPT_LANGUAGE, "ru")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    send_page(app, request).await
}

async fn get_page(app: &Router, uri: &str, cookie: &str) -> Page {
    let request = Request::builder()
        .uri(uri)
        .header(axum::http::header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    send_page(app, request).await
}

async fn post_form(app: &Router, uri: &str, cookie: &str, fields: &[(&str, &str)]) -> Page {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(axum::http::header::COOKIE, cookie)
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    send_page(app, request).await
}

fn csrf_of(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

/// A local account with a working web login; returns the session cookie.
async fn web_login(app: &Router, pool: &PgPool, username: &str, password: &str) -> String {
    let account = create_local_account(pool, username, username).await;
    user::create(pool, account.id, None, &hash_password(password).unwrap())
        .await
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Body::from(
            serde_urlencoded::to_string([("email", username), ("password", password)]).unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_aliases_page_adds_and_removes(pool: PgPool) {
    let other = RemoteUser::new("new.example", "dave");
    let stub = StubFederation::with_users(&[&other]);
    let app = test_app_with(pool.clone(), stub);
    let cookie = web_login(&app, &pool, "dave", "correct horse battery").await;

    let page = get_page(&app, "/settings/aliases", &cookie).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("You have no declared aliases"));

    let csrf = csrf_of(&page.body);
    let response = post_form(
        &app,
        "/web/settings/aliases",
        &cookie,
        &[("csrf", &csrf), ("acct", "dave@new.example")],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location.as_deref(),
        Some("/settings/aliases?saved=1")
    );

    let page = get_page(&app, "/settings/aliases", &cookie).await;
    assert!(page.body.contains("dave@new.example"), "{}", page.body);

    let response = post_form(
        &app,
        "/web/settings/aliases/delete",
        &cookie,
        &[("csrf", &csrf), ("uri", &other.actor.id)],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let page = get_page(&app, "/settings/aliases", &cookie).await;
    assert!(page.body.contains("You have no declared aliases"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_migration_page_moves_the_account(pool: PgPool) {
    let dave_uri = format!("https://{TEST_DOMAIN}/users/dave");
    let dave_new = aliased("new.example", "dave", &dave_uri);
    let stub = StubFederation::with_users(&[&dave_new]);
    let app = test_app_with(pool.clone(), stub);
    let cookie = web_login(&app, &pool, "dave", "correct horse battery").await;

    let page = get_page(&app, "/settings/migration", &cookie).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Move to a different account"));
    let csrf = csrf_of(&page.body);

    // Wrong password: refused, nothing recorded.
    let response = post_form(
        &app,
        "/web/settings/migration",
        &cookie,
        &[
            ("csrf", &csrf),
            ("acct", "dave@new.example"),
            ("current_password", "wrong"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(
        response
            .location
            .as_deref()
            .unwrap_or_default()
            .contains("error="),
        "{:?}",
        response.location
    );

    let response = post_form(
        &app,
        "/web/settings/migration",
        &cookie,
        &[
            ("csrf", &csrf),
            ("acct", "dave@new.example"),
            ("current_password", "correct horse battery"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location.as_deref(),
        Some("/settings/migration?saved=1")
    );

    let dave = account::find_local_by_username(&pool, "dave")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dave.moved_to_uri.as_deref(),
        Some(dave_new.actor.id.as_str())
    );

    // The page now shows the redirect, the history row and the cooldown.
    let page = get_page(&app, "/settings/migration", &cookie).await;
    assert!(page.body.contains(&dave_new.actor.id));
    assert!(page.body.contains("dave@new.example"));
    assert!(
        page.body.contains("next move is possible after"),
        "{}",
        page.body
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_migration_pages_negotiate_russian_including_refusals(pool: PgPool) {
    // The remote account exists but does not claim dave as an alias, so the
    // move is refused — the refusal the reader sees comes from the catalog.
    let other = RemoteUser::new("new.example", "dave");
    let stub = StubFederation::with_users(&[&other]);
    let app = test_app_with(pool.clone(), stub);
    let cookie = web_login(&app, &pool, "dave", "correct horse battery").await;

    let aliases = get_page_in_russian(&app, "/settings/aliases", &cookie).await;
    assert!(aliases.body.contains("Псевдонимы"));
    assert!(aliases.body.contains("У вас нет объявленных псевдонимов."));
    let csrf = csrf_of(&aliases.body);

    let migration = get_page_in_russian(&app, "/settings/migration", &cookie).await;
    assert!(migration.body.contains("Переезд аккаунта"));
    assert!(migration.body.contains("Перенести подписчиков"));

    // A wrong password, and then a target that does not claim this account.
    let wrong = post_form_in_russian(
        &app,
        "/web/settings/migration",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("acct", "dave@new.example"),
            ("current_password", "not the password"),
        ],
    )
    .await;
    let location = wrong.location.unwrap();
    assert!(
        location.contains(&urlencoding_of("Неверный пароль")),
        "location {location}"
    );

    let refused = post_form_in_russian(
        &app,
        "/web/settings/migration",
        &cookie,
        &[
            ("csrf", csrf.as_str()),
            ("acct", "dave@new.example"),
            ("current_password", "correct horse battery"),
        ],
    )
    .await;
    let location = refused.location.unwrap();
    assert!(
        location.contains(&urlencoding_of("Целевой аккаунт не указывает")),
        "location {location}"
    );
}

/// The form-encoded rendering of an expected error string, as it appears in the
/// redirect the settings pages use to carry a message back.
fn urlencoding_of(text: &str) -> String {
    serde_urlencoded::to_string([("error", text)])
        .unwrap()
        .trim_start_matches("error=")
        .to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_move_rolls_back_redirect_history_and_replay_when_delivery_fails(pool: PgPool) {
    let dave = create_local_account(&pool, "dave", "Dave").await;
    let dave_new = aliased(
        "new.example",
        "dave",
        &format!("https://{TEST_DOMAIN}/users/dave"),
    );
    let eve = RemoteUser::new("remote.example", "eve");
    let follower = store_remote_actor(&pool, &eve.actor).await.unwrap();
    follow::create(&pool, follower.id, dave.id, None)
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[&dave_new]));
    sqlx::query("CREATE FUNCTION reject_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END; $$ LANGUAGE plpgsql").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_delivery()").execute(&pool).await.unwrap();
    let error = migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert!(
        account::find_by_id(&pool, dave.id)
            .await
            .unwrap()
            .unwrap()
            .moved_to_uri
            .is_none()
    );
    assert!(
        account_migration::latest_at(&pool, dave.id)
            .await
            .unwrap()
            .is_none()
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM account_move_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0);
    sqlx::query("DROP TRIGGER reject_delivery ON delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    migration::migrate_local_account(&state, "dave", "dave@new.example")
        .await
        .unwrap();
    assert_eq!(
        account::find_by_id(&pool, dave.id)
            .await
            .unwrap()
            .unwrap()
            .moved_to_uri
            .as_deref(),
        Some(dave_new.actor.id.as_str())
    );
    assert!(plamenu_db::job::pending_count(&pool).await.unwrap() > 0);
}
