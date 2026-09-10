//! Direct messages: `visibility=direct` gating, conversations, and the
//! mentioned-only federation audience (a DM must never reach followers).

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
use plamenu::auth::hash_password;
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, status, user};
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
    // Drive the real OAuth machinery once per user.
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "direct",
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

/// Posts a direct status and returns its entity.
async fn post_direct(app: Router, token: &str, text: &str, reply_to: Option<&str>) -> Value {
    let mut body = json!({"status": text, "visibility": "direct"});
    if let Some(parent) = reply_to {
        body["in_reply_to_id"] = json!(parent);
    }
    let (code, posted) = api(app, "POST", "/api/v1/statuses", Some(token), Some(body)).await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    posted
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_statuses_stay_between_participants(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    // Carol follows alice: she may see private posts, but never DMs.
    follow::create(&pool, carol.id, alice.id, None)
        .await
        .unwrap();
    let app = || test_app(pool.clone());

    let posted = post_direct(app(), &alice_token, "@bob secret plans", None).await;
    assert_eq!(posted["visibility"], "direct");
    assert_eq!(posted["mentions"][0]["username"], "bob");
    let status_id = posted["id"].as_str().unwrap().to_owned();
    let show = format!("/api/v1/statuses/{status_id}");

    // Author and the mentioned account see it; a follower and anonymous don't.
    let (code, _) = api(app(), "GET", &show, Some(&alice_token), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(app(), "GET", &show, Some(&bob_token), None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = api(app(), "GET", &show, Some(&carol_token), None).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "a follow must not expose DMs");
    let (code, _) = api(app(), "GET", &show, None, None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // DMs live in conversations, not in home timelines.
    for token in [&alice_token, &carol_token] {
        let (_, home) = api(app(), "GET", "/api/v1/timelines/home", Some(token), None).await;
        assert!(
            home.as_array()
                .unwrap()
                .iter()
                .all(|s| s["id"] != status_id.as_str()),
            "home timeline must not contain DMs: {home}"
        );
    }

    // Account statuses: the author and the mentioned account see it there,
    // the follower doesn't.
    let listing = format!("/api/v1/accounts/{}/statuses", alice.id);
    for (token, expected) in [(&alice_token, 1), (&bob_token, 1), (&carol_token, 0)] {
        let (_, statuses) = api(app(), "GET", &listing, Some(token), None).await;
        let found = statuses
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["id"] == status_id.as_str())
            .count();
        assert_eq!(found, expected, "viewer gating in account statuses");
    }

    // DMs cannot be boosted (Mastodon: 422) but can be favourited.
    let (code, body) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/reblog"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/favourite"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // The Note is not dereferenceable over ActivityPub.
    let request = Request::builder()
        .uri(format!("/users/alice/statuses/{status_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn conversations_track_direct_threads(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    let dm = post_direct(app(), &alice_token, "@bob lunch?", None).await;
    let dm_id = dm["id"].as_str().unwrap().to_owned();

    // The recipient's conversation is unread; the sender's is not.
    let (_, bob_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(bob_list.as_array().unwrap().len(), 1, "{bob_list}");
    assert_eq!(bob_list[0]["unread"], true);
    assert_eq!(bob_list[0]["accounts"][0]["acct"], "alice");
    assert_eq!(bob_list[0]["last_status"]["id"], dm_id.as_str());
    let (_, alice_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(alice_list.as_array().unwrap().len(), 1);
    assert_eq!(alice_list[0]["unread"], false);
    assert_eq!(alice_list[0]["accounts"][0]["acct"], "bob");

    // A direct reply lands in the same conversation and flips it unread for
    // the other side.
    let reply = post_direct(app(), &bob_token, "@alice sure", Some(&dm_id)).await;
    let reply_id = reply["id"].as_str().unwrap().to_owned();
    let (_, alice_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        alice_list.as_array().unwrap().len(),
        1,
        "same thread, one row"
    );
    assert_eq!(alice_list[0]["unread"], true);
    assert_eq!(alice_list[0]["last_status"]["id"], reply_id.as_str());

    // Mark it read; a foreign row id is not found.
    let alice_row = alice_list[0]["id"].as_str().unwrap().to_owned();
    let (code, marked) = api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{alice_row}/read"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(marked["unread"], false);
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{alice_row}/read"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "rows are owner-scoped");

    // Deleting the newest message rolls last_status back; deleting the rest
    // removes the conversation.
    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{reply_id}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, alice_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(alice_list[0]["last_status"]["id"], dm_id.as_str());
    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{dm_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    for token in [&alice_token, &bob_token] {
        let (_, list) = api(app(), "GET", "/api/v1/conversations", Some(token), None).await;
        assert_eq!(list, json!([]), "emptied conversations disappear");
    }

    // A self-DM falls back to showing the account itself.
    post_direct(app(), &alice_token, "note to self", None).await;
    let (_, alice_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(alice_list[0]["accounts"][0]["acct"], "alice");
}

/// Posts a status through the real compose endpoint; returns the entity.
async fn post_status(app: Router, token: &str, text: &str, reply_to: Option<&str>) -> Value {
    let mut body = json!({ "status": text, "visibility": "public" });
    if let Some(parent) = reply_to {
        body["in_reply_to_id"] = json!(parent);
    }
    let (code, posted) = api(app, "POST", "/api/v1/statuses", Some(token), Some(body)).await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    posted
}

async fn notification_count(app: Router, token: &str) -> usize {
    let (_, body) = api(app, "GET", "/api/v1/notifications", Some(token), None).await;
    body.as_array().unwrap().len()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn conversation_unread_toggle_and_delete(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    let dm = post_direct(app(), &alice_token, "@bob hi", None).await;
    let dm_id = dm["id"].as_str().unwrap().to_owned();
    // Bob's row, read it, then mark it unread again.
    let (_, list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&bob_token),
        None,
    )
    .await;
    let row = list[0]["id"].as_str().unwrap().to_owned();
    api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{row}/read"),
        Some(&bob_token),
        None,
    )
    .await;
    let (code, marked) = api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{row}/unread"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(marked["unread"], true);
    assert_eq!(marked["last_status"]["id"], dm_id.as_str());

    // Unread on a foreign row id is owner-scoped → 404.
    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{row}/unread"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Deleting drops only the caller's view; the other party keeps theirs.
    let (code, body) = api(
        app(),
        "DELETE",
        &format!("/api/v1/conversations/{row}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (_, bob_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(bob_list, json!([]), "bob's view is gone");
    let (_, alice_list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        alice_list.as_array().unwrap().len(),
        1,
        "alice still has hers"
    );

    // A second delete of the now-missing row is a 404.
    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/conversations/{row}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn muting_a_thread_suppresses_notifications_and_marks_muted(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    // Alice posts a public status; Bob replies mentioning her → one mention
    // notification. This is a normal (non-DM) thread.
    let root = post_status(app(), &alice_token, "thinking out loud", None).await;
    let root_id = root["id"].as_str().unwrap().to_owned();
    post_status(app(), &bob_token, "@alice good point", Some(&root_id)).await;
    assert_eq!(notification_count(app(), &alice_token).await, 1);

    // Alice mutes the conversation off the root status; it renders muted.
    let (code, muted) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{root_id}/mute"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{muted}");
    assert_eq!(muted["muted"], true);

    // A further reply in the muted thread raises no new notification.
    post_status(
        app(),
        &bob_token,
        "@alice and another thing",
        Some(&root_id),
    )
    .await;
    assert_eq!(
        notification_count(app(), &alice_token).await,
        1,
        "muted thread must not notify"
    );

    // The muted flag rides every fetch of the thread's statuses for her.
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["muted"], true);
    // Other viewers are unaffected — Bob sees it unmuted.
    let (_, bob_view) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{root_id}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(bob_view["muted"], false);

    // Unmuting clears the flag and restores notifications.
    let (code, unmuted) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{root_id}/unmute"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(unmuted["muted"], false);
    post_status(app(), &bob_token, "@alice back again", Some(&root_id)).await;
    assert_eq!(notification_count(app(), &alice_token).await, 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn conversations_paginate_by_last_status(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    // Two separate threads (a fresh DM never joins an existing thread).
    post_direct(app(), &alice_token, "@bob thread one", None).await;
    let newer = post_direct(app(), &alice_token, "@bob thread two", None).await;

    let request = Request::builder()
        .uri("/api/v1/conversations?limit=1")
        .header(header::AUTHORIZATION, format!("Bearer {bob_token}"))
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let link = response.headers()[header::LINK]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["last_status"]["id"], newer["id"]);
    let next_max = newer["id"].as_str().unwrap();
    assert!(
        link.contains(&format!("max_id={next_max}>; rel=\"next\"")),
        "{link}"
    );

    let (_, second_page) = api(
        app(),
        "GET",
        &format!("/api/v1/conversations?limit=1&max_id={next_max}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(second_page.as_array().unwrap().len(), 1);
    let older_content = second_page[0]["last_status"]["content"].as_str().unwrap();
    assert!(older_content.contains("thread one"), "{older_content}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_direct_notes_become_conversations(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (code, policy) = api(
        app(),
        "PATCH",
        "/api/v2/notifications/policy",
        Some(&alice_token),
        Some(json!({ "for_private_mentions": "filter" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(policy["for_private_mentions"], "filter");

    // A DM addressed to alice only: `to` carries her actor, no Public, no
    // followers collection.
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://plamenu.test/users/alice"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>psst, alice</p>",
            "published": "2026-06-11T00:00:00Z",
            "to": ["https://plamenu.test/users/alice"],
            "tag": [{
                "type": "Mention",
                "href": "https://plamenu.test/users/alice",
                "name": "@alice@plamenu.test",
            }],
        },
    });
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.visibility, "direct",
        "mentions-only addressing is a DM"
    );

    // Carol follows bob — the original leak: a followers-only
    // misclassification would show her the DM.
    follow::create(&pool, carol.id, stored.account_id, None)
        .await
        .unwrap();
    let show = format!("/api/v1/statuses/{}", stored.id);
    let (code, _) = api(app(), "GET", &show, Some(&carol_token), None).await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "the sender's followers must not see a DM"
    );
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(home, json!([]));

    // The DM is a mentions-only message from a non-followed sender. With
    // Alice's private-mention policy set to filter, the mention notification is
    // hidden from the default index, surfaced with `include_filtered`, and
    // rolled up into a notification request. The conversation row is withheld
    // until the sender's request is accepted.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications, json!([]), "the stranger DM is filtered out");
    let (_, filtered) = api(
        app(),
        "GET",
        "/api/v1/notifications?include_filtered=true",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(filtered[0]["type"], "mention");
    assert_eq!(filtered[0]["status"]["visibility"], "direct");
    assert_eq!(filtered[0]["filtered"], true);
    let (_, requests) = api(
        app(),
        "GET",
        "/api/v1/notifications/requests",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["account"]["acct"], "bob@remote.example");
    let request_id = requests[0]["id"].as_str().unwrap().to_owned();
    let (_, list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(list, json!([]), "filtered DMs stay out of conversations");

    let (code, _) = api(
        app(),
        "POST",
        &format!("/api/v1/notifications/requests/{request_id}/accept"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["unread"], true);
    assert_eq!(list[0]["accounts"][0]["acct"], "bob@remote.example");
    assert_eq!(list[0]["last_status"]["content"], "<p>psst, alice</p>");

    // Re-delivery does not duplicate the conversation or flip read state.
    let row_id = list[0]["id"].as_str().unwrap().to_owned();
    api(
        app(),
        "POST",
        &format!("/api/v1/conversations/{row_id}/read"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let (_, list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["unread"], false, "re-delivery is a no-op");

    // The sender deleting the DM clears it from the conversation.
    let delete = json!({
        "id": format!("{note_uri}#delete"),
        "type": "Delete",
        "actor": bob.actor.id,
        "object": { "id": note_uri, "type": "Tombstone" },
    });
    assert_eq!(
        post_signed(app(), &delete, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let (_, list) = api(
        app(),
        "GET",
        "/api/v1/conversations",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(list, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_direct_delivery_targets_mentions_only(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let eve = RemoteUser::new("remote.example", "eve");
    let stub = StubFederation::with_users(&[&bob, &eve]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    // Eve follows alice; a DM to bob must not reach her inbox.
    let stored_eve = remote::store_remote_actor(&pool, &eve.actor).await.unwrap();
    let alice_account = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, stored_eve.id, alice_account.id, None)
        .await
        .unwrap();

    let dm = post_direct(app(), &alice_token, "@bob@remote.example psst", None).await;
    let dm_id = dm["id"].as_str().unwrap().to_owned();
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1, "exactly one delivery: the mentioned actor");
    assert_eq!(sent[0].inbox_url, bob.actor.inbox);
    let activity = &sent[0].activity;
    assert_eq!(activity["type"], "Create");
    assert_eq!(activity["object"]["to"], json!([bob.actor.id]));
    assert_eq!(activity["object"]["cc"], json!([]));

    // Edits keep the same audience.
    let (code, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{dm_id}"),
        Some(&alice_token),
        Some(json!({"status": "@bob@remote.example psst (edited)"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].activity["type"], "Update");
    assert_eq!(sent[1].inbox_url, bob.actor.inbox);

    // And so does the Delete.
    let (code, _) = api(
        app(),
        "DELETE",
        &format!("/api/v1/statuses/{dm_id}"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2].activity["type"], "Delete");
    assert_eq!(sent[2].inbox_url, bob.actor.inbox);

    // Sanity: a public post does fan out to the follower.
    let (_, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "hello world"})),
    )
    .await;
    assert_eq!(posted["visibility"], "public");
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    assert!(
        sent.iter().any(|d| d.inbox_url
            == eve
                .actor
                .endpoints
                .as_ref()
                .unwrap()
                .shared_inbox
                .clone()
                .unwrap()
            || d.inbox_url == eve.actor.inbox),
        "followers receive public posts: {:?}",
        sent.iter().map(|d| d.inbox_url.clone()).collect::<Vec<_>>()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mention_less_direct_reply_federates_with_mention_tags(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let dm = post_direct(app(), &alice_token, "@bob@remote.example psst", None).await;
    let dm_id = dm["id"].as_str().unwrap().to_owned();
    delivery::run_due(&state).await;

    // The follow-up carries no typed @-mention: the audience is inherited
    // and the wire object must still tag the recipients — Mastodon
    // demotes an audience-only direct Note to unnotified `limited` and
    // GoToSocial hides it outright; both grant DM visibility from `tag` alone.
    let reply = post_direct(app(), &alice_token, "forgot to add: tuesday", Some(&dm_id)).await;
    let reply_id = reply["id"].as_str().unwrap().to_owned();
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let create = &sent.last().unwrap().activity;
    assert_eq!(create["type"], "Create");
    assert_eq!(create["object"]["to"], json!([bob.actor.id]));
    let tags = create["object"]["tag"].as_array().unwrap().clone();
    assert!(
        tags.iter().any(|t| t["type"] == "Mention"
            && t["href"] == json!(bob.actor.id)
            && t["name"] == json!("@bob@remote.example")),
        "inherited audience must ride `tag` as a Mention: {tags:?}"
    );

    // The second Note builder (edits/refetch) must agree with the Create.
    let (code, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{reply_id}"),
        Some(&alice_token),
        Some(json!({"status": "forgot to add: wednesday"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let update = &sent.last().unwrap().activity;
    assert_eq!(
        update["type"],
        "Update",
        "all deliveries: {:?}",
        sent.iter()
            .map(|d| (d.activity["type"].clone(), d.inbox_url.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(update["object"]["to"], json!([bob.actor.id]));
    let tags = update["object"]["tag"].as_array().unwrap().clone();
    assert!(
        tags.iter().any(|t| t["type"] == "Mention"
            && t["href"] == json!(bob.actor.id)
            && t["name"] == json!("@bob@remote.example")),
        "edited DM must keep its audience Mention tags: {tags:?}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_parent_forces_direct_reply(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let app = || test_app(pool.clone());

    // A "whisper": a direct reply under a public root. The root's visibility
    // no longer bounds the whisper's subtree — the direct parent does.
    let (_, root) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "public thread root"})),
    )
    .await;
    let root_id = root["id"].as_str().unwrap();
    let whisper = post_direct(app(), &alice_token, "@bob quietly though", Some(root_id)).await;
    let whisper_id = whisper["id"].as_str().unwrap();

    // Bob answers the whisper asking for public: clamped to direct (Akkoma
    // parity) so the answer can't leak the closed subthread into the open.
    let (code, reply) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&bob_token),
        Some(json!({
            "status": "sure thing",
            "in_reply_to_id": whisper_id,
            "visibility": "public",
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{reply}");
    assert_eq!(reply["visibility"], "direct");
}
