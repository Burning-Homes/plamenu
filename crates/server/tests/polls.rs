//! Poll integration tests: inbound `Question` ingestion, the polls API,
//! voting (local and federated) and outbound `Question` authoring.

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
use plamenu::auth::hash_password;
use plamenu::delivery;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, account, follow, poll, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

const ALICE_URI: &str = "https://plamenu.test/users/alice";
const QUESTION_URI: &str = "https://remote.example/users/bob/statuses/77";

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
            "client_name": "polls",
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
        None,
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", "read write"),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await
    .1;
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

/// Form-encoded POST; returns (status, body text).
async fn post_form(
    app: Router,
    uri: &str,
    bearer: Option<&str>,
    fields: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
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

/// A Mastodon-shaped `Question` object by bob, with the given tallies.
fn question_object(bob: &RemoteUser, tallies: [i64; 2], end_time: &str) -> Value {
    json!({
        "id": QUESTION_URI,
        "type": "Question",
        "attributedTo": bob.actor.id,
        "content": "<p>pick one</p>",
        "published": "2026-06-10T12:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{}/followers", bob.actor.id)],
        "oneOf": [
            {"type": "Note", "name": "yes",
             "replies": {"type": "Collection", "totalItems": tallies[0]}},
            {"type": "Note", "name": "no",
             "replies": {"type": "Collection", "totalItems": tallies[1]}},
        ],
        "endTime": end_time,
        "votersCount": tallies[0] + tallies[1],
    })
}

fn create_activity(bob: &RemoteUser, object: &Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{QUESTION_URI}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": object,
    })
}

/// Delivers bob's Question and returns the stored status id.
async fn ingest_question(pool: &PgPool, stub: &Arc<StubFederation>, bob: &RemoteUser) -> i64 {
    ingest_question_ending(pool, stub, bob, "2027-01-01T00:00:00Z").await
}

async fn ingest_question_ending(
    pool: &PgPool,
    stub: &Arc<StubFederation>,
    bob: &RemoteUser,
    end_time: &str,
) -> i64 {
    let activity = create_activity(bob, &question_object(bob, [3, 4], end_time));
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &activity,
        &bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    status::find_by_uri(pool, QUESTION_URI)
        .await
        .unwrap()
        .expect("question stored")
        .id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_question_creates_poll_and_renders_entity(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status_id = ingest_question(&pool, &stub, &bob).await;

    // Anonymous view: poll entity without viewer flags.
    let (code, entity) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let rendered = &entity["poll"];
    assert_eq!(rendered["multiple"], false);
    assert_eq!(rendered["expired"], false);
    assert_eq!(rendered["votes_count"], 7);
    assert_eq!(rendered["voters_count"], 7);
    assert_eq!(
        rendered["options"],
        json!([
            {"title": "yes", "votes_count": 3},
            {"title": "no", "votes_count": 4},
        ])
    );
    assert!(rendered.get("voted").is_none());
    assert!(rendered.get("own_votes").is_none());

    // The poll endpoint serves it too; authenticated viewers get flags.
    let poll_id = rendered["id"].as_str().unwrap().to_owned();
    let (code, fetched) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/polls/{poll_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(fetched["voted"], false);
    assert_eq!(fetched["own_votes"], json!([]));

    // Re-delivery does not duplicate the poll.
    ingest_question(&pool, &stub, &bob).await;
    assert_eq!(
        poll::find_by_status(&pool, status_id)
            .await
            .unwrap()
            .unwrap()
            .id
            .to_string(),
        poll_id
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_question_refreshes_tallies_without_an_edit(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status_id = ingest_question(&pool, &stub, &bob).await;

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{QUESTION_URI}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": question_object(&bob, [10, 5], "2027-01-01T00:00:00Z"),
    });
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &update,
        &bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    let refreshed = poll::find_by_status(&pool, status_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.cached_tallies, [10, 5]);
    assert_eq!(refreshed.voters_count, Some(15));
    // A tally refresh is not an edit.
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert!(stored.edited_at.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn voting_on_a_remote_poll_federates_the_votes(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status_id = ingest_question(&pool, &stub, &bob).await;
    let stored_poll = poll::find_by_status(&pool, status_id)
        .await
        .unwrap()
        .unwrap();

    let (code, voted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/polls/{}/votes", stored_poll.id),
        Some(&token),
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(voted["voted"], true);
    assert_eq!(voted["own_votes"], json!([0]));
    // The origin tallies the vote; our cached counts are unchanged.
    assert_eq!(voted["votes_count"], 7);

    // The status entity uses the batched poll renderer (the web UI's poll
    // fragment goes through the same path). It must retain the local viewer's
    // choice even though the remote origin remains authoritative for tallies.
    let (code, status_entity) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(status_entity["poll"]["voted"], true);
    assert_eq!(status_entity["poll"]["own_votes"], json!([0]));

    // The vote went out as Mastodon's Create(Note) shape.
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    let activity = &sent[0].activity;
    assert_eq!(activity["type"], "Create");
    assert_eq!(activity["to"], bob.actor.id);
    let object = &activity["object"];
    assert_eq!(object["type"], "Note");
    assert_eq!(object["name"], "yes");
    assert_eq!(object["inReplyTo"], QUESTION_URI);
    assert_eq!(object["attributedTo"], ALICE_URI);
    assert!(object.get("content").is_none());

    // Voting twice is rejected.
    let (code, error) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/polls/{}/votes", stored_poll.id),
        Some(&token),
        Some(json!({"choices": ["1"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "You have already voted on this poll");
    assert_eq!(
        poll::votes_by(&pool, stored_poll.id, alice.id)
            .await
            .unwrap(),
        [0]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn vote_validation_mirrors_mastodon(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    // The poll ended in the past.
    let status_id = ingest_question_ending(&pool, &stub, &bob, "2026-01-01T00:00:00Z").await;
    let stored_poll = poll::find_by_status(&pool, status_id)
        .await
        .unwrap()
        .unwrap();
    let votes_path = format!("/api/v1/polls/{}/votes", stored_poll.id);

    let (code, error) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&token),
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "The poll has already ended");

    // Missing choices.
    let (code, error) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&token),
        Some(json!({"choices": []})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"], "Param choices is missing");

    // An anonymous vote needs a token at all.
    let (code, _) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        None,
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn posting_a_poll_federates_a_question(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();

    let (code, entity) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "which one?",
            "poll": {"options": ["yes", "no"], "expires_in": 3600, "multiple": true},
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let rendered = &entity["poll"];
    assert_eq!(rendered["multiple"], true);
    assert_eq!(rendered["votes_count"], 0);
    assert_eq!(rendered["voters_count"], 0);
    assert_eq!(rendered["voted"], true, "authors count as having voted");
    assert_eq!(
        rendered["options"][0],
        json!({"title": "yes", "votes_count": 0})
    );
    assert!(rendered["expires_at"].is_string());

    // The follower receives a Create(Question).
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 1);
    let sent = stub.deliveries();
    let object = &sent[0].activity["object"];
    assert_eq!(object["type"], "Question");
    assert_eq!(object["anyOf"][0]["name"], "yes");
    assert_eq!(object["anyOf"][1]["replies"]["totalItems"], 0);
    assert!(object["endTime"].is_string());
    assert_eq!(object["votersCount"], 0);

    // The AP object endpoint serves the same Question.
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/users/alice/statuses/{}",
            entity["id"].as_str().unwrap()
        ))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let served: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(served["type"], "Question");
    assert_eq!(served["anyOf"][0]["name"], "yes");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn poll_creation_validation_mirrors_mastodon(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let cases = [
        (
            json!({"options": ["solo"], "expires_in": 3600}),
            "Validation failed: Options must have more than one item",
        ),
        (
            json!({"options": ["a", "b", "c", "d", "e", "f", "g"], "expires_in": 3600}),
            "Validation failed: Options can't contain more than 6 items",
        ),
        (
            json!({"options": ["x".repeat(51), "b".to_owned()], "expires_in": 3600}),
            "Validation failed: Options cannot be longer than 50 characters each",
        ),
        (
            json!({"options": ["same", "same"], "expires_in": 3600}),
            "Validation failed: Options contain duplicate items",
        ),
        (
            json!({"options": ["a", "b"]}),
            "Validation failed: Expires at can't be blank",
        ),
        (
            json!({"options": ["a", "b"], "expires_in": 60}),
            "Validation failed: Expires at is too soon",
        ),
        (
            json!({"options": ["a", "b"], "expires_in": 100_000_000}),
            "Validation failed: Expires at is too far into the future",
        ),
    ];
    for (poll_body, message) in cases {
        let (code, error) = api(
            test_app(pool.clone()),
            "POST",
            "/api/v1/statuses",
            Some(&token),
            Some(json!({"status": "pick", "poll": poll_body})),
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{message}");
        assert_eq!(error["error"], message);
    }
    // No stray statuses were created by failed validations.
    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status::count_by_account(&pool, alice.id).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn form_encoded_poll_params_are_recovered(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let (code, body) = post_form(
        test_app(pool.clone()),
        "/api/v1/statuses",
        Some(&token),
        &[
            ("status", "form poll"),
            ("poll[options][]", "yes"),
            ("poll[options][]", "no"),
            ("poll[expires_in]", "3600"),
            ("poll[hide_totals]", "true"),
        ],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let entity: Value = serde_json::from_str(&body).unwrap();
    let rendered = &entity["poll"];
    assert_eq!(rendered["options"][0]["title"], "yes");
    // hide_totals polls hide per-option counts while running.
    assert_eq!(rendered["options"][0]["votes_count"], Value::Null);
    assert_eq!(rendered["multiple"], false);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn own_poll_votes_are_rejected_and_local_votes_tally(pool: PgPool) {
    let (_, alice_token) = user_with_token(&pool, "alice").await;
    let (_, carol_token) = user_with_token(&pool, "carol").await;

    let (_, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({
            "status": "which?",
            "poll": {"options": ["yes", "no"], "expires_in": 3600},
        })),
    )
    .await;
    let poll_id = entity["poll"]["id"].as_str().unwrap().to_owned();
    let votes_path = format!("/api/v1/polls/{poll_id}/votes");

    let (code, error) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&alice_token),
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "You cannot vote in your own polls");

    // Out-of-range choices are invalid.
    let (code, error) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&carol_token),
        Some(json!({"choices": ["5"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "The chosen vote option does not exist");

    // Two choices on a single-choice poll are one too many.
    let (code, error) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&carol_token),
        Some(json!({"choices": ["0", "1"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "You have already voted on this poll");

    // A valid local vote tallies immediately.
    let (code, voted) = api(
        test_app(pool.clone()),
        "POST",
        &votes_path,
        Some(&carol_token),
        Some(json!({"choices": [1]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(voted["voted"], true);
    assert_eq!(voted["own_votes"], json!([1]));
    assert_eq!(voted["votes_count"], 1);
    assert_eq!(voted["voters_count"], 1);
    assert_eq!(voted["options"][1]["votes_count"], 1);
}

/// Bob's federated vote on alice's poll, shaped like Mastodon sends it.
fn vote_activity(bob: &RemoteUser, status_id: &str, name: &str, marker: u32) -> Value {
    let alice_status = format!("{ALICE_URI}/statuses/{status_id}");
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#votes/{marker}/activity", bob.actor.id),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ALICE_URI,
        "object": {
            "id": format!("{}#votes/{marker}", bob.actor.id),
            "type": "Note",
            "name": name,
            "attributedTo": bob.actor.id,
            "inReplyTo": alice_status,
            "to": ALICE_URI,
        },
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_votes_tally_and_distribute_an_update(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("elsewhere.example", "carol");
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);
    for user in [&bob, &carol] {
        let remote = plamenu::remote::store_remote_actor(&pool, &user.actor)
            .await
            .unwrap();
        follow::create(&pool, remote.id, alice.id, None)
            .await
            .unwrap();
    }

    let (_, entity) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "which?",
            "poll": {"options": ["yes", "no"], "expires_in": 3600},
        })),
    )
    .await;
    let status_id = entity["id"].as_str().unwrap().to_owned();
    let poll_id = entity["poll"]["id"].as_str().unwrap().to_owned();
    // Flush the Create deliveries so later assertions see only the Update.
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 2);

    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &vote_activity(&bob, &status_id, "yes", 1),
        &bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    // The vote tallied without creating a reply status.
    let (_, fetched) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/polls/{poll_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(fetched["votes_count"], 1);
    assert_eq!(fetched["voters_count"], 1);
    assert_eq!(fetched["options"][0]["votes_count"], 1);
    assert!(
        status::find_by_uri(&pool, &format!("{}#votes/1", bob.actor.id))
            .await
            .unwrap()
            .is_none(),
        "votes are not statuses"
    );

    // Both followers got an Update(Question) with the new tallies.
    assert_eq!(delivery::run_due(&state).await, 2);
    let updates: Vec<_> = stub
        .deliveries()
        .into_iter()
        .filter(|d| d.activity["type"] == "Update")
        .collect();
    assert_eq!(updates.len(), 2);
    let object = &updates[0].activity["object"];
    assert_eq!(object["type"], "Question");
    assert_eq!(object["oneOf"][0]["replies"]["totalItems"], 1);
    assert_eq!(object["votersCount"], 1);

    // Redelivery of the same vote changes nothing.
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &vote_activity(&bob, &status_id, "yes", 1),
        &bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let stored_poll = poll::find_by_id(&pool, poll_id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored_poll.cached_tallies, [1, 0]);
}

// ---------------------------------------------------------------------------
// Poll expiry

/// Time-travels a poll past its deadline through the public update path
/// (only `expires_at` changes; tallies and votes stay).
async fn force_expired(pool: &PgPool, poll_id: i64) {
    let row = poll::find_by_id(pool, poll_id).await.unwrap().unwrap();
    poll::apply_remote_update(
        pool,
        poll_id,
        poll::RemotePollUpdate {
            options: &row.options,
            cached_tallies: &row.cached_tallies,
            multiple: row.multiple,
            voters_count: row.voters_count,
            expires_at: Some(time::OffsetDateTime::now_utc() - time::Duration::minutes(5)),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_poll_expiry_notifies_and_sends_the_final_update(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();

    let (code, entity) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({
            "status": "closing soon",
            "poll": {"options": ["yes", "no"], "expires_in": 300},
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let poll_id: i64 = entity["poll"]["id"].as_str().unwrap().parse().unwrap();
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&carol_token),
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Flush the Create and the vote-driven Update to the remote follower.
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 2);

    // Nothing is due while the poll runs.
    assert_eq!(plamenu::poll_expiry::run_due(&state).await, 0);

    force_expired(&pool, poll_id).await;
    assert_eq!(plamenu::poll_expiry::run_due(&state).await, 1);
    // Each poll is processed exactly once.
    assert_eq!(plamenu::poll_expiry::run_due(&state).await, 0);

    // The follower receives the final tallies, now marked closed.
    assert_eq!(delivery::run_due(&state).await, 1);
    let closed: Vec<_> = stub
        .deliveries()
        .into_iter()
        .filter(|d| d.activity["object"]["closed"].is_string())
        .collect();
    assert_eq!(closed.len(), 1);
    let object = &closed[0].activity["object"];
    assert_eq!(object["type"], "Question");
    assert_eq!(object["oneOf"][0]["replies"]["totalItems"], 1);

    // The author hears their own poll ended; so does the local voter.
    for (token, who) in [(&alice_token, "author"), (&carol_token, "voter")] {
        let (_, items) = api(
            test_app(pool.clone()),
            "GET",
            "/api/v1/notifications?types%5B%5D=poll",
            Some(token),
            None,
        )
        .await;
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1, "{who} gets exactly one poll notification");
        assert_eq!(items[0]["type"], "poll", "{who}");
        assert_eq!(items[0]["status"]["id"], entity["id"], "{who}");
        assert_eq!(items[0]["status"]["poll"]["expired"], true, "{who}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_poll_expiry_notifies_local_voters(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let status_id = ingest_question(&pool, &stub, &bob).await;
    let stored_poll = poll::find_by_status(&pool, status_id)
        .await
        .unwrap()
        .unwrap();

    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!("/api/v1/polls/{}/votes", stored_poll.id),
        Some(&alice_token),
        Some(json!({"choices": ["0"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 1, "the vote federates");

    // Bob's server closes the poll: an Update(Question) with a past end.
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{QUESTION_URI}#updates/2"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": question_object(&bob, [4, 4], "2026-01-01T00:00:00Z"),
    });
    let code = post_signed(
        test_app_with(pool.clone(), stub.clone()),
        "/inbox",
        &update,
        &bob.signer(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    assert_eq!(plamenu::poll_expiry::run_due(&state).await, 1);
    assert_eq!(plamenu::poll_expiry::run_due(&state).await, 0);
    // Remote polls never fan out tallies from here.
    assert_eq!(delivery::run_due(&state).await, 0);

    let (_, items) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=poll",
        Some(&alice_token),
        None,
    )
    .await;
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1, "the local voter is notified");
    assert_eq!(items[0]["account"]["acct"], "bob@remote.example");
    assert_eq!(items[0]["status"]["id"], json!(status_id.to_string()));
}
