//! Status editing integration tests: `PUT /api/v1/statuses/{id}`, edit
//! history, `/source`, federated `Update(Note)` fan-out, and history for
//! inbound remote edits.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, id, job, media, status, user};
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
            "client_name": "edits",
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

async fn post_status(app: Router, token: &str, body: Value) -> Value {
    let (status, entity) = api(app, "POST", "/api/v1/statuses", Some(token), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    entity
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_updates_content_history_and_source(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let posted = post_status(
        app(),
        &token,
        json!({"status": "draft wording", "language": "en"}),
    )
    .await;
    let status_id = posted["id"].as_str().unwrap().to_owned();
    assert!(posted["edited_at"].is_null());

    // Unedited: history is the single current version (Mastodon synthesizes
    // it), readable anonymously for public posts.
    let (history_status, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(history_status, StatusCode::OK);
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["content"], posted["content"]);
    assert_eq!(history[0]["account"]["id"], alice.id.to_string());
    assert_eq!(history[0]["created_at"], posted["created_at"]);

    let (put_status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({"status": "final #wording", "spoiler_text": "cw"})),
    )
    .await;
    assert_eq!(put_status, StatusCode::OK, "{edited}");
    let content = edited["content"].as_str().unwrap();
    assert!(
        content.contains("final") && content.contains("/tags/wording"),
        "{content}"
    );
    assert!(!edited["edited_at"].is_null());
    assert_eq!(edited["spoiler_text"], "cw");
    assert_eq!(edited["sensitive"], true, "a CW forces sensitive on");
    assert_eq!(edited["language"], "en", "absent fields keep their value");

    // History now: the original first, the edit second.
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    let entries = history.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["content"], posted["content"]);
    assert_eq!(entries[0]["spoiler_text"], "");
    assert_eq!(entries[0]["created_at"], posted["created_at"]);
    assert_eq!(entries[1]["content"], edited["content"]);
    assert_eq!(entries[1]["spoiler_text"], "cw");
    assert_eq!(entries[1]["created_at"], edited["edited_at"]);

    // The source is the raw text, for the edit composer.
    let (source_status, source) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(source_status, StatusCode::OK);
    assert_eq!(source["id"], status_id);
    assert_eq!(source["text"], "final #wording");
    assert_eq!(source["spoiler_text"], "cw");

    // Source needs authentication.
    let (anon, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/source"),
        None,
        None,
    )
    .await;
    assert_eq!(anon, StatusCode::UNAUTHORIZED);

    // The AP object now carries `updated`.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/users/alice/statuses/{status_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let note: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(note["updated"], edited["edited_at"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_is_scoped_and_validated(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let posted = post_status(app(), &alice_token, json!({"status": "mine"})).await;
    let status_id = posted["id"].as_str().unwrap().to_owned();
    let put = |token: String, body: Value| {
        let uri = format!("/api/v1/statuses/{status_id}");
        let pool = pool.clone();
        async move { api(test_app(pool), "PUT", &uri, Some(&token), Some(body)).await }
    };

    // Someone else's status is a 404, like Mastodon.
    let (foreign, _) = put(carol_token, json!({"status": "hijack"})).await;
    assert_eq!(foreign, StatusCode::NOT_FOUND);

    let (blank, body) = put(alice_token.clone(), json!({"status": ""})).await;
    assert_eq!(blank, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Validation failed: Text can't be blank");

    let (bad_language, _) = put(
        alice_token.clone(),
        json!({"status": "x", "language": "nope!"}),
    )
    .await;
    assert_eq!(bad_language, StatusCode::UNPROCESSABLE_ENTITY);

    let (bad_media, body) = put(alice_token.clone(), json!({"media_ids": ["999999"]})).await;
    assert_eq!(bad_media, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Media 999999 not found or already attached to another post"
    );

    // A no-change submission is a no-op: 200, but no edit is recorded.
    let (unchanged, entity) = put(alice_token.clone(), json!({"status": "mine"})).await;
    assert_eq!(unchanged, StatusCode::OK);
    assert!(entity["edited_at"].is_null(), "{entity}");
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 1);

    // Boost wrappers cannot be edited.
    let (_, boost) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{status_id}/reblog"),
        Some(&alice_token),
        None,
    )
    .await;
    let boost_id = boost["id"].as_str().unwrap();
    let (status, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{boost_id}"),
        Some(&alice_token),
        Some(json!({"status": "edited boost"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Private statuses hide their history from outsiders.
    let private = post_status(
        app(),
        &alice_token,
        json!({"status": "secret", "visibility": "private"}),
    )
    .await;
    let private_id = private["id"].as_str().unwrap();
    let (anon, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(anon, StatusCode::NOT_FOUND);
    let (own, _) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{private_id}/history"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(own, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_federates_update_and_notifies_only_new_mentions(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    // Bob follows alice, so the edit must reach his inbox.
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, stored_bob.id, alice.id, None)
        .await
        .unwrap();
    let app = || test_app_with(pool.clone(), stub.clone());
    let state = test_state_with(pool.clone(), stub.clone());

    let posted = post_status(app(), &alice_token, json!({"status": "hi @carol"})).await;
    let status_id = posted["id"].as_str().unwrap().to_owned();
    assert_eq!(delivery::run_due(&state).await, 1, "the Create reaches bob");

    let (put_status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&alice_token),
        Some(json!({"status": "hi again @carol"})),
    )
    .await;
    assert_eq!(put_status, StatusCode::OK, "{edited}");
    assert_eq!(delivery::run_due(&state).await, 1, "the Update reaches bob");

    let deliveries = stub.deliveries();
    assert_eq!(deliveries.len(), 2);
    let update = &deliveries[1];
    assert_eq!(update.inbox_url, "https://remote.example/inbox");
    assert_eq!(update.activity["type"], "Update");
    assert_eq!(update.activity["object"]["type"], "Note");
    assert_eq!(update.activity["object"]["content"], edited["content"]);
    assert_eq!(update.activity["object"]["updated"], edited["edited_at"]);

    // Carol was mentioned in both versions: one notification, not two.
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&carol_token),
        None,
    )
    .await;
    let mentions: Vec<&Value> = notifications
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "mention")
        .collect();
    assert_eq!(mentions.len(), 1, "{notifications}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_reconciles_media_attachments(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());
    let upload = |name: &'static str| {
        let pool = pool.clone();
        let account_id = alice.id;
        async move {
            media::create_local(
                &pool,
                media::NewLocalMedia {
                    width: Some(10),
                    height: Some(10),
                    ..media::NewLocalMedia::new(account_id, id::next(), name, "image/jpeg")
                },
            )
            .await
            .unwrap()
            .id
        }
    };
    let first = upload("first.jpg").await;
    let second = upload("second.jpg").await;

    let posted = post_status(
        app(),
        &token,
        json!({"status": "pics", "media_ids": [first.to_string()]}),
    )
    .await;
    let status_id = posted["id"].as_str().unwrap().to_owned();

    // Swap the attachment for another one.
    let (put_status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({"media_ids": [second.to_string()]})),
    )
    .await;
    assert_eq!(put_status, StatusCode::OK, "{edited}");
    let attachments = edited["media_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["id"], second.to_string());
    assert_eq!(edited["content"], posted["content"], "text untouched");

    // The removed upload is unattached again, not deleted.
    let detached = media::find_owned(&pool, first, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detached.status_id, None);

    // Each history version shows its own attachments.
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    let entries = history.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["media_attachments"][0]["id"], first.to_string());
    assert_eq!(entries[1]["media_attachments"][0]["id"], second.to_string());

    // Attaching someone else's upload fails.
    let (_mallory, mallory_token) = user_with_token(&pool, "mallory").await;
    let theirs = post_status(app(), &mallory_token, json!({"status": "their post"})).await;
    let their_id = theirs["id"].as_str().unwrap();
    let (steal, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{their_id}"),
        Some(&mallory_token),
        Some(json!({"media_ids": [second.to_string()]})),
    )
    .await;
    assert_eq!(steal, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_applies_media_attributes(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());
    let upload = media::create_local(
        &pool,
        media::NewLocalMedia {
            width: Some(10),
            height: Some(10),
            description: Some("old alt"),
            ..media::NewLocalMedia::new(alice.id, id::next(), "pic.jpg", "image/jpeg")
        },
    )
    .await
    .unwrap()
    .id;
    let posted = post_status(
        app(),
        &token,
        json!({"status": "pic", "media_ids": [upload.to_string()]}),
    )
    .await;
    let status_id = posted["id"].as_str().unwrap().to_owned();

    // A description-only edit (Mastodon's `media_attributes`) changes the
    // attached media's alt text and focal point, and marks the post edited.
    let (put_status, edited) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({"media_attributes": [
            {"id": upload.to_string(), "description": "new alt", "focus": "0.5,-0.5"},
        ]})),
    )
    .await;
    assert_eq!(put_status, StatusCode::OK, "{edited}");
    assert_eq!(edited["media_attachments"][0]["description"], "new alt");
    assert!(!edited["edited_at"].is_null(), "{edited}");
    let row = media::find_owned(&pool, upload, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.description.as_deref(), Some("new alt"));
    assert_eq!((row.focus_x, row.focus_y), (Some(0.5), Some(-0.5)));
    assert_eq!(
        row.status_id.unwrap().to_string(),
        status_id,
        "still attached"
    );

    // Re-sending the same attributes is a no-op: no new history version.
    let (_, again) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({"media_attributes": [
            {"id": upload.to_string(), "description": "new alt"},
        ]})),
    )
    .await;
    assert_eq!(again["edited_at"], edited["edited_at"]);
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 2, "{history}");
}

/// Signs `body` as a POST to `path` and sends it through the router.
async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers =
        signer.sign_post("plamenu.test", path, &bytes, std::time::SystemTime::now());
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
async fn inbound_edit_records_history_and_ignores_no_change_redelivery(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let router = test_app_with(pool.clone(), stub.clone());
    let app = || router.clone();
    let note_uri = format!("{}/notes/1", bob.actor.id);
    let note = |content: &str, updated: Option<&str>| {
        let mut object = json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": content,
            "published": "2026-06-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        });
        if let Some(updated) = updated {
            object["updated"] = json!(updated);
        }
        object
    };

    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": note("<p>first</p>", None),
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": note("<p>second</p>", Some("2026-06-01T13:00:00Z")),
    });
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/history", stored.id),
        None,
        None,
    )
    .await;
    let entries = history.as_array().unwrap();
    assert_eq!(entries.len(), 2, "{history}");
    assert_eq!(entries[0]["content"], "<p>first</p>");
    assert_eq!(entries[1]["content"], "<p>second</p>");

    // The exact original envelope also remains idempotent after an edit.
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .unwrap()
            .content,
        "<p>second</p>"
    );

    // A fresh activity id for the original Create must not overwrite a later
    // edit. This reaches the status-URI replay guard rather than activity-id
    // deduplication, including the inline-image/content rewrite after upsert.
    let mut create_replay = create.clone();
    create_replay["id"] = json!(format!("{note_uri}#creates/replay"));
    assert_eq!(
        post_signed(app(), "/inbox", &create_replay, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after_create_replay = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_create_replay.id, stored.id);
    assert_eq!(after_create_replay.content, "<p>second</p>");
    assert_eq!(after_create_replay.edited_at, stored.edited_at);
    let (_, replay_history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/history", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(replay_history, history, "a Create replay is not a revision");

    // Redelivering the same content (e.g. a quote-stamp redistribution) is
    // not an edit: no new version, edited_at unchanged.
    let redelivery = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/2"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": note("<p>second</p>", Some("2026-06-01T14:00:00Z")),
    });
    assert_eq!(
        post_signed(app(), "/inbox", &redelivery, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.edited_at, stored.edited_at);
    let (_, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/history", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 2);

    // Deduplicating Creates must not consume subsequent genuine Updates.
    let third_update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/3"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": note("<p>third</p>", Some("2026-06-01T15:00:00Z")),
    });
    let signer = bob.signer();
    let (update_code, replay_code) = tokio::join!(
        post_signed(app(), "/inbox", &third_update, &signer),
        post_signed(app(), "/inbox", &create_replay, &signer),
    );
    assert_eq!(update_code, StatusCode::ACCEPTED);
    assert_eq!(replay_code, StatusCode::ACCEPTED);
    let third = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third.id, stored.id);
    assert_eq!(third.content, "<p>third</p>");
    assert!(third.edited_at > stored.edited_at);
    let (_, third_history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}/history", stored.id),
        None,
        None,
    )
    .await;
    let entries = third_history.as_array().unwrap();
    assert_eq!(entries.len(), 3, "{third_history}");
    assert_eq!(entries[2]["content"], "<p>third</p>");

    // These remote edits/replays leave no pending deliveries behind either.
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
}

fn remote_create(actor: &RemoteUser, _uri: &str, object: Value, suffix: &str) -> Value {
    let mut created = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activities/{suffix}", actor.actor.id),
        "type": "Create", "actor": actor.actor.id, "object": null,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    created["object"] = object;
    created
}

fn remote_note(actor: &RemoteUser, uri: &str, content: &str) -> Value {
    json!({"id": uri, "type": "Note", "attributedTo": actor.actor.id,
        "content": content, "published": "2026-06-01T12:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"]})
}

#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_original_creates_share_row_and_delivery_claim(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = test_app_with(pool.clone(), stub);
    let uri = format!("{}/notes/concurrent", bob.actor.id);
    let create = remote_create(&bob, &uri, remote_note(&bob, &uri, "<p>once</p>"), "first");
    let signer = bob.signer();
    let (first, replay) = tokio::join!(
        post_signed(app.clone(), "/inbox", &create, &signer),
        post_signed(app.clone(), "/inbox", &create, &signer),
    );
    assert_eq!(first, StatusCode::ACCEPTED);
    assert_eq!(replay, StatusCode::ACCEPTED);
    let row = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(row.content, "<p>once</p>");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(&uri)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        !status::claim_delivery_side_effects(&pool, row.id)
            .await
            .unwrap()
    );
    let (_, history) = api(
        app,
        "GET",
        &format!("/api/v1/statuses/{}/history", row.id),
        None,
        None,
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_first_create_retry_repairs_attachments_without_duplicate_row(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let app = test_app_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone()]),
    );
    let uri = format!("{}/notes/repair", bob.actor.id);
    let mut note = remote_note(&bob, &uri, "<p>attachment repair</p>");
    note["attachment"] = json!([{"type":"Document", "mediaType":"image/png",
        "url":"https://remote.example/repair.png", "name":"repaired alt"}]);
    let create = remote_create(&bob, &uri, note, "repair");
    sqlx::query("CREATE FUNCTION fixture_reject_media() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture transient media failure'; END $$")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fixture_reject_media BEFORE INSERT ON media_attachments FOR EACH ROW EXECUTE FUNCTION fixture_reject_media()")
        .execute(&pool).await.unwrap();
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let partial = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert!(
        !status::claim_delivery_side_effects(&pool, partial.id)
            .await
            .unwrap(),
        "first failed ingestion already claimed its effects"
    );
    assert!(
        media::for_statuses_read_only(&pool, &[partial.id])
            .await
            .unwrap()
            .remove(&partial.id)
            .unwrap_or_default()
            .is_empty()
    );
    sqlx::query("DROP TRIGGER fixture_reject_media ON media_attachments")
        .execute(&pool)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }
    let repaired = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(repaired.id, partial.id);
    assert_eq!(repaired.content, partial.content);
    assert!(repaired.edited_at.is_none());
    let files = media::for_statuses_read_only(&pool, &[repaired.id])
        .await
        .unwrap()
        .remove(&repaired.id)
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].description.as_deref(), Some("repaired alt"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_other_author_cannot_reassign_known_uri_or_reserve_foreign_origin(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mallory = RemoteUser::new("remote.example", "mallory");
    let ancestor = RemoteUser::new("ancestor.example", "ancestor");
    let stub = StubFederation::with_actors([
        bob.actor.clone(),
        mallory.actor.clone(),
        ancestor.actor.clone(),
    ]);
    let app = test_app_with(pool.clone(), stub.clone());
    let uri = format!("{}/notes/owned", bob.actor.id);
    let create = remote_create(
        &bob,
        &uri,
        remote_note(&bob, &uri, "<p>Bob's canonical content</p>"),
        "owned",
    );
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let original = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    let parent_uri = format!("{}/notes/parent", bob.actor.id);
    let parent = remote_create(
        &bob,
        &parent_uri,
        remote_note(&bob, &parent_uri, "<p>parent</p>"),
        "parent",
    );
    assert_eq!(
        post_signed(app.clone(), "/inbox", &parent, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let mut forged = remote_note(&mallory, &uri, "<p>forged content</p>");
    forged["inReplyTo"] = json!(parent_uri);
    forged["attachment"] = json!([{"type":"Document", "mediaType":"image/png", "url":"https://remote.example/forged.png"}]);
    let attack = remote_create(&mallory, &uri, forged.clone(), "steal");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &attack, &mallory.signer()).await,
        StatusCode::FORBIDDEN
    );
    let after = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(after.account_id, original.account_id);
    assert_eq!(after.content, original.content);
    assert_eq!(after.in_reply_to_id, original.in_reply_to_id);
    let parent_ref: Option<String> =
        sqlx::query_scalar("SELECT in_reply_to_uri FROM statuses WHERE id=$1")
            .bind(after.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(parent_ref.is_none());
    assert!(
        media::for_statuses_read_only(&pool, &[after.id])
            .await
            .unwrap()
            .remove(&after.id)
            .unwrap_or_default()
            .is_empty()
    );
    let update = json!({"@context":"https://www.w3.org/ns/activitystreams", "type":"Update",
        "id":format!("{}/activities/edit-victim", mallory.actor.id), "actor":mallory.actor.id, "object":forged});
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &mallory.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        status::find_by_uri(&pool, &uri)
            .await
            .unwrap()
            .unwrap()
            .content,
        original.content
    );
    let unknown_parent_uri = format!("{}/notes/never-allocated", ancestor.actor.id);
    stub.objects.lock().unwrap().insert(
        unknown_parent_uri.clone(),
        remote_note(
            &ancestor,
            &unknown_parent_uri,
            "<p>ancestor must not be discovered</p>",
        ),
    );
    forged["inReplyTo"] = json!(unknown_parent_uri);
    let unknown_parent_attack = remote_create(&mallory, &uri, forged, "unknown-ancestor");
    stub.fetches.lock().unwrap().clear();
    assert_eq!(
        post_signed(
            app.clone(),
            "/inbox",
            &unknown_parent_attack,
            &mallory.signer()
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert!(!stub.fetches.lock().unwrap().contains(&unknown_parent_uri));
    assert!(
        status::find_by_uri(&pool, &unknown_parent_uri)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        plamenu_db::account::find_by_uri(&pool, &ancestor.actor.id)
            .await
            .unwrap()
            .is_none()
    );
    let foreign_uri = "https://victim.example/notes/unseen";
    let reserve = remote_create(
        &mallory,
        foreign_uri,
        remote_note(&mallory, foreign_uri, "<p>foreign reservation</p>"),
        "reserve",
    );
    assert_eq!(
        post_signed(app.clone(), "/inbox", &reserve, &mallory.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, foreign_uri)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn held_initial_create_finishes_before_update_and_cannot_restore_stale_video_sidecars(
    pool: PgPool,
) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = plamenu::build_router(state.clone());
    let uri = format!("{}/videos/held", bob.actor.id);
    let chapters_uri = "https://remote.example/videos/held/chapters";
    stub.objects.lock().unwrap().insert(
        chapters_uri.into(),
        json!({"id":chapters_uri,
        "hasPart":[{"name":"old chapter", "startOffset":0}]}),
    );
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    *stub.remote_media_fetch_gate.lock().unwrap() = Some((chapters_uri.into(), gate.clone()));
    *stub.remote_media_fetch_started.lock().unwrap() = Some(started.clone());
    let mut object = remote_note(&bob, &uri, "<p>first video</p>");
    object["type"] = json!("Video");
    object["name"] = json!("first title");
    object["category"] = json!({"identifier":1});
    object["licence"] =
        json!({"identifier":1,"url":"https://creativecommons.org/licenses/by/4.0/"});
    object["url"] = json!([{"type":"Link","mediaType":"video/mp4", "href":"https://remote.example/held.mp4", "height":240, "size":1000}]);
    object["hasParts"] = json!(chapters_uri);
    let create = remote_create(&bob, &uri, object.clone(), "held");
    let create_app = app.clone();
    let create_signer = bob.signer();
    let first =
        tokio::spawn(
            async move { post_signed(create_app, "/inbox", &create, &create_signer).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let initial = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    object["content"] = json!("<p>edited video</p>");
    object["name"] = json!("edited title");
    object["category"] = json!({"identifier":2});
    object["licence"] =
        json!({"identifier":2,"url":"https://creativecommons.org/licenses/by-sa/4.0/"});
    object["summary"] = json!("edited warning");
    object["sensitive"] = json!(true);
    object["contentMap"] = json!({"fr":"<p>edited video</p>"});
    object["hasParts"] = json!([{"name":"edited chapter", "startOffset":0}]);
    object["updated"] = json!("2026-06-01T13:00:00Z");
    let update = json!({"@context":"https://www.w3.org/ns/activitystreams", "id":format!("{uri}#update"),
        "type":"Update", "actor":bob.actor.id, "object":object});
    let update_app = app.clone();
    let update_signer = bob.signer();
    let edit =
        tokio::spawn(
            async move { post_signed(update_app, "/inbox", &update, &update_signer).await },
        );
    // A barrier holds Create inside production sidecar enrichment. Update is
    // guaranteed to remain pending for as long as that object guard is owned.
    common::wait_remote_status_writers(&state, &uri, 2).await;
    assert!(
        !edit.is_finished(),
        "Update must wait behind held initial Create"
    );
    *stub.remote_media_fetch_gate.lock().unwrap() = None;
    gate.add_permits(1);
    assert_eq!(first.await.unwrap(), StatusCode::ACCEPTED);
    assert_eq!(edit.await.unwrap(), StatusCode::ACCEPTED);
    let edited = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(edited.id, initial.id);
    assert_eq!(edited.content, "<p>edited video</p>");
    assert_eq!(edited.title.as_deref(), Some("edited title"));
    assert_eq!(edited.spoiler_text, "edited warning");
    assert!(edited.sensitive);
    assert_eq!(edited.language.as_deref(), Some("fr"));
    assert!(edited.edited_at.is_some());
    let files = media::for_statuses_read_only(&pool, &[edited.id])
        .await
        .unwrap()
        .remove(&edited.id)
        .unwrap();
    assert_eq!(files.len(), 1);
    let metadata = plamenu_db::video_metadata::for_media(&pool, &[files[0].id])
        .await
        .unwrap()
        .remove(&files[0].id)
        .unwrap();
    assert_eq!(metadata.category, Some(2));
    assert_eq!(metadata.licence, Some(2));
    assert!(metadata.licence_version_known);
    let chapters = plamenu_db::video_chapters::for_media(&pool, &[files[0].id])
        .await
        .unwrap()
        .remove(&files[0].id)
        .unwrap();
    assert_eq!(chapters.len(), 1);
    assert_eq!(chapters[0].title, "edited chapter");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn nested_cyclic_quotes_resolve_inserted_rows_without_reentrant_write_deadlock(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let first_uri = format!("{}/notes/quote-cycle-a", bob.actor.id);
    let second_uri = format!("{}/notes/quote-cycle-b", bob.actor.id);
    let mut first = remote_note(&bob, &first_uri, "<p>first cycle</p>");
    let mut second = remote_note(&bob, &second_uri, "<p>second cycle</p>");
    first["quote"] = json!(second_uri);
    second["quote"] = json!(first_uri);
    stub.objects
        .lock()
        .unwrap()
        .insert(second_uri.clone(), second);
    let app = test_app_with(pool.clone(), stub);
    let create = remote_create(&bob, &first_uri, first, "quote-cycle");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        post_signed(app, "/inbox", &create, &bob.signer()),
    )
    .await
    .unwrap();
    assert_eq!(result, StatusCode::ACCEPTED);
    let first = status::find_by_uri(&pool, &first_uri)
        .await
        .unwrap()
        .unwrap();
    let second = status::find_by_uri(&pool, &second_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu_db::quote::find_by_status_uri(&pool, &first_uri)
            .await
            .unwrap()
            .unwrap()
            .quoted_status_id,
        Some(second.id)
    );
    assert_eq!(
        plamenu_db::quote::find_by_status_uri(&pool, &second_uri)
            .await
            .unwrap()
            .unwrap()
            .quoted_status_id,
        Some(first.id)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn saturated_nested_quote_ingest_returns_retryable_error_then_repairs_cycle(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let first_uri = format!("{}/notes/saturated-a", bob.actor.id);
    let second_uri = format!("{}/notes/saturated-b", bob.actor.id);
    let mut first = remote_note(&bob, &first_uri, "<p>first saturated cycle</p>");
    let mut second = remote_note(&bob, &second_uri, "<p>second saturated cycle</p>");
    first["quote"] = json!(second_uri);
    second["quote"] = json!(first_uri);
    stub.objects
        .lock()
        .unwrap()
        .insert(second_uri.clone(), second);
    let state = test_state_with(pool.clone(), stub);
    let mut held = Vec::new();
    for index in 0..1023 {
        held.push(
            state
                .remote_status_writes
                .lock(&format!("https://fixture.example/held/{index}"))
                .await
                .unwrap(),
        );
    }
    let app = plamenu::build_router(state);
    let create = remote_create(&bob, &first_uri, first, "saturated-cycle");
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        post_signed(app.clone(), "/inbox", &create, &bob.signer()),
    )
    .await
    .unwrap();
    assert_eq!(
        code,
        StatusCode::SERVICE_UNAVAILABLE,
        "nested registry capacity exhaustion must request a retry, not skip or deadlock"
    );
    let partial = status::find_by_uri(&pool, &first_uri)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_by_uri(&pool, &second_uri)
            .await
            .unwrap()
            .is_none()
    );
    drop(held);
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        post_signed(app, "/inbox", &create, &bob.signer()),
    )
    .await
    .unwrap();
    assert_eq!(code, StatusCode::ACCEPTED);
    let repaired = status::find_by_uri(&pool, &first_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repaired.id, partial.id);
    let second = status::find_by_uri(&pool, &second_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu_db::quote::find_by_status_uri(&pool, &first_uri)
            .await
            .unwrap()
            .unwrap()
            .quoted_status_id,
        Some(second.id)
    );
    assert_eq!(
        plamenu_db::quote::find_by_status_uri(&pool, &second_uri)
            .await
            .unwrap()
            .unwrap()
            .quoted_status_id,
        Some(repaired.id)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edited_cold_first_create_promotes_canonical_media_mentions_tags_and_quote_once(
    pool: PgPool,
) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let (dave, dave_token) = user_with_token(&pool, "dave").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    let app = plamenu::build_router(state.clone());
    let target = post_status(
        app.clone(),
        &token,
        json!({"status":"canonical quote target"}),
    )
    .await;
    let new_target = post_status(
        app.clone(),
        &dave_token,
        json!({"status":"new canonical quote target"}),
    )
    .await;
    let new_target_id = new_target["id"].as_str().unwrap().parse::<i64>().unwrap();
    let new_target_uri = new_target["uri"].as_str().unwrap();
    let target_id = target["id"].as_str().unwrap().parse::<i64>().unwrap();
    let target_uri = target["uri"].as_str().unwrap();
    let author = remote::refresh_remote_actor(&state, &bob.actor)
        .await
        .unwrap();
    let uri = format!("{}/notes/edited-cold", bob.actor.id);
    plamenu_db::quote::create(
        &pool,
        plamenu_db::quote::NewQuote {
            quote_id: id::next(),
            status_id: None,
            status_uri: &uri,
            account_id: author.id,
            quoted_status_id: Some(target_id),
            quoted_account_id: Some(alice.id),
            state: "accepted",
            activity_uri: None,
            approval_uri: None,
            quoted_uri: Some(target_uri),
            legacy: false,
        },
    )
    .await
    .unwrap();
    let mut first = remote_note(&bob, &uri, "<p>old cold body</p>");
    first["attachment"] = json!([{"type":"Image","mediaType":"image/jpeg","url":"https://remote.example/old-cold.jpg","name":"old alt"}]);
    first["tag"] = json!([{"type":"Mention","href":"https://plamenu.test/users/alice","name":"@alice"},
        {"type":"Hashtag","name":"#stale"}]);
    first["quote"] = json!(target_uri);
    first["summary"] = json!("old warning");
    first["contentMap"] = json!({"en":"<p>old cold body</p>"});
    let cold = plamenu::ingest::ingest_remote_note_in_context(
        &state,
        &author,
        &first,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    let mut latest = first.clone();
    latest["content"] = json!("<p>new canonical cold body</p>");
    latest["updated"] = json!("2026-06-01T13:00:00Z");
    latest["attachment"] = json!([{"type":"Image","mediaType":"image/jpeg","url":"https://remote.example/new-cold.jpg","name":"canonical alt"}]);
    latest["tag"] = json!([{"type":"Mention","href":"https://plamenu.test/users/carol","name":"@carol"},
        {"type":"Hashtag","name":"#canonical"}]);
    latest["summary"] = json!("canonical warning");
    latest["sensitive"] = json!(true);
    latest["contentMap"] = json!({"fr":"<p>new canonical cold body</p>"});
    // Drop the old accepted quote, then introduce a new target through the
    // supported History Update paths. Seed the resulting local consent row as
    // accepted, as the QuoteRequest flow does, before any live delivery.
    latest.as_object_mut().unwrap().remove("quote");
    let without_quote = plamenu::ingest::update_remote_note_in_context(
        &state,
        &author,
        &cold,
        &latest,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    latest["quote"] = json!(new_target_uri);
    latest["updated"] = json!("2026-06-01T14:00:00Z");
    let canonical = plamenu::ingest::update_remote_note_in_context(
        &state,
        &author,
        &without_quote,
        &latest,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    assert!(canonical.edited_at.is_some());
    let quote = plamenu_db::quote::find_by_status_uri(&pool, &uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(quote.quoted_status_id, Some(new_target_id));
    plamenu_db::quote::set_state(&pool, quote.id, "accepted")
        .await
        .unwrap();
    let canonical_history: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,content FROM status_edits WHERE status_id=$1 ORDER BY id")
            .bind(cold.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    let before_media = media::for_statuses_read_only(&pool, &[cold.id])
        .await
        .unwrap()
        .remove(&cold.id)
        .unwrap();
    assert_eq!(before_media.len(), 1);
    assert_eq!(
        before_media[0].remote_url.as_deref(),
        Some("https://remote.example/new-cold.jpg")
    );
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs_before, 0);
    let before_notifications: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE status_id=$1")
            .bind(cold.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before_notifications, 0);
    let create = remote_create(&bob, &uri, first, "cold-first-live");
    let mut first_promoted_at = None;
    for attempt in 0..2 {
        assert_eq!(
            post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
        let current = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
        if attempt == 0 {
            first_promoted_at = Some(current.updated_at);
        } else {
            assert_eq!(Some(current.updated_at), first_promoted_at);
        }
    }
    let promoted = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(promoted.id, cold.id);
    assert_eq!(promoted.content, canonical.content);
    assert_eq!(promoted.edited_at, canonical.edited_at);
    assert_eq!(promoted.in_reply_to_id, canonical.in_reply_to_id);
    assert_eq!(promoted.spoiler_text, canonical.spoiler_text);
    assert_eq!(promoted.language, canonical.language);
    assert_eq!(promoted.sensitive, canonical.sensitive);
    assert!(promoted.updated_at >= canonical.updated_at);
    assert_eq!(promoted.visibility, canonical.visibility);
    let after_history: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,content FROM status_edits WHERE status_id=$1 ORDER BY id")
            .bind(cold.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(after_history, canonical_history);
    let files = media::for_statuses_read_only(&pool, &[cold.id])
        .await
        .unwrap()
        .remove(&cold.id)
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, before_media[0].id);
    assert_eq!(files[0].remote_url, before_media[0].remote_url);
    assert_eq!(files[0].description.as_deref(), Some("canonical alt"));
    let deferred: bool =
        sqlx::query_scalar("SELECT history_deferred FROM media_attachments WHERE id=$1")
            .bind(files[0].id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deferred);
    let jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs WHERE media_id=$1")
            .bind(files[0].id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(jobs, 1);
    let active_mentions = plamenu_db::mention::for_statuses(&pool, &[cold.id], true)
        .await
        .unwrap()
        .remove(&cold.id)
        .unwrap();
    assert_eq!(
        active_mentions
            .iter()
            .map(|account| account.id)
            .collect::<Vec<_>>(),
        vec![carol.id]
    );
    let reasons: Vec<(i64, String)> = sqlx::query_as(
        "SELECT account_id,kind FROM notifications WHERE status_id=$1 ORDER BY account_id",
    )
    .bind(cold.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected = vec![
        (dave.id, "quote".to_owned()),
        (carol.id, "mention".to_owned()),
    ];
    expected.sort();
    assert_eq!(reasons, expected);
    let usages: Vec<(String,i64)> = sqlx::query_as("SELECT t.name,SUM(tu.uses)::bigint FROM tag_usages tu JOIN tags t ON t.id=tu.tag_id WHERE tu.account_id=$1 GROUP BY t.name ORDER BY t.name")
        .bind(author.id).fetch_all(&pool).await.unwrap();
    assert_eq!(usages, vec![("canonical".to_owned(), 1)]);
    let current_quote = plamenu_db::quote::find_by_status_uri(&pool, &uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current_quote.id, quote.id);
    assert_eq!(current_quote.quoted_status_id, Some(new_target_id));
    assert_eq!(current_quote.state, "accepted");
    assert_eq!(
        status::ingest_provenance(&pool, cold.id)
            .await
            .unwrap()
            .as_deref(),
        Some("delivery")
    );
    assert!(
        !status::claim_delivery_side_effects(&pool, cold.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unstamped_question_tallies_after_real_edit_preserve_body_options_history_and_timestamp(
    pool: PgPool,
) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone()]),
    );
    let app = plamenu::build_router(state.clone());
    let uri = format!("{}/notes/edited-question", bob.actor.id);
    let mut object = remote_note(&bob, &uri, "<p>original poll body</p>");
    object["type"] = json!("Question");
    object["oneOf"] = json!([{"type":"Note","name":"yes","replies":{"totalItems":1}},
        {"type":"Note","name":"no","replies":{"totalItems":0}}]);
    object["votersCount"] = json!(1);
    object["endTime"] = json!("2027-01-01T00:00:00Z");
    let create = remote_create(&bob, &uri, object.clone(), "question");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let original = object.clone();
    object["content"] = json!("<p>edited poll body</p>");
    object["updated"] = json!("2026-06-01T13:00:00Z");
    object["summary"] = json!("canonical poll warning");
    object["sensitive"] = json!(true);
    object["attachment"] = json!([{"type":"Image","mediaType":"image/jpeg","url":"https://remote.example/poll-canonical.jpg"}]);
    let update = json!({"@context":"https://www.w3.org/ns/activitystreams", "id":format!("{uri}#real-edit"),
        "type":"Update", "actor":bob.actor.id, "object":object});
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let edited = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert!(edited.edited_at.is_some());
    let canonical_poll = plamenu_db::poll::find_by_status(&pool, edited.id)
        .await
        .unwrap()
        .unwrap();
    let vote_id = plamenu_db::poll::insert_vote(&pool, canonical_poll.id, alice.id, 0, None)
        .await
        .unwrap()
        .unwrap();
    let canonical_media = media::for_statuses_read_only(&pool, &[edited.id])
        .await
        .unwrap()
        .remove(&edited.id)
        .unwrap();
    let (_, history) = api(
        app.clone(),
        "GET",
        &format!("/api/v1/statuses/{}/history", edited.id),
        None,
        None,
    )
    .await;
    let mut tally = original;
    tally["type"] = json!(["Question", "Note"]);
    tally["oneOf"][0]["replies"]["totalItems"] = json!(10);
    tally["oneOf"][1]["replies"]["totalItems"] = json!(5);
    tally["votersCount"] = json!(15);
    let mut tally_update = json!({"@context":"https://www.w3.org/ns/activitystreams", "id":format!("{uri}#tallies"),
        "type":"Update", "actor":bob.actor.id, "object":tally});
    assert_eq!(
        post_signed(app.clone(), "/inbox", &tally_update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(after.content, edited.content);
    assert_eq!(after.edited_at, edited.edited_at);
    assert_eq!(after.updated_at, edited.updated_at);
    assert_eq!(after.spoiler_text, edited.spoiler_text);
    assert_eq!(after.sensitive, edited.sensitive);
    let after_media = media::for_statuses_read_only(&pool, &[edited.id])
        .await
        .unwrap()
        .remove(&edited.id)
        .unwrap();
    assert_eq!(after_media.len(), canonical_media.len());
    assert_eq!(after_media[0].id, canonical_media[0].id);
    assert_eq!(after_media[0].remote_url, canonical_media[0].remote_url);
    let poll = plamenu_db::poll::find_by_status(&pool, edited.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(poll.options, ["yes", "no"]);
    assert_eq!(poll.cached_tallies, [10, 5]);
    assert_eq!(poll.voters_count, Some(15));
    assert_eq!(poll.id, canonical_poll.id);
    assert_eq!(
        plamenu_db::poll::votes_by(&pool, poll.id, alice.id)
            .await
            .unwrap(),
        [0]
    );
    let preserved_vote: i64 =
        sqlx::query_scalar("SELECT id FROM poll_votes WHERE poll_id=$1 AND account_id=$2")
            .bind(poll.id)
            .bind(alice.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(preserved_vote, vote_id);
    let (_, after_history) = api(
        app.clone(),
        "GET",
        &format!("/api/v1/statuses/{}/history", edited.id),
        None,
        None,
    )
    .await;
    assert_eq!(after_history, history);
    // An unstamped payload cannot change the canonical poll options either.
    tally_update["id"] = json!(format!("{uri}#unstamped-option-rollback"));
    tally_update["object"]["oneOf"][0]["name"] = json!("obsolete option");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &tally_update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let after = plamenu_db::poll::find_by_status(&pool, edited.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.options, poll.options);
    assert_eq!(after.cached_tallies, poll.cached_tallies);
    assert_eq!(
        plamenu_db::poll::votes_by(&pool, poll.id, alice.id)
            .await
            .unwrap(),
        [0]
    );
    // Matching-option counter-only closure remains valid after a body edit.
    tally_update["id"] = json!(format!("{uri}#closed-tallies"));
    tally_update["object"]["oneOf"][0]["name"] = json!("yes");
    tally_update["object"]["endTime"] = json!("2020-01-01T00:00:00Z");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &tally_update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let closed = plamenu_db::poll::find_by_status(&pool, edited.id)
        .await
        .unwrap()
        .unwrap();
    assert!(closed.expired());
    assert_eq!(closed.options, poll.options);
    assert_eq!(
        plamenu_db::poll::votes_by(&pool, poll.id, alice.id)
            .await
            .unwrap(),
        [0]
    );
    let final_status = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(final_status.content, edited.content);
    assert_eq!(final_status.edited_at, edited.edited_at);
    let (_, final_history) = api(
        app,
        "GET",
        &format!("/api/v1/statuses/{}/history", edited.id),
        None,
        None,
    )
    .await;
    assert_eq!(final_history, history);
    let author = plamenu_db::account::find_by_id(&pool, edited.account_id)
        .await
        .unwrap()
        .unwrap();
    let mut archived = tally_update["object"].clone();
    archived["oneOf"][0]["replies"]["totalItems"] = json!(1);
    archived["endTime"] = json!("2027-01-01T00:00:00Z");
    plamenu::ingest::update_remote_note_in_context(
        &state,
        &author,
        &final_status,
        &archived,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    let after_archive = plamenu_db::poll::find_by_status(&pool, edited.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_archive.cached_tallies, closed.cached_tallies);
    assert_eq!(after_archive.expires_at, closed.expires_at);
    assert_eq!(after_archive.options, closed.options);
    assert_eq!(
        plamenu_db::poll::votes_by(&pool, poll.id, alice.id)
            .await
            .unwrap(),
        [0]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edited_cold_video_promotion_preserves_canonical_captions_chapters_and_metadata(
    pool: PgPool,
) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone()]),
    );
    let app = plamenu::build_router(state.clone());
    let author = remote::refresh_remote_actor(&state, &bob.actor)
        .await
        .unwrap();
    let uri = format!("{}/videos/cold-video", bob.actor.id);
    let mut first = remote_note(&bob, &uri, "<p>old video body</p>");
    first["type"] = json!("Video");
    first["name"] = json!("old video title");
    first["category"] = json!({"identifier":1});
    first["licence"] = json!({"identifier":1,"url":"https://creativecommons.org/licenses/by/4.0/"});
    first["url"] = json!([{"type":"Link","mediaType":"video/mp4","href":"https://remote.example/cold-video.mp4","height":240,"size":1000}]);
    first["hasParts"] = json!([{"name":"old chapter","startOffset":0}]);
    first["subtitleLanguage"] = json!([{"identifier":"en","url":"https://remote.example/old.vtt"}]);
    let cold = plamenu::ingest::ingest_remote_note_in_context(
        &state,
        &author,
        &first,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    let mut latest = first.clone();
    latest["content"] = json!("<p>canonical video body</p>");
    latest["name"] = json!("canonical video title");
    latest["category"] = json!({"identifier":2});
    latest["licence"] =
        json!({"identifier":2,"url":"https://creativecommons.org/licenses/by-sa/4.0/"});
    latest["hasParts"] = json!([{"name":"canonical chapter","startOffset":7}]);
    latest["subtitleLanguage"] = json!([{"identifier":"fr","url":"https://remote.example/canonical.vtt","automaticallyGenerated":true}]);
    latest["summary"] = json!("canonical warning");
    latest["sensitive"] = json!(true);
    latest["contentMap"] = json!({"fr":"<p>canonical video body</p>"});
    latest["updated"] = json!("2026-06-01T14:00:00Z");
    let canonical = plamenu::ingest::update_remote_note_in_context(
        &state,
        &author,
        &cold,
        &latest,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    let files_before = media::for_statuses_read_only(&pool, &[cold.id])
        .await
        .unwrap()
        .remove(&cold.id)
        .unwrap();
    assert_eq!(files_before.len(), 1);
    let media_id = files_before[0].id;
    let metadata_before = plamenu_db::video_metadata::for_media(&pool, &[media_id])
        .await
        .unwrap()
        .remove(&media_id)
        .unwrap();
    let chapters_before = plamenu_db::video_chapters::for_media(&pool, &[media_id])
        .await
        .unwrap()
        .remove(&media_id)
        .unwrap();
    let captions_before = plamenu_db::video_captions::for_media(&pool, &[media_id])
        .await
        .unwrap()
        .remove(&media_id)
        .unwrap();
    assert_eq!(metadata_before.category, Some(2));
    assert_eq!(chapters_before[0].title, "canonical chapter");
    assert_eq!(captions_before[0].language, "fr");
    let create = remote_create(&bob, &uri, first, "cold-video-first-live");
    for _ in 0..2 {
        assert_eq!(
            post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }
    let promoted = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(promoted.content, canonical.content);
    assert_eq!(promoted.title, canonical.title);
    assert_eq!(promoted.spoiler_text, canonical.spoiler_text);
    assert_eq!(promoted.sensitive, canonical.sensitive);
    assert_eq!(promoted.language, canonical.language);
    assert_eq!(promoted.edited_at, canonical.edited_at);
    let files_after = media::for_statuses_read_only(&pool, &[cold.id])
        .await
        .unwrap()
        .remove(&cold.id)
        .unwrap();
    assert_eq!(files_after.len(), 1);
    assert_eq!(files_after[0].id, media_id);
    assert_eq!(files_after[0].remote_url, files_before[0].remote_url);
    let metadata = plamenu_db::video_metadata::for_media(&pool, &[media_id])
        .await
        .unwrap()
        .remove(&media_id)
        .unwrap();
    assert_eq!(metadata.category, metadata_before.category);
    assert_eq!(metadata.licence, metadata_before.licence);
    assert_eq!(
        metadata.licence_version_known,
        metadata_before.licence_version_known
    );
    assert_eq!(
        plamenu_db::video_chapters::for_media(&pool, &[media_id])
            .await
            .unwrap()
            .remove(&media_id)
            .unwrap(),
        chapters_before
    );
    let captions = plamenu_db::video_captions::for_media(&pool, &[media_id])
        .await
        .unwrap()
        .remove(&media_id)
        .unwrap();
    assert_eq!(captions.len(), captions_before.len());
    assert_eq!(captions[0].language, captions_before[0].language);
    assert_eq!(captions[0].remote_url, captions_before[0].remote_url);
    assert_eq!(
        captions[0].automatically_generated,
        captions_before[0].automatically_generated
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn held_explicit_origin_create_and_stale_history_cannot_restore_newer_live_edit(
    pool: PgPool,
) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let uri = format!("{}/notes/origin-discovery-race", bob.actor.id);
    let mut first = remote_note(&bob, &uri, "<p>old fetched body</p>");
    first["attachment"] = json!([{"type":"Image","mediaType":"image/jpeg","url":"https://remote.example/discovery-old.jpg"}]);
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.objects
        .lock()
        .unwrap()
        .insert(uri.clone(), first.clone());
    stub.gate_object_fetches();
    let state = test_state_with(pool.clone(), stub.clone());
    let app = plamenu::build_router(state.clone());
    let resolve_state = state.clone();
    let resolve_uri = uri.clone();
    let resolver = tokio::spawn(async move {
        plamenu::ingest::resolve_or_fetch_status(&resolve_state, &resolve_uri).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if stub.fetches.lock().unwrap().contains(&uri) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let create = remote_create(&bob, &uri, first.clone(), "discovery-live");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let mut latest = first.clone();
    latest["content"] = json!("<p>new canonical discovery body</p>");
    latest["summary"] = json!("canonical discovery warning");
    latest["attachment"] = json!([{"type":"Image","mediaType":"image/jpeg","url":"https://remote.example/discovery-new.jpg"}]);
    latest["updated"] = json!("2026-06-01T14:00:00Z");
    let update = json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{uri}#newer"),"type":"Update","actor":bob.actor.id,"object":latest});
    assert_eq!(
        post_signed(app, "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let canonical = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    let before_media = media::for_statuses_read_only(&pool, &[canonical.id])
        .await
        .unwrap()
        .remove(&canonical.id)
        .unwrap();
    let before_history: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,content FROM status_edits WHERE status_id=$1 ORDER BY id")
            .bind(canonical.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    stub.open_object_fetches();
    let resolved = tokio::time::timeout(std::time::Duration::from_secs(5), resolver)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(resolved.content, canonical.content);
    let author = plamenu_db::account::find_by_id(&pool, canonical.account_id)
        .await
        .unwrap()
        .unwrap();
    // A history batch may likewise have selected an absent URI before this
    // live edit committed and then reach storage with its old captured object.
    plamenu::ingest::ingest_remote_note_in_context(
        &state,
        &author,
        &first,
        plamenu::ingest::RemoteIngestContext::History,
    )
    .await
    .unwrap();
    let after = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(after.content, canonical.content);
    assert_eq!(after.spoiler_text, canonical.spoiler_text);
    assert_eq!(after.edited_at, canonical.edited_at);
    assert_eq!(after.updated_at, canonical.updated_at);
    let after_media = media::for_statuses_read_only(&pool, &[canonical.id])
        .await
        .unwrap()
        .remove(&canonical.id)
        .unwrap();
    assert_eq!(after_media.len(), before_media.len());
    assert_eq!(after_media[0].id, before_media[0].id);
    assert_eq!(after_media[0].remote_url, before_media[0].remote_url);
    let after_history: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,content FROM status_edits WHERE status_id=$1 ORDER BY id")
            .bind(canonical.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(after_history, before_history);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edited_explicit_private_promotion_never_records_public_emoji_usage(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone()]),
    );
    let app = plamenu::build_router(state.clone());
    let author = remote::refresh_remote_actor(&state, &bob.actor)
        .await
        .unwrap();
    plamenu_db::custom_emoji::upsert_remote(
        &pool,
        plamenu_db::custom_emoji::RemoteEmojiData {
            shortcode: "canonical",
            domain: "remote.example",
            uri: Some("https://remote.example/emoji/canonical"),
            image_remote_url: "https://remote.example/canonical.png",
            updated: None,
        },
    )
    .await
    .unwrap();
    for visibility in ["private", "direct"] {
        let uri = format!("{}/notes/edited-{visibility}", bob.actor.id);
        let mut first = remote_note(&bob, &uri, "<p>old private body</p>");
        first["to"] = if visibility == "private" {
            json!([bob.actor.followers])
        } else {
            json!([alice
                .uri
                .as_deref()
                .unwrap_or("https://plamenu.test/users/alice")])
        };
        first["cc"] = json!([]);
        let cold = plamenu::ingest::ingest_remote_note_in_context(
            &state,
            &author,
            &first,
            plamenu::ingest::RemoteIngestContext::ExplicitResolution,
        )
        .await
        .unwrap();
        assert_eq!(cold.visibility, visibility);
        let mut latest = first.clone();
        latest["content"] = json!("<p>canonical private :canonical:</p>");
        latest["updated"] = json!("2026-06-01T14:00:00Z");
        if visibility == "direct" {
            latest["to"] = json!([
                "https://plamenu.test/users/alice",
                "https://plamenu.test/users/carol"
            ]);
            latest["tag"] = json!([{"type":"Mention","href":"https://plamenu.test/users/carol","name":"@carol"}]);
        }
        let canonical = plamenu::ingest::update_remote_note_in_context(
            &state,
            &author,
            &cold,
            &latest,
            plamenu::ingest::RemoteIngestContext::ExplicitResolution,
        )
        .await
        .unwrap();
        let late_orphan = if visibility == "private" {
            let reply_uri = format!("{}/notes/late-orphan", bob.actor.id);
            let orphan = status::upsert_remote(
                &pool,
                status::NewRemoteStatus {
                    uri: &reply_uri,
                    account_id: author.id,
                    content: "<p>late orphan</p>",
                    created_at: time::OffsetDateTime::now_utc(),
                    visibility: "public",
                    in_reply_to_id: None,
                    in_reply_to_uri: Some(&uri),
                    spoiler_text: "",
                    sensitive: false,
                    language: None,
                    url: None,
                    quote_approval_policy: 0,
                    title: None,
                    object_type: None,
                    external_url: None,
                },
            )
            .await
            .unwrap();
            {
                let mut conn = pool.acquire().await.unwrap();
                plamenu::conversations::ensure_conversation(
                    &mut conn,
                    &orphan,
                    true,
                    plamenu_db::conversation::ContextRefs::default(),
                )
                .await
                .unwrap();
            }
            Some(orphan.id)
        } else {
            None
        };
        let before_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM account_conversations WHERE $1=ANY(status_ids)",
        )
        .bind(cold.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(before_rows, 0);
        let create = remote_create(&bob, &uri, first, visibility);
        for _ in 0..2 {
            assert_eq!(
                post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
                StatusCode::ACCEPTED
            );
        }
        let promoted = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
        assert_eq!(promoted.content, canonical.content);
        assert_eq!(promoted.visibility, visibility);
        assert_eq!(promoted.edited_at, canonical.edited_at);
        if let Some(orphan_id) = late_orphan {
            let joined = status::find_by_id(&pool, orphan_id).await.unwrap().unwrap();
            assert_eq!(joined.in_reply_to_id, Some(cold.id));
            assert_eq!(
                joined.visibility, "private",
                "canonical parent ceiling clamps the newly adopted child"
            );
        }
        if visibility == "direct" {
            let rows: Vec<(i64,Vec<i64>)> = sqlx::query_as("SELECT account_id,status_ids FROM account_conversations WHERE $1=ANY(status_ids) ORDER BY account_id")
                .bind(cold.id).fetch_all(&pool).await.unwrap();
            let mut expected = vec![alice.id, carol.id];
            expected.sort_unstable();
            assert_eq!(
                rows.iter()
                    .map(|(account_id, _)| *account_id)
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(rows.iter().all(|(_, ids)| ids == &vec![cold.id]));
            let notified: Vec<i64> = sqlx::query_scalar("SELECT account_id FROM notifications WHERE status_id=$1 AND kind='mention' ORDER BY account_id")
                .bind(cold.id).fetch_all(&pool).await.unwrap();
            assert_eq!(
                notified,
                vec![carol.id],
                "current explicit mention notifies once; silent audience gets DM inbox only"
            );
        }
    }
    let uses: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(post_uses),0)::bigint FROM custom_emoji_usages WHERE account_id=$1",
    )
    .bind(author.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(uses, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_foreign_known_uri_cannot_be_reinterpreted_as_a_poll_vote(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mallory = RemoteUser::new("remote.example", "mallory");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone(), mallory.actor.clone()]),
    );
    let app = plamenu::build_router(state.clone());
    let uri = format!("{}/notes/not-a-vote", bob.actor.id);
    let create = remote_create(
        &bob,
        &uri,
        remote_note(&bob, &uri, "<p>Bob owns this URI</p>"),
        "owned-vote-attack",
    );
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let owned = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    let poll_entity = post_status(
        app.clone(),
        &token,
        json!({"status":"choose","poll":{"options":["yes","no"],"expires_in":3600}}),
    )
    .await;
    let poll_status_id = poll_entity["id"].as_str().unwrap().parse::<i64>().unwrap();
    let before = plamenu_db::poll::find_by_status(&pool, poll_status_id)
        .await
        .unwrap()
        .unwrap();
    let attacker = remote::refresh_remote_actor(&state, &mallory.actor)
        .await
        .unwrap();
    follow::create(&pool, attacker.id, alice.id, None)
        .await
        .unwrap();
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut forged = remote_note(&mallory, &uri, "");
    forged["name"] = json!("yes");
    forged["inReplyTo"] = poll_entity["uri"].clone();
    let vote = remote_create(&mallory, &uri, forged, "forged-known-vote");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &vote, &mallory.signer()).await,
        StatusCode::FORBIDDEN
    );
    let votes: i64 = sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE poll_id=$1")
        .bind(before.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(votes, 0);
    let after = plamenu_db::poll::find_by_status(&pool, poll_status_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.cached_tallies, before.cached_tallies);
    assert_eq!(after.voters_count, before.voters_count);
    assert_eq!(after.updated_at, before.updated_at);
    let jobs_after: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        jobs_after, jobs_before,
        "forged known URI must not distribute a poll Update"
    );
    let preserved = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(preserved.account_id, owned.account_id);
    assert_eq!(preserved.content, owned.content);
    assert_eq!(preserved.in_reply_to_id, owned.in_reply_to_id);
    // Both signers see an absent URI before storage. The real Bob Create
    // queues first, then the vote-shaped Mallory Create must recheck its owner
    // after the shared guard admits the reserved row.
    let race_uri = format!("{}/notes/vote-admission-race", bob.actor.id);
    let held = state.remote_status_writes.lock(&race_uri).await.unwrap();
    let real = remote_create(
        &bob,
        &race_uri,
        remote_note(&bob, &race_uri, "<p>Bob wins reservation</p>"),
        "reserve-before-vote",
    );
    let real_signer = bob.signer();
    let real_app = app.clone();
    let real_request =
        tokio::spawn(async move { post_signed(real_app, "/inbox", &real, &real_signer).await });
    common::wait_remote_status_writers(&state, &race_uri, 2).await;
    assert!(
        status::find_by_uri(&pool, &race_uri)
            .await
            .unwrap()
            .is_none()
    );
    let mut race_object = remote_note(&mallory, &race_uri, "");
    race_object["name"] = json!("yes");
    race_object["inReplyTo"] = poll_entity["uri"].clone();
    let race_vote = remote_create(&mallory, &race_uri, race_object, "queued-foreign-vote");
    let attacker_signer = mallory.signer();
    let vote_request =
        tokio::spawn(async move { post_signed(app, "/inbox", &race_vote, &attacker_signer).await });
    common::wait_remote_status_writers(&state, &race_uri, 3).await;
    drop(held);
    let (created, rejected) = tokio::join!(real_request, vote_request);
    assert_eq!(created.unwrap(), StatusCode::ACCEPTED);
    assert_eq!(rejected.unwrap(), StatusCode::FORBIDDEN);
    let raced = status::find_by_uri(&pool, &race_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raced.account_id, owned.account_id);
    let vote_count: i64 = sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE poll_id=$1")
        .bind(before.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(vote_count, 0);
    let final_poll = plamenu_db::poll::find_by_status(&pool, poll_status_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_poll.cached_tallies, before.cached_tallies);
    assert_eq!(final_poll.updated_at, before.updated_at);
    let jobs_final: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs_final, jobs_before);
}

async fn complete_remote_video_snapshot(pool: &PgPool, status_id: i64) -> (Value, Value, Value) {
    let row: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM statuses s WHERE id=$1")
        .bind(status_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let media: Value = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY m.id),'[]'::jsonb) FROM media_attachments m WHERE status_id=$1")
        .bind(status_id).fetch_one(pool).await.unwrap();
    let edits: Value = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(e) ORDER BY e.id),'[]'::jsonb) FROM status_edits e WHERE status_id=$1")
        .bind(status_id).fetch_one(pool).await.unwrap();
    (row, media, edits)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signed_unstamped_video_updates_clear_accessibility_without_snapshot_rollback(
    pool: PgPool,
) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let eve = RemoteUser::new("remote.example", "eve");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([bob.actor.clone(), eve.actor.clone()]),
    );
    let app = plamenu::build_router(state.clone());
    let uri = format!("{}/videos/unstamped-accessibility", bob.actor.id);
    let mut object = remote_note(&bob, &uri, "<p>initial film</p>");
    object["type"] = json!("Video");
    object["name"] = json!("Film");
    object["url"] = json!([{"type":"Link","mediaType":"application/x-mpegURL","href":"https://remote.example/unstamped.m3u8","tag":[{"type":"Link","mediaType":"video/mp4","href":"https://remote.example/unstamped.mp4","height":360}]}]);
    object["transcript"] = json!("Original speech");
    object["visualDescription"] = json!("Original scene");
    object["audioDescribed"] = json!(true);
    let original = object.clone();
    let create = remote_create(&bob, &uri, object.clone(), "unstamped-video-create");
    assert_eq!(
        post_signed(app.clone(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let row = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    let media_id = media::for_statuses_read_only(&pool, &[row.id])
        .await
        .unwrap()
        .remove(&row.id)
        .unwrap()[0]
        .id;
    object["content"] = json!("<p>timestamped edit</p>");
    object["updated"] = json!("2026-06-01T13:00:00Z");
    let mut update = json!({"@context":"https://www.w3.org/ns/activitystreams", "type":"Update", "actor":bob.actor.id, "object":object, "id":format!("{uri}#stamped")});
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    update["object"].as_object_mut().unwrap().remove("updated");
    update["object"]["content"] = json!("<p>unstamped revision</p>");
    update["object"]["transcript"] = json!("Revised speech");
    update["object"]["audioDescribed"] = json!(false);
    update["object"]["visualsConveyedInAudio"] = json!(true);
    update["id"] = json!(format!("{uri}#unstamped-one"));
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let revised = media::for_statuses_read_only(&pool, &[row.id])
        .await
        .unwrap()
        .remove(&row.id)
        .unwrap()
        .remove(0);
    assert_eq!(revised.id, media_id);
    assert_eq!(revised.transcript.as_deref(), Some("Revised speech"));
    assert!(!revised.audio_described && revised.visuals_conveyed_in_audio);
    update["object"]["content"] = json!("<p>current film</p>");
    update["object"]["transcript"] = Value::Null;
    update["object"]["visualDescription"] = Value::Null;
    update["object"]["visualsConveyedInAudio"] = json!(false);
    update["id"] = json!(format!("{uri}#unstamped-two"));
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let current = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(current.content, "<p>current film</p>");
    let cleared = media::for_statuses_read_only(&pool, &[row.id])
        .await
        .unwrap()
        .remove(&row.id)
        .unwrap()
        .remove(0);
    assert_eq!(cleared.id, media_id);
    assert!(
        cleared.transcript.is_none()
            && cleared.description.is_none()
            && !cleared.audio_described
            && !cleared.visuals_conveyed_in_audio
    );
    let canonical = complete_remote_video_snapshot(&pool, row.id).await;
    assert_eq!(
        canonical.2.as_array().unwrap().len(),
        4,
        "initial plus three legitimate edits"
    );
    // Identical signed delivery remains a no-change Update and cannot add history.
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        complete_remote_video_snapshot(&pool, row.id).await,
        canonical
    );
    for (index, timestamp) in [json!("2020-01-01T00:00:00Z"), Value::Null, json!("invalid")]
        .into_iter()
        .enumerate()
    {
        update["id"] = json!(format!("{uri}#rejected-{index}"));
        update["object"] = original.clone();
        update["object"]["updated"] = timestamp;
        assert_eq!(
            post_signed(app.clone(), "/inbox", &update, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            complete_remote_video_snapshot(&pool, row.id).await,
            canonical
        );
    }
    // Same-origin foreign authors are still unable to overwrite the known owner.
    update["id"] = json!(format!("{}/activities/foreign-unstamped", eve.actor.id));
    update["actor"] = json!(eve.actor.id);
    update["object"] = original.clone();
    update["object"]["attributedTo"] = json!(eve.actor.id);
    assert_eq!(
        post_signed(app.clone(), "/inbox", &update, &eve.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        complete_remote_video_snapshot(&pool, row.id).await,
        canonical
    );
    let author = plamenu_db::account::find_by_id(&pool, row.account_id)
        .await
        .unwrap()
        .unwrap();
    for context in [
        plamenu::ingest::RemoteIngestContext::History,
        plamenu::ingest::RemoteIngestContext::ExplicitResolution,
    ] {
        plamenu::ingest::update_remote_note_in_context(
            &state, &author, &current, &original, context,
        )
        .await
        .unwrap();
        assert_eq!(
            complete_remote_video_snapshot(&pool, row.id).await,
            canonical
        );
    }
    // A replayed original Create keeps all canonical data, even after unstamped edits.
    assert_eq!(
        post_signed(app, "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        complete_remote_video_snapshot(&pool, row.id).await,
        canonical
    );
}
