//! Profile identity (M6) integration tests: `update_credentials`, actor
//! documents with avatar/header/fields, outbound `Update(Actor)` fan-out,
//! and inbound `Update` of actors and notes.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::profile::{ProfileChanges, ProfileImage, clear_profile_image, update_profile};
use plamenu::{delivery, remote};
use plamenu_db::account::Account;
use plamenu_db::notification::NotificationFilter;
use plamenu_db::{PgPool, account, follow, notification, status, tag, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

const ALICE_URI: &str = "https://plamenu.test/users/alice";

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
            "client_name": "profile-tests",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    let client_id = app_response.1["client_id"].as_str().unwrap().to_owned();
    let client_secret = app_response.1["client_secret"].as_str().unwrap().to_owned();
    let auth_request = Request::builder()
        .method("POST")
        .uri("/oauth/authorize")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            serde_urlencoded::to_string([
                ("client_id", client_id.as_str()),
                ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                ("scope", "read write"),
                ("email", &email),
                ("password", "pw"),
            ])
            .unwrap(),
        ))
        .unwrap();
    let auth_response = test_app(pool.clone()).oneshot(auth_request).await.unwrap();
    let auth_page = String::from_utf8_lossy(
        &auth_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .into_owned();
    let code = auth_page
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

async fn fetch_actor_doc(app: Router, username: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/users/{username}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_credentials_changes_profile_and_federates(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    // Bob follows alice, so the profile change must reach his inbox.
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let stored_bob = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let alice_row = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(&pool, stored_bob.id, alice_row.id, None)
        .await
        .unwrap();

    let app = || test_app_with(pool.clone(), stub.clone());
    let (status, body) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({
            "display_name": "Alice in Chains",
            "note": "I write #rust",
            "discoverable": true,
            "bot": true,
            "indexable": true,
            "hide_collections": true,
            "fields_attributes": [
                {"name": "Website", "value": "https://alice.example"},
                {"name": "Pronouns", "value": "she/her"},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "Alice in Chains");
    assert_eq!(body["discoverable"], true);
    assert_eq!(body["bot"], true);
    assert_eq!(body["indexable"], true);
    assert_eq!(body["hide_collections"], true);
    assert_eq!(body["source"]["indexable"], true);
    assert_eq!(body["source"]["hide_collections"], true);
    // The bio is rendered (hashtag linked), the source stays raw.
    let note = body["note"].as_str().unwrap();
    assert!(
        note.starts_with("<p>") && note.contains("/tags/rust"),
        "{note}"
    );
    assert_eq!(body["source"]["note"], "I write #rust");
    assert_eq!(
        body["source"]["fields"][0]["value"],
        "https://alice.example"
    );
    // Whole-URL field values become links, plain ones stay text.
    let field_value = body["fields"][0]["value"].as_str().unwrap();
    assert!(field_value.starts_with("<a href"), "{field_value}");
    assert_eq!(body["fields"][1]["value"], "she/her");
    assert!(body["fields"][0]["verified_at"].is_null());

    // The actor document carries the same identity.
    let actor = fetch_actor_doc(app(), "alice").await;
    assert_eq!(actor["type"], "Service");
    assert_eq!(actor["name"], "Alice in Chains");
    assert_eq!(actor["discoverable"], true);
    assert_eq!(actor["indexable"], true);
    assert!(actor["summary"].as_str().unwrap().contains("/tags/rust"));
    assert_eq!(actor["attachment"][0]["type"], "PropertyValue");
    assert_eq!(actor["attachment"][0]["name"], "Website");

    // An Update(Actor) was queued for bob's inbox.
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(delivery::run_due(&state).await, 1);
    let deliveries = stub.deliveries();
    assert_eq!(deliveries.len(), 1);
    let update = &deliveries[0];
    assert_eq!(update.inbox_url, "https://remote.example/inbox");
    assert_eq!(update.activity["type"], "Update");
    assert_eq!(update.activity["actor"], ALICE_URI);
    assert_eq!(update.activity["object"]["id"], ALICE_URI);
    assert_eq!(update.activity["object"]["type"], "Service");
    assert_eq!(update.activity["object"]["name"], "Alice in Chains");
    assert_eq!(update.activity["object"]["discoverable"], true);
    assert_eq!(update.activity["object"]["indexable"], true);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_tab_settings_persist_federate_and_gate_by_endpoint(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // `update_credentials` silently drops the profile-tab params (Mastodon
    // permits them only on `/api/v1/profile`).
    let (status, body) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "show_media": false, "show_featured": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["show_media"], true, "credentials endpoint ignores it");
    assert_eq!(body["show_featured"], true);

    // `/api/v1/profile` accepts and persists them.
    let (status, body) = api(
        app(),
        "PATCH",
        "/api/v1/profile",
        Some(&token),
        Some(json!({
            "show_media": false,
            "show_media_replies": false,
            "show_featured": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["show_media"], false);
    assert_eq!(body["show_media_replies"], false);
    assert_eq!(body["show_featured"], false);

    // The Account entity (verify_credentials) reflects the stored values and
    // carries the implicit "Everyone" role.
    let (status, body) = api(
        app(),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["show_media"], false);
    assert_eq!(body["show_featured"], false);
    // Local accounts carry `noindex` (default false) and an empty highlighted
    // `roles` array; the credential entity's `role` is the Everyone fallback.
    assert_eq!(body["noindex"], false);
    assert_eq!(body["roles"], json!([]));
    assert!(body["role"].is_object(), "credential role present");
    assert!(
        body.get("memorial").is_none(),
        "memorial omitted when unset"
    );

    // The actor document federates the profile-tab settings under the toot
    // context terms.
    let actor = fetch_actor_doc(app(), "alice").await;
    assert_eq!(actor["showMedia"], false);
    assert_eq!(actor["showRepliesInMedia"], false);
    assert_eq!(actor["showFeatured"], false);
    assert_eq!(actor["memorial"], false);
}

fn sample_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        600,
        600,
        image::Rgb([200, 30, 30]),
    ));
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_credentials_multipart_uploads_avatar(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    // One router for the whole test: the in-memory media store must be
    // shared between the upload and the later /media/ fetch.
    let shared_app = test_app(pool.clone());
    let app = || shared_app.clone();

    let boundary = "PlamenuTestBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"display_name\"\r\n\r\nPic Alice\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&sample_png());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let request = Request::builder()
        .method("PATCH")
        .uri("/api/v1/accounts/update_credentials")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(entity["display_name"], "Pic Alice");
    let avatar = entity["avatar"].as_str().unwrap();
    assert!(
        avatar.starts_with("https://plamenu.test/media/"),
        "{avatar}"
    );

    // The re-encoded avatar is a Mastodon-compatible photo (JPEG for this
    // opaque source) and actually served — avatars are never AVIF, which
    // Mastodon rejects. The 400-edge downscale is covered by the
    // media_processing unit tests.
    let file_name = avatar.rsplit('/').next().unwrap();
    assert_eq!(
        std::path::Path::new(file_name)
            .extension()
            .and_then(|ext| ext.to_str()),
        Some("jpg"),
        "{file_name}"
    );
    let request = Request::builder()
        .method("GET")
        .uri(format!("/media/{file_name}"))
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
    let served = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        image::guess_format(&served).unwrap(),
        image::ImageFormat::Jpeg
    );

    // The actor document advertises the avatar as its icon.
    let actor = fetch_actor_doc(app(), "alice").await;
    assert_eq!(actor["icon"]["type"], "Image");
    assert_eq!(actor["icon"]["url"], avatar);
    assert!(actor.get("image").is_none(), "no header was uploaded");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_api_show_update_and_clear_image(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let shared_app = test_app(pool.clone());
    let app = || shared_app.clone();

    // PATCH /api/v1/profile takes update_credentials params and returns the
    // Profile entity: raw `note`, HTML `formatted_note`, alt text.
    let (status, body) = api(
        app(),
        "PATCH",
        "/api/v1/profile",
        Some(&token),
        Some(json!({
            "display_name": "Profile Alice",
            "note": "I write #rust",
            "avatar_description": "a smiling avatar",
            "header_description": "a mountain range",
            "indexable": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "Profile Alice");
    assert_eq!(body["note"], "I write #rust", "raw source note");
    let formatted = body["formatted_note"].as_str().unwrap();
    assert!(
        formatted.starts_with("<p>") && formatted.contains("/tags/rust"),
        "{formatted}"
    );
    assert_eq!(body["avatar_description"], "a smiling avatar");
    assert_eq!(body["header_description"], "a mountain range");
    assert_eq!(body["indexable"], true);
    // Unmodelled 4.6 settings carry Mastodon's defaults.
    assert_eq!(body["show_media"], true);
    assert_eq!(body["attribution_domains"], json!([]));
    assert_eq!(body["featured_tags"], json!([]));

    // GET reflects it.
    let (status, body) = api(app(), "GET", "/api/v1/profile", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["avatar_description"], "a smiling avatar");

    // Upload an avatar so there is something to clear.
    let boundary = "PlamenuProfileBoundary";
    let mut up = Vec::new();
    up.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    up.extend_from_slice(&sample_png());
    up.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("PATCH")
        .uri("/api/v1/accounts/update_credentials")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(up))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        entity["avatar"].as_str().unwrap().contains("/media/"),
        "avatar uploaded"
    );

    // DELETE /api/v1/profile/avatar reverts to the placeholder and clears the
    // alt text, returning a CredentialAccount (it carries `source`).
    let (status, body) = api(
        app(),
        "DELETE",
        "/api/v1/profile/avatar",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["avatar"]
            .as_str()
            .unwrap()
            .ends_with("/static/missing.png"),
        "{}",
        body["avatar"]
    );
    assert_eq!(body["avatar_description"], "");
    assert!(body["source"].is_object(), "credential account shape");

    // The header description survives an avatar clear.
    let (_, body) = api(app(), "GET", "/api/v1/profile", Some(&token), None).await;
    assert_eq!(body["header_description"], "a mountain range");
}

/// The profile editor as masto.js drives it: PUT (not PATCH) with a
/// `multipart/form-data` body, and `fields_attributes` as a numerically
/// indexed map rather than an array. Mastodon routes `/api/v1/profile` as a
/// Rails `resource :profile, :update`, so it answers both verbs; we only
/// answered PATCH, and Phanpy's "Edit profile" got a bare 405.
#[sqlx::test(migrations = "../db/migrations")]
async fn profile_api_accepts_multipart_put_from_masto_js(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let shared_app = test_app(pool.clone());
    let app = || shared_app.clone();

    let boundary = "geckoformboundary5bd2e1160e797fb147851080b79be0e6";
    let mut body = String::new();
    for (name, value) in [
        ("display_name", "also lnkr"),
        ("note", "developing and testing a new thing.\nEn, Ru, MDNI"),
        ("avatar_description", "perhaps"),
        ("fields_attributes[0][name]", "Site"),
        ("fields_attributes[0][value]", "https://example.com/"),
    ] {
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    write!(body, "--{boundary}--\r\n").unwrap();

    let request = Request::builder()
        .method("PUT")
        .uri("/api/v1/profile")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["display_name"], "also lnkr");
    assert_eq!(entity["avatar_description"], "perhaps");
    assert_eq!(entity["fields"][0]["name"], "Site");
    assert_eq!(entity["fields"][0]["value"], "https://example.com/");
    // The Profile entity, not a CredentialAccount: raw note plus HTML.
    assert!(
        entity["formatted_note"]
            .as_str()
            .unwrap()
            .starts_with("<p>")
    );
    assert!(entity["note"].as_str().unwrap().contains("MDNI"));

    // GET reflects the write, so the editor reopens with what was saved.
    let (status, body) = api(app(), "GET", "/api/v1/profile", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "also lnkr");
    assert_eq!(body["fields"][0]["name"], "Site");
}

/// Replacing or clearing a local avatar/header must remove the
/// superseded file from the media store. These images have no
/// `media_attachments` row, so a leaked file is unreachable by any orphan
/// sweep and stays publicly fetchable via `/media/{file}`. Removal is durable:
/// the update queues the old key on `media_cleanup_jobs` and the leased worker
/// deletes it (retrying transient store errors), so the test drains the worker
/// between steps.
#[sqlx::test(migrations = "../db/migrations")]
async fn replacing_or_clearing_profile_images_deletes_superseded_files(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let drain_cleanup =
        || async { while plamenu::media_cleanup_worker::run_due(&state).await > 0 {} };

    // Upload an initial avatar and header.
    let v1 = update_profile(
        &state,
        &alice,
        ProfileChanges {
            avatar: Some(sample_png()),
            header: Some(sample_png()),
            ..ProfileChanges::default()
        },
    )
    .await
    .unwrap();
    let avatar_v1 = v1.avatar_file_name.clone().expect("avatar stored");
    let header_v1 = v1.header_file_name.clone().expect("header stored");
    assert!(
        state.media.get(&avatar_v1).await.is_ok(),
        "avatar v1 present"
    );
    assert!(
        state.media.get(&header_v1).await.is_ok(),
        "header v1 present"
    );

    // Replace only the avatar: the old avatar file is deleted, the new one is
    // present, and the untouched header file survives.
    let v2 = update_profile(
        &state,
        &v1,
        ProfileChanges {
            avatar: Some(sample_png()),
            ..ProfileChanges::default()
        },
    )
    .await
    .unwrap();
    let avatar_v2 = v2.avatar_file_name.clone().expect("new avatar stored");
    assert_ne!(avatar_v1, avatar_v2, "a fresh key is minted");
    drain_cleanup().await;
    assert!(
        state.media.get(&avatar_v1).await.is_err(),
        "superseded avatar deleted"
    );
    assert!(
        state.media.get(&avatar_v2).await.is_ok(),
        "avatar v2 present"
    );
    assert!(
        state.media.get(&header_v1).await.is_ok(),
        "untouched header kept"
    );

    // Clearing the header deletes its file; the untouched avatar survives.
    let cleared = clear_profile_image(&state, &v2, ProfileImage::Header)
        .await
        .unwrap();
    assert!(cleared.header_file_name.is_none(), "header column cleared");
    drain_cleanup().await;
    assert!(
        state.media.get(&header_v1).await.is_err(),
        "cleared header deleted"
    );
    assert!(
        state.media.get(&avatar_v2).await.is_ok(),
        "avatar kept after header clear"
    );
}

/// An image stored ahead of a profile update whose database
/// write then fails is compensated — the stray file is queued for durable
/// cleanup instead of leaking with no database reference.
#[sqlx::test(migrations = "../db/migrations")]
async fn failed_profile_update_cleans_up_the_stored_image(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    // A phantom account: the image is processed and stored, then the update
    // matches no row (`Ok(None)`), which must trigger the compensation path.
    let mut phantom = alice.clone();
    phantom.id += 1;
    let error = update_profile(
        &state,
        &phantom,
        ProfileChanges {
            avatar: Some(sample_png()),
            ..ProfileChanges::default()
        },
    )
    .await
    .expect_err("phantom account cannot be updated");
    assert!(matches!(error, plamenu::error::ApiError::NotFound));

    // The stored file was queued; draining the worker removes it, leaving the
    // store empty.
    while plamenu::media_cleanup_worker::run_due(&state).await > 0 {}
    assert!(
        state.media.list().await.unwrap().is_empty(),
        "no stray files survive a failed update"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_credentials_form_fields_and_validation(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // Rails-style bracket params (what form-encoded clients send).
    let form = "display_name=Form+Alice\
                &fields_attributes%5B0%5D%5Bname%5D=Lang\
                &fields_attributes%5B0%5D%5Bvalue%5D=Rust";
    let request = Request::builder()
        .method("PATCH")
        .uri("/api/v1/accounts/update_credentials")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["display_name"], "Form Alice");
    assert_eq!(entity["fields"][0]["name"], "Lang");
    assert_eq!(entity["fields"][0]["value"], "Rust");

    // Limits are enforced with 422s, like Mastodon.
    let too_long_name = "x".repeat(31);
    let (status, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "display_name": too_long_name })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let five_fields: Vec<Value> = (0..5)
        .map(|i| json!({"name": format!("f{i}"), "value": "v"}))
        .collect();
    let (status, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "fields_attributes": five_fields })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // No token: 401.
    let (status, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        None,
        Some(json!({ "display_name": "nope" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_credentials_source_params_change_posting_defaults(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // JSON body: a nested `source` object, like Mastodon's user_params.
    let (status, entity) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "source": {
            "privacy": "unlisted",
            "sensitive": true,
            "language": "de",
            "quote_policy": "followers",
            "show_application": false,
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    assert_eq!(entity["source"]["privacy"], "unlisted");
    assert_eq!(entity["source"]["sensitive"], true);
    assert_eq!(entity["source"]["language"], "de");
    assert_eq!(entity["source"]["quote_policy"], "followers");
    assert_eq!(entity["source"]["show_application"], false);

    // Form body: Rails-style bracket params. Omitted keys keep their values.
    let form = "source%5Bquote_policy%5D=nobody";
    let request = Request::builder()
        .method("PATCH")
        .uri("/api/v1/accounts/update_credentials")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["source"]["quote_policy"], "nobody");
    assert_eq!(entity["source"]["privacy"], "unlisted", "kept");
    assert_eq!(entity["source"]["show_application"], false, "kept");

    // The stored preference now feeds a param-less post.
    let (status, posted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({ "status": "uses my defaults" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(posted["visibility"], "unlisted");
    assert_eq!(posted["quote_approval"]["automatic"], json!([]));

    // Unknown enum values are 422s, like Mastodon's `in:` validations.
    for body in [
        json!({ "source": { "quote_policy": "everyone" } }),
        json!({ "source": { "privacy": "secret" } }),
    ] {
        let (status, _) = api(
            app(),
            "PATCH",
            "/api/v1/accounts/update_credentials",
            Some(&token),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_update_actor_refreshes_the_profile(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#updates/1", bob.actor.id),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": bob.actor.id,
            "type": "Person",
            "preferredUsername": "bob",
            "inbox": bob.actor.inbox,
            "name": "Bobby",
            "summary": "<p>hi</p><script>alert(1)</script>",
            "published": "2020-02-03T04:05:06Z",
            "url": "https://remote.example/@bob",
            "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.example/avatar.png", "summary": "Bob <b>smiling</b>"},
            "image": {"type": "Image", "url": "https://remote.example/header.png", "summary": "A <i>blue</i> sky"},
            "attachment": [
                {"type": "PropertyValue", "name": "Site", "value": "<a href=\"https://bob.example\">bob.example</a>"},
            ],
            "publicKey": {
                "id": bob.actor.public_key.id,
                "owner": bob.actor.id,
                "publicKeyPem": bob.actor.public_key.public_key_pem,
            },
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.display_name, "Bobby");
    assert_eq!(stored.note, "<p>hi</p>", "scripts are sanitized away");
    assert_eq!(
        stored.url.as_deref(),
        Some("https://remote.example/@bob"),
        "the actor's published web url is captured"
    );
    assert_eq!(
        stored.avatar_remote_url.as_deref(),
        Some("https://remote.example/avatar.png")
    );
    assert_eq!(
        stored.header_remote_url.as_deref(),
        Some("https://remote.example/header.png")
    );
    assert_eq!(stored.avatar_description, "Bob smiling");
    assert_eq!(stored.header_description, "A blue sky");
    assert_eq!(
        stored.created_at,
        OffsetDateTime::parse("2020-02-03T04:05:06Z", &Rfc3339).unwrap(),
        "the origin's account creation time is not replaced with ingest time"
    );
    assert_eq!(stored.fields[0]["name"], "Site");
    assert!(
        stored.fields[0]["value"].as_str().unwrap().contains("<a"),
        "links in field values survive sanitization"
    );
}

/// `attribution_domains` round-trip: the `update_credentials` param persists
/// (normalized), echoes in `source`, rides the actor document under
/// `attributionDomains`, and a remote actor's declaration is stored on
/// ingest.
#[sqlx::test(migrations = "../db/migrations")]
async fn attribution_domains_persist_federate_and_ingest(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let (status, body) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({
            "attribution_domains": ["https://News.example/", "*.blogs.example", "news.example"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["source"]["attribution_domains"],
        json!(["news.example", "blogs.example"]),
        "scheme/wildcard stripped, lowercased, deduped"
    );
    let stored = account::attribution_domains(&pool, alice.id).await.unwrap();
    assert_eq!(stored, ["news.example", "blogs.example"]);

    // A bad domain is a 422, like Mastodon's `domain: true` validation.
    let (status, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "attribution_domains": ["not a domain"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The actor document advertises the list.
    let actor = fetch_actor_doc(app(), "alice").await;
    assert_eq!(
        actor["attributionDomains"],
        json!(["news.example", "blogs.example"])
    );

    // Inbound: a remote actor's declaration is stored (and normalized).
    let bob = RemoteUser::new("remote.example", "bob");
    let mut remote_actor = bob.actor.clone();
    remote_actor.attribution_domains = vec![json!("https://bob.example")];
    let stored_bob = remote::store_remote_actor(&pool, &remote_actor)
        .await
        .unwrap();
    assert_eq!(
        account::attribution_domains(&pool, stored_bob.id)
            .await
            .unwrap(),
        ["bob.example"]
    );
}

/// Mastodon's `set_suspension!` semantics on inbound actor documents: a
/// `suspended: true` marker suspends with origin `remote` while freezing the
/// stored profile at its pre-suspension values; the marker disappearing lifts
/// the suspension; and a locally-imposed suspension is never altered by the
/// origin's documents.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_suspension_marker_suspends_freezes_and_lifts(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let update = |seq: u32, name: &str, suspended: bool| {
        let mut object = json!({
            "id": bob.actor.id,
            "type": "Person",
            "preferredUsername": "bob",
            "inbox": bob.actor.inbox,
            "name": name,
            "publicKey": {
                "id": bob.actor.public_key.id,
                "owner": bob.actor.id,
                "publicKeyPem": bob.actor.public_key.public_key_pem,
            },
        });
        if suspended {
            object["suspended"] = json!(true);
        }
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{}#updates/{seq}", bob.actor.id),
            "type": "Update",
            "actor": bob.actor.id,
            "object": object,
        })
    };

    // Establish bob with a normal profile.
    let first = update(1, "Bobby", false);
    assert_eq!(
        post_signed(app(), "/inbox", &first, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.suspended());
    assert_eq!(stored.display_name, "Bobby");

    // The origin reports the account suspended (blanked document): mirror
    // it with origin `remote`, but keep the stored profile frozen.
    let suspended = update(2, "bob", true);
    assert_eq!(
        post_signed(app(), "/inbox", &suspended, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.suspended());
    assert_eq!(stored.suspension_origin.as_deref(), Some("remote"));
    assert_eq!(
        stored.display_name, "Bobby",
        "profile attributes freeze at their pre-suspension values"
    );

    // The origin stops reporting the suspension: it lifts, and attribute
    // updates flow again.
    let lifted = update(3, "Bobbie", false);
    assert_eq!(
        post_signed(app(), "/inbox", &lifted, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.suspended());
    assert!(stored.suspension_origin.is_none());
    assert_eq!(stored.display_name, "Bobbie");

    // A locally-imposed suspension out-ranks the origin: neither the
    // suspension nor the profile moves on further updates.
    account::suspend(&pool, stored.id, "local").await.unwrap();
    let ignored = update(4, "Robert", false);
    assert_eq!(
        post_signed(app(), "/inbox", &ignored, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.suspended());
    assert_eq!(stored.suspension_origin.as_deref(), Some("local"));
    assert_eq!(stored.display_name, "Bobbie");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_update_actor_ignores_foreign_objects(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);

    // Bob (verified) tries to update carol's profile.
    let carol_uri = "https://remote.example/users/carol";
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#updates/2", bob.actor.id),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": carol_uri,
            "type": "Person",
            "preferredUsername": "carol",
            "inbox": format!("{carol_uri}/inbox"),
            "name": "Hijacked",
            "publicKey": {
                "id": format!("{carol_uri}#main-key"),
                "owner": carol_uri,
                "publicKeyPem": "PEM",
            },
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            "/inbox",
            &update,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        account::find_by_uri(&pool, carol_uri)
            .await
            .unwrap()
            .is_none(),
        "a foreign Update must not create or change accounts"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_update_note_applies_the_edit(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let note_uri = format!("{}/notes/1", bob.actor.id);

    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>first draft</p>",
            "published": "2026-06-01T12:00:00Z",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // The edit changes the text, adds a hashtag and mentions alice.
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>final version</p><script>x</script>",
            "summary": "on second thought",
            "sensitive": true,
            "published": "2026-06-01T12:00:00Z",
            "updated": "2026-06-01T13:00:00Z",
            "tag": [
                {"type": "Hashtag", "name": "#rust"},
                {"type": "Mention", "href": ALICE_URI, "name": "@alice@plamenu.test"},
            ],
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "<p>final version</p>");
    // The same edit also adds a content warning and flips sensitive on.
    assert_eq!(stored.spoiler_text, "on second thought");
    assert!(stored.sensitive);
    let edited_at = stored.edited_at.expect("edit must be recorded");
    assert_eq!(
        edited_at.unix_timestamp(),
        time::OffsetDateTime::parse(
            "2026-06-01T13:00:00Z",
            &time::format_description::well_known::Rfc3339
        )
        .unwrap()
        .unix_timestamp()
    );
    let tags = tag::names_for_statuses(&pool, &[stored.id]).await.unwrap();
    assert_eq!(tags[&stored.id], ["rust"]);
    // Count with `include_filtered` so this test stays about edit-driven
    // re-notification even if the recipient's private-mention policy changes.
    let filtered = NotificationFilter {
        include_filtered: true,
        ..Default::default()
    };
    let mentions = notification::list(&pool, alice.id, None, None, None, filtered, 10)
        .await
        .unwrap();
    assert_eq!(mentions.len(), 1, "the new mention notifies once");
    assert_eq!(mentions[0].kind, "mention");

    // Redelivering the same edit must not re-notify.
    assert_eq!(
        post_signed(app(), "/inbox", &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let mentions = notification::list(&pool, alice.id, None, None, None, filtered, 10)
        .await
        .unwrap();
    assert_eq!(mentions.len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_update_note_ignores_foreign_and_unknown_notes(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let carol = RemoteUser::new("remote.example", "carol");
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // Carol posts a note…
    let note_uri = format!("{}/notes/9", carol.actor.id);
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": carol.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": carol.actor.id,
            "content": "<p>carol's words</p>",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &create, &carol.signer()).await,
        StatusCode::ACCEPTED
    );

    // …and bob (verified, but not the author) tries to edit it.
    let forged = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>defaced</p>",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &forged, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "<p>carol's words</p>");
    assert!(stored.edited_at.is_none());

    // An Update for a note we never stored is not a Create.
    let unknown_uri = format!("{}/notes/404", bob.actor.id);
    let unknown = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{unknown_uri}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "object": {
            "id": unknown_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>ghost</p>",
        },
    });
    assert_eq!(
        post_signed(app(), "/inbox", &unknown, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &unknown_uri)
            .await
            .unwrap()
            .is_none()
    );
}

/// FEP-521a: once an account holds an Ed25519 pair (creation or the
/// startup backfill), its actor document publishes the Multikey under
/// `assertionMethod`. The RSA signing key is mirrored there too, under the
/// `#main-key` id its `publicKey` block already uses, so a peer that keeps
/// only FEP-521a verification methods — Mastodon 4.7 builds from the
/// 2026-06-19…07-06 window (mastodon#39725) discard `publicKey` outright —
/// can still resolve the keyId our signatures name.
#[sqlx::test(migrations = "../db/migrations")]
async fn actor_document_publishes_the_ed25519_assertion_method(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let stored = plamenu_db::actor_key::usable_for_account(&pool, alice.id)
        .await
        .unwrap();
    let ed25519_public = stored
        .iter()
        .find(|key| key.algorithm == "ed25519")
        .unwrap()
        .public_key
        .clone();
    let doc = fetch_actor_doc(test_app(pool.clone()), "alice").await;
    let rsa_mirror = serde_json::json!({
        "id": "https://plamenu.test/users/alice#main-key",
        "type": "Multikey",
        "controller": "https://plamenu.test/users/alice",
        "publicKeyMultibase": plamenu_ap::multikey::encode_rsa_public(
            doc["publicKey"]["publicKeyPem"].as_str().unwrap(),
        )
        .unwrap(),
    });
    assert_eq!(
        doc["assertionMethod"],
        serde_json::json!([
            {
                "id": "https://plamenu.test/users/alice#ed25519-key",
                "type": "Multikey",
                "controller": "https://plamenu.test/users/alice",
                "publicKeyMultibase": ed25519_public,
            },
            rsa_mirror,
        ]),
        "Ed25519 first (the FEP-8b32 proof key), RSA mirror after"
    );
    // The legacy RSA block stays alongside.
    assert!(
        doc["publicKey"]["publicKeyPem"]
            .as_str()
            .unwrap()
            .contains("BEGIN PUBLIC KEY")
    );
    assert_eq!(
        doc["assertionMethod"][1]["id"], doc["publicKey"]["id"],
        "the mirror must carry the very id our signatures name"
    );
}

fn identity_statement(uri: &str) -> Value {
    let pair = plamenu_ap::keys::generate_ed25519_keypair();
    let did = format!("did:key:{}", pair.public_multibase);
    plamenu_ap::proof::sign_document(
        &json!({"type":"VerifiableIdentityStatement", "subject":did, "alsoKnownAs":uri, "label":"preserved extension"}),
        &pair.private_multibase, &did, "2026-01-01T00:00:00Z",
    ).unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn identity_proofs_api_publishes_replaces_removes_and_rejects_invalid(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let app = test_app(pool.clone());
    let path = "/api/v1/accounts/identity_statements";
    let read_path = format!("/api/v1/accounts/{}/identity_statements", alice.id);
    let granted = plamenu_db::oauth::find_active_token(&pool, &plamenu::auth::hash_secret(&token))
        .await
        .unwrap()
        .unwrap();
    let read_token = "identity-read-only-test-token";
    plamenu_db::oauth::create_token(
        &pool,
        &plamenu::auth::hash_secret(read_token),
        granted.app_id,
        granted.user_id,
        "read",
    )
    .await
    .unwrap();
    for method in ["POST", "DELETE"] {
        let input = if method == "POST" {
            identity_statement(ALICE_URI)
        } else {
            json!({"subject":"did:key:unused"})
        };
        assert_eq!(
            api(app.clone(), method, path, Some(read_token), Some(input))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }

    let proof = identity_statement(ALICE_URI);
    assert_eq!(
        api(app.clone(), "POST", path, None, Some(proof.clone()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut bad = proof.clone();
    bad["alsoKnownAs"] = json!("https://other.example/users/alice");
    assert_eq!(
        api(app.clone(), "POST", path, Some(&token), Some(bad))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for _ in 0..2 {
        let (status, body) =
            api(app.clone(), "POST", path, Some(&token), Some(proof.clone())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!([proof]));
    }
    assert_eq!(
        api(app.clone(), "GET", &read_path, None, None).await.1,
        json!([proof])
    );
    let actor = fetch_actor_doc(app.clone(), "alice").await;
    assert!(actor["attachment"].as_array().unwrap().contains(&proof));
    assert_eq!(
        api(
            app.clone(),
            "DELETE",
            path,
            Some(&token),
            Some(json!({"subject":proof["subject"]}))
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        api(app.clone(), "GET", &read_path, None, None).await.1,
        json!([])
    );
    let state = test_state_with(pool.clone(), Arc::default());
    plamenu::identity::publish(&state, alice.id, proof)
        .await
        .unwrap();
    account::suspend(&pool, alice.id, "local").await.unwrap();
    let suspended = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    assert!(
        plamenu::identity::list(&state, &suspended)
            .await
            .unwrap()
            .is_empty()
    );
    account::purge_local_data(&pool, alice.id).await.unwrap();
    assert!(
        plamenu_db::identity_proof::list(&pool, alice.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn identity_proofs_remote_refresh_discards_invalid_and_removed(pool: PgPool) {
    let bob = RemoteUser::new("remote.test", "bob");
    let mut actor = bob.actor;
    let proof = identity_statement(&actor.id);
    let unrelated = identity_statement(ALICE_URI);
    actor.attachment = vec![proof.clone(), proof.clone(), unrelated];
    let stored = remote::store_remote_actor(&pool, &actor).await.unwrap();
    assert_eq!(
        plamenu_db::identity_proof::list(&pool, stored.id)
            .await
            .unwrap(),
        vec![proof]
    );
    actor.attachment.clear();
    remote::store_remote_actor(&pool, &actor).await.unwrap();
    assert!(
        plamenu_db::identity_proof::list(&pool, stored.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn identity_proofs_outbox_failure_rolls_back(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.test", "bob");
    let remote = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), Arc::default());
    sqlx::query("CREATE FUNCTION reject_identity_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END; $$ LANGUAGE plpgsql").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_identity_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_identity_delivery()").execute(&pool).await.unwrap();
    assert!(
        plamenu::identity::publish(&state, alice.id, identity_statement(ALICE_URI))
            .await
            .is_err()
    );
    assert!(
        plamenu_db::identity_proof::list(&pool, alice.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn identity_proofs_concurrent_publications_respect_cap(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), Arc::default());
    let existing = identity_statement(ALICE_URI);
    plamenu::identity::publish(&state, alice.id, existing.clone())
        .await
        .unwrap();
    for _ in 0..8 {
        plamenu::identity::publish(&state, alice.id, identity_statement(ALICE_URI))
            .await
            .unwrap();
    }
    let (one, two) = tokio::join!(
        plamenu::identity::publish(&state, alice.id, identity_statement(ALICE_URI)),
        plamenu::identity::publish(&state, alice.id, identity_statement(ALICE_URI)),
    );
    assert_ne!(
        one.is_ok(),
        two.is_ok(),
        "Only one writer can fill the last slot"
    );
    let replaced = plamenu::identity::publish(&state, alice.id, existing.clone())
        .await
        .unwrap();
    assert_eq!(replaced.len(), 10);
    assert!(replaced.contains(&existing));
}
