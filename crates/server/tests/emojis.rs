//! Custom emoji: ingestion of remote `Emoji` tags (notes and actors),
//! REST entity rendering, the picker listing, outbound federation and the
//! `/emojis/{id}` `ActivityPub` object.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, custom_emoji, oauth, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

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
            name: "emoji-tests",
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
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(json) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// GET a web UI page as HTML.
async fn web_get(app: Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The inline `<img>` the web UI swaps in for a `:shortcode:` reference.
fn emoji_img(shortcode: &str, url: &str) -> String {
    format!(
        "<img class=\"emoji\" src=\"{url}\" alt=\":{shortcode}:\" \
         title=\":{shortcode}:\" draggable=\"false\" loading=\"lazy\">"
    )
}

/// GET with an `ActivityPub` `Accept` header.
async fn ap_get(app: Router, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post_signed(app: Router, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let headers = signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", headers.host)
        .header("date", headers.date)
        .header("digest", headers.digest)
        .header("signature", headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

fn emoji_tag_entry(shortcode: &str, image: &str, updated: &str) -> Value {
    json!({
        "id": format!("https://remote.example/emojis/{shortcode}"),
        "type": "Emoji",
        "name": format!(":{shortcode}:"),
        "updated": updated,
        "icon": { "type": "Image", "mediaType": "image/png", "url": image },
    })
}

fn create_note(bob: &RemoteUser, note_id: u64, content: &str, tag: &Value) -> Value {
    let note_uri = format!("{}/statuses/{note_id}", bob.actor.id);
    json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": content,
            "published": "2026-06-11T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "tag": tag,
        },
    })
}

// ---------------------------------------------------------------------------
// Inbound

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_note_emoji_is_stored_and_rendered(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let create = create_note(
        &bob,
        1,
        "<p>hello :blobcat: world</p>",
        &json!([emoji_tag_entry(
            "blobcat",
            "https://remote.example/files/blobcat.png",
            "2026-06-01T00:00:00Z",
        )]),
    );
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // The emoji row was recorded under the sender's domain.
    let codes = vec!["blobcat".to_owned()];
    let stored = custom_emoji::lookup(&pool, &codes, Some("remote.example"))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].image_remote_url.as_deref(),
        Some("https://remote.example/files/blobcat.png")
    );
    assert_eq!(
        stored[0].uri.as_deref(),
        Some("https://remote.example/emojis/blobcat")
    );

    // The status entity re-scans the text and carries the emoji.
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let item = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let (code, entity) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", item.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let emoji = &entity["emojis"][0];
    assert_eq!(emoji["shortcode"], "blobcat");
    assert_eq!(emoji["visible_in_picker"], true);
    // The remote emoji image is proxied through the instance, never hot-linked.
    for key in ["url", "static_url"] {
        let url = emoji[key].as_str().unwrap();
        assert!(
            url.starts_with("https://plamenu.test/media/proxy/emoji/"),
            "{key} = {url}"
        );
        assert!(!url.contains("remote.example"), "{key} leaks origin: {url}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn update_note_refreshes_the_emoji_image(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let create = create_note(
        &bob,
        1,
        "<p>:blobcat:</p>",
        &json!([emoji_tag_entry(
            "blobcat",
            "https://remote.example/files/old.png",
            "2026-06-01T00:00:00Z",
        )]),
    );
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // The edit re-announces the emoji with a new image.
    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let update = json!({
        "id": format!("{note_uri}#updates/1"),
        "type": "Update",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>:blobcat: (edited)</p>",
            "published": "2026-06-11T00:00:00Z",
            "updated": "2026-06-12T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "tag": [emoji_tag_entry(
                "blobcat",
                "https://remote.example/files/new.png",
                "2026-06-12T00:00:00Z",
            )],
        },
    });
    assert_eq!(
        post_signed(app(), &update, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let codes = vec!["blobcat".to_owned()];
    let stored = custom_emoji::lookup(&pool, &codes, Some("remote.example"))
        .await
        .unwrap();
    assert_eq!(
        stored[0].image_remote_url.as_deref(),
        Some("https://remote.example/files/new.png")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_actor_emoji_render_on_the_account_entity(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let mut bob = RemoteUser::new("remote.example", "bob");
    bob.actor.name = Some(":wave: Bob".to_owned());
    bob.actor.tag = vec![emoji_tag_entry(
        "wave",
        "https://remote.example/files/wave.png",
        "2026-06-01T00:00:00Z",
    )];
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // Any signed activity stores the actor (and with it, its emoji).
    let create = create_note(&bob, 1, "<p>hi</p>", &json!([]));
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    let (code, entity) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entity["emojis"][0]["shortcode"], "wave");
    // The remote emoji image is proxied through the instance, never hot-linked.
    let url = entity["emojis"][0]["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://plamenu.test/media/proxy/emoji/"),
        "{url}"
    );
    assert!(!url.contains("remote.example"), "{url}");
}

// ---------------------------------------------------------------------------
// Local emoji

#[sqlx::test(migrations = "../db/migrations")]
async fn custom_emojis_endpoint_lists_the_picker(pool: PgPool) {
    let visible = custom_emoji::create_local(&pool, "party", "1.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    // A categorized emoji carries `category`; an uncategorized one omits the
    // key entirely, like Mastodon's serializer.
    let categorized = custom_emoji::create_local(&pool, "blobwave", "3.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    custom_emoji::update_local_flags(&pool, categorized.id, false, true, Some("blobs"))
        .await
        .unwrap()
        .unwrap();
    let hidden = custom_emoji::create_local(&pool, "hidden", "2.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    sqlx::query!(
        "UPDATE custom_emojis SET visible_in_picker = FALSE WHERE id = $1",
        hidden.id
    )
    .execute(&pool)
    .await
    .unwrap();
    // Remote emoji never appear in the picker.
    custom_emoji::upsert_remote(
        &pool,
        custom_emoji::RemoteEmojiData {
            shortcode: "remote_only",
            domain: "remote.example",
            uri: None,
            image_remote_url: "https://remote.example/files/r.png",
            updated: None,
        },
    )
    .await
    .unwrap();

    let (code, listing) = api(
        test_app_with(pool.clone(), StubFederation::with_actors([])),
        "GET",
        "/api/v1/custom_emojis",
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        listing,
        json!([{
            "shortcode": "blobwave",
            "url": "https://plamenu.test/media/3.png",
            "static_url": "https://plamenu.test/media/3.png",
            "visible_in_picker": true,
            "category": "blobs",
        }, {
            "shortcode": "party",
            "url": "https://plamenu.test/media/1.png",
            "static_url": "https://plamenu.test/media/1.png",
            "visible_in_picker": true,
        }])
    );
    let _ = visible;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_status_renders_and_federates_its_emoji(pool: PgPool) {
    custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let stub = StubFederation::with_actors([]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (code, entity) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "time to :party: hard", "visibility": "public"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entity["emojis"][0]["shortcode"], "party");
    assert_eq!(
        entity["emojis"][0]["url"],
        "https://plamenu.test/media/7.png"
    );

    // A disabled emoji vanishes from the rendered entity.
    sqlx::query!("UPDATE custom_emojis SET disabled = TRUE WHERE shortcode = 'party'")
        .execute(&pool)
        .await
        .unwrap();
    let status_id = entity["id"].as_str().unwrap().to_owned();
    let (_, refreshed) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(refreshed["emojis"], json!([]));
    sqlx::query!("UPDATE custom_emojis SET disabled = FALSE WHERE shortcode = 'party'")
        .execute(&pool)
        .await
        .unwrap();

    // The AP Note advertises the emoji as a dereferenceable Emoji tag.
    let (code, note) = ap_get(app(), &format!("/users/alice/statuses/{status_id}")).await;
    assert_eq!(code, StatusCode::OK);
    let tags = note["tag"].as_array().unwrap();
    let emoji_tag = tags
        .iter()
        .find(|t| t["type"] == "Emoji")
        .expect("Note carries an Emoji tag");
    assert_eq!(emoji_tag["name"], ":party:");
    assert_eq!(emoji_tag["icon"]["url"], "https://plamenu.test/media/7.png");
    assert_eq!(emoji_tag["icon"]["mediaType"], "image/png");
    let emoji_id = emoji_tag["id"].as_str().unwrap();
    assert!(
        emoji_id.starts_with("https://plamenu.test/emojis/"),
        "{emoji_id}"
    );
    assert!(emoji_tag["updated"].is_string());

    // The advertised id dereferences to the AP Emoji object.
    let path = emoji_id.strip_prefix("https://plamenu.test").unwrap();
    let (code, object) = ap_get(app(), path).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(object["type"], "Emoji");
    assert_eq!(object["name"], ":party:");
    assert_eq!(object["icon"]["url"], "https://plamenu.test/media/7.png");
    assert_eq!(object["@context"][1]["Emoji"], "toot:Emoji");

    // The edit-history synthesis carries the emoji too.
    let (code, history) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{status_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(history[0]["emojis"][0]["shortcode"], "party");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_status_page_renders_local_emoji_images(pool: PgPool) {
    // The web UI must swap `:shortcode:` for the emoji image everywhere the
    // entity's texts show: body, content warning, and the author's display
    // name (the account entity's own `emojis`).
    custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let stub = StubFederation::with_actors([]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let (code, _) = api(
        app(),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "display_name": "Alice :party:" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, entity) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "time to :party: hard",
            "spoiler_text": "loud :party: inside",
            "visibility": "public",
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    let img = emoji_img("party", "https://plamenu.test/media/7.png");
    let (code, page) = web_get(
        app(),
        &format!("/@alice/{}", entity["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        page.contains(&format!("time to {img} hard")),
        "body keeps the raw shortcode: {page}"
    );
    assert!(
        page.contains(&format!("loud {img} inside")),
        "content warning keeps the raw shortcode: {page}"
    );
    assert!(
        page.contains(&format!("Alice {img}")),
        "display name keeps the raw shortcode: {page}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn web_status_page_renders_remote_emoji_images(pool: PgPool) {
    // A remote post's emoji resolve against the sender's domain and render
    // from the remote image URL.
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let create = create_note(
        &bob,
        1,
        "<p>hello :blobcat: world</p>",
        &json!([emoji_tag_entry(
            "blobcat",
            "https://remote.example/files/blobcat.png",
            "2026-06-01T00:00:00Z",
        )]),
    );
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let note_uri = format!("{}/statuses/1", bob.actor.id);
    let item = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let (code, page) = web_get(app(), &format!("/@bob@remote.example/{}", item.id)).await;
    assert_eq!(code, StatusCode::OK);
    // The remote emoji renders as an <img>, but proxied through the instance —
    // its src never points at the origin.
    assert!(
        page.contains(r#"class="emoji" src="https://plamenu.test/media/proxy/emoji/"#)
            && page.contains(r#"alt=":blobcat:""#),
        "remote emoji should render a proxied image: {page}"
    );
    assert!(
        !page.contains("remote.example/files/blobcat.png"),
        "remote emoji leaks origin: {page}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delivered_create_carries_the_emoji_tag(pool: PgPool) {
    // The fanned-out Create is built separately from the object endpoint
    // (actions::post_status vs note_for_status) — both must advertise the
    // emoji, or remote servers never learn about it.
    custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();

    let stub: std::sync::Arc<StubFederation> = std::sync::Arc::default();
    let state = common::test_state_with(pool.clone(), stub.clone());
    plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "time to :party: hard",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);

    let sent = stub.deliveries();
    assert_eq!(sent.len(), 1);
    let tags = sent[0].activity["object"]["tag"].as_array().unwrap();
    let emoji_tag = tags
        .iter()
        .find(|t| t["type"] == "Emoji")
        .expect("delivered Create carries an Emoji tag");
    assert_eq!(emoji_tag["name"], ":party:");
    assert_eq!(emoji_tag["icon"]["url"], "https://plamenu.test/media/7.png");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn poll_options_carry_their_emoji(pool: PgPool) {
    custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app_with(pool.clone(), StubFederation::with_actors([]));

    let (code, entity) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "which?",
            "poll": {"options": [":party: now", "later"], "expires_in": 3600},
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entity["poll"]["emojis"][0]["shortcode"], "party");
    // The status entity scans poll options too.
    assert_eq!(entity["emojis"][0]["shortcode"], "party");

    // And so does the standalone poll endpoint.
    let poll_id = entity["poll"]["id"].as_str().unwrap();
    let (code, poll) = api(
        app(),
        "GET",
        &format!("/api/v1/polls/{poll_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(poll["emojis"][0]["shortcode"], "party");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn actor_document_carries_profile_emoji(pool: PgPool) {
    custom_emoji::create_local(&pool, "wave", "3.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let account = create_local_account(&pool, "alice", ":wave: Alice").await;
    let app = || test_app_with(pool.clone(), StubFederation::with_actors([]));

    let (code, actor) = ap_get(app(), "/users/alice").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(actor["tag"][0]["type"], "Emoji");
    assert_eq!(actor["tag"][0]["name"], ":wave:");
    assert_eq!(
        actor["tag"][0]["icon"]["url"],
        "https://plamenu.test/media/3.png"
    );

    // The REST entity sees it as well.
    let (code, entity) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/{}", account.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entity["emojis"][0]["shortcode"], "wave");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn emoji_object_endpoint_handles_missing_and_disabled(pool: PgPool) {
    let emoji = custom_emoji::create_local(&pool, "party", "7.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    let app = || test_app_with(pool.clone(), StubFederation::with_actors([]));

    // Without an ActivityPub Accept header the endpoint refuses.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/emojis/{}", emoji.id))
        .body(Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);

    let (code, _) = ap_get(app(), "/emojis/999").await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    sqlx::query!(
        "UPDATE custom_emojis SET disabled = TRUE WHERE id = $1",
        emoji.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let (code, _) = ap_get(app(), &format!("/emojis/{}", emoji.id)).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn proxy_cache_recodes_still_emoji_and_keeps_animated_ones(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let mut blobcat = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        128,
        128,
        image::Rgb([200, 100, 50]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut blobcat),
        image::ImageFormat::Png,
    )
    .unwrap();
    let mut party = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut party);
        for shade in [0u8, 255u8] {
            let frame = image::Frame::new(image::RgbaImage::from_pixel(
                8,
                8,
                image::Rgba([shade, 0, 0, 255]),
            ));
            encoder.encode_frame(frame).unwrap();
        }
    }
    stub.serve_media(
        "https://remote.example/files/blobcat.png",
        "image/png",
        blobcat,
    );
    stub.serve_media("https://remote.example/files/party.gif", "image/gif", party);
    let state = common::test_state_with(pool.clone(), stub.clone());
    let app = || plamenu::build_router(state.clone());

    let create = create_note(
        &bob,
        1,
        "<p>:blobcat: :party:</p>",
        &json!([
            emoji_tag_entry(
                "blobcat",
                "https://remote.example/files/blobcat.png",
                "2026-06-01T00:00:00Z",
            ),
            emoji_tag_entry(
                "party",
                "https://remote.example/files/party.gif",
                "2026-06-01T00:00:00Z",
            ),
        ]),
    );
    assert_eq!(
        post_signed(app(), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let codes = vec!["blobcat".to_owned(), "party".to_owned()];
    let stored = custom_emoji::lookup(&pool, &codes, Some("remote.example"))
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);

    // Hitting the proxy caches on demand: the still PNG recodes to AVIF (the
    // default cached-image setting; these copies never federate outward), the
    // animated GIF is kept as arrived so it stays animated.
    for emoji in &stored {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/media/proxy/emoji/{}", emoji.id))
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert!(response.status().is_redirection(), "{}", response.status());
    }
    let cached = custom_emoji::lookup(&pool, &codes, Some("remote.example"))
        .await
        .unwrap();
    let by_code = |code: &str| cached.iter().find(|emoji| emoji.shortcode == code).unwrap();
    let still = by_code("blobcat");
    assert!(
        still
            .image_file_name
            .as_deref()
            .unwrap()
            .ends_with(".emoji.avif"),
        "{:?}",
        still.image_file_name
    );
    assert_eq!(still.image_content_type.as_deref(), Some("image/avif"));
    let animated = by_code("party");
    assert!(
        animated
            .image_file_name
            .as_deref()
            .unwrap()
            .ends_with(".emoji.gif"),
        "{:?}",
        animated.image_file_name
    );
    let bytes = state
        .media
        .get(animated.image_file_name.as_deref().unwrap())
        .await
        .unwrap();
    assert!(plamenu::media_processing::is_animated_image(&bytes));
}
