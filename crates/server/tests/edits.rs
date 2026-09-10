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
    let app = || test_app_with(pool.clone(), stub.clone());
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

    // No-change edits leave no pending deliveries behind either.
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
}
