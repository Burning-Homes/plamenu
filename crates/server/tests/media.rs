//! Media pipeline tests: blurhash + small styles + focal points on image
//! uploads, ffmpeg video/audio/gifv processing (synchronous v1 and queued
//! v2), the `GET`/`PUT`/`DELETE /api/v1/media/{id}` endpoints, posting
//! validations, and federated attachment metadata in both directions.
//!
//! The video/audio tests shell out to the real ffmpeg/ffprobe — the same
//! binaries the server itself requires.

mod common;

use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{AppState, media_worker};
use plamenu_db::{PgPool, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> String {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let (_, app_response) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "media-tests",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    let client_id = app_response["client_id"].as_str().unwrap().to_owned();
    let client_secret = app_response["client_secret"].as_str().unwrap().to_owned();
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
    let (_, token) = api(
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
    token["access_token"].as_str().unwrap().to_owned()
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

/// A router over a shared state — file-touching steps (upload, serve,
/// worker) must reuse one state, or each request gets a fresh `MemoryStore`.
fn router(state: &AppState) -> Router {
    plamenu::build_router(state.clone())
}

/// True when the URL's file extension is `ext`.
fn has_extension(url: &Value, ext: &str) -> bool {
    url.as_str().is_some_and(|u| {
        std::path::Path::new(u)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
    })
}

/// Builds a `multipart/form-data` body: the file plus extra text fields.
fn multipart_body(file: &[u8], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "plamenu-media-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"upload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; \
                 name=\"{name}\"\r\n\r\n{value}"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// Uploads a file through the router; returns (status, entity).
async fn upload(
    state: &AppState,
    token: &str,
    uri: &str,
    file: &[u8],
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    let (content_type, body) = multipart_body(file, fields);
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Fetches a `/media/...` file through the router; returns the status.
async fn fetch_media_file(state: &AppState, url: &str) -> StatusCode {
    let path = url.strip_prefix("https://plamenu.test").unwrap_or(url);
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    router(state).oneshot(request).await.unwrap().status()
}

fn sample_png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([200, 100, 50]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

/// Generates a media fixture by running ffmpeg with `args` (output name
/// appended); requires ffmpeg on the PATH, like the server does.
fn ffmpeg_fixture(args: &[&str], out_name: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(out_name);
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "fatal"])
        .args(args)
        .arg("-y")
        .arg(&out)
        .status()
        .expect("ffmpeg must be installed to run the media tests");
    assert!(status.success(), "ffmpeg fixture generation failed");
    std::fs::read(&out).unwrap()
}

/// A one-second soundless 64x64 H.264 mp4 (passthrough-eligible → gifv).
fn soundless_mp4() -> Vec<u8> {
    ffmpeg_fixture(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
        ],
        "fixture.mp4",
    )
}

/// The same clip with a sine audio track (a real `video`).
fn mp4_with_audio() -> Vec<u8> {
    ffmpeg_fixture(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
        ],
        "fixture.mp4",
    )
}

fn wav_audio() -> Vec<u8> {
    ffmpeg_fixture(
        &["-f", "lavfi", "-i", "sine=frequency=440:duration=1"],
        "fixture.wav",
    )
}

/// A VP8 webm — not passthrough-eligible, exercising the encode path.
fn vp8_webm() -> Vec<u8> {
    ffmpeg_fixture(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-c:v",
            "libvpx",
        ],
        "fixture.webm",
    )
}

/// A two-frame animated GIF (10 fps), via the image crate.
fn animated_gif() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for shade in [60u8, 220u8] {
            let buffer = image::RgbaImage::from_pixel(16, 16, image::Rgba([shade, 30, 30, 255]));
            let frame =
                image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(100, 1));
            encoder.encode_frame(frame).unwrap();
        }
    }
    bytes
}

/// A two-frame GIF whose first frame has transparent pixels — the kind the
/// gifv conversion would flatten onto a solid background.
fn alpha_animated_gif() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for shade in [60u8, 220u8] {
            let mut buffer =
                image::RgbaImage::from_pixel(16, 16, image::Rgba([shade, 30, 30, 255]));
            for pixel in buffer.pixels_mut().take(128) {
                *pixel = image::Rgba([0, 0, 0, 0]);
            }
            let frame =
                image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(100, 1));
            encoder.encode_frame(frame).unwrap();
        }
    }
    bytes
}

#[sqlx::test(migrations = "../db/migrations")]
async fn image_upload_carries_blurhash_small_style_and_focus(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    let (code, uploaded) = upload(
        &state,
        &token,
        "/api/v1/media",
        &sample_png(64, 48),
        &[("description", "a pic"), ("focus", "-0.5,0.3")],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["type"], "image");
    assert_eq!(uploaded["description"], "a pic");
    assert_eq!(uploaded["text_url"], Value::Null);
    assert_eq!(uploaded["preview_remote_url"], Value::Null);

    // Mastodon's 4x4 blurhash is 36 characters.
    let blurhash = uploaded["blurhash"].as_str().unwrap();
    assert_eq!(blurhash.len(), 36);

    // meta: original + small geometry and the focal point.
    assert_eq!(uploaded["meta"]["original"]["width"], 64);
    assert_eq!(uploaded["meta"]["original"]["size"], "64x48");
    assert_eq!(uploaded["meta"]["small"]["width"], 64);
    assert_eq!(uploaded["meta"]["small"]["height"], 48);
    assert_eq!(uploaded["meta"]["focus"]["x"], -0.5);
    assert_eq!(uploaded["meta"]["focus"]["y"], 0.3);

    // The preview is its own stored file and is served.
    let preview_url = uploaded["preview_url"].as_str().unwrap();
    assert!(preview_url.contains(".small."), "{preview_url}");
    assert_eq!(fetch_media_file(&state, preview_url).await, StatusCode::OK);

    // GET answers 200 for a processed upload.
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let (code, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(shown["blurhash"], blurhash);

    // PUT merges: focus alone keeps the description, and vice versa.
    let (code, updated) = api(
        router(&state),
        "PUT",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        Some(json!({"focus": "0.1,0.2"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(updated["description"], "a pic");
    assert_eq!(updated["meta"]["focus"]["x"], 0.1);
    let (_, updated) = api(
        router(&state),
        "PUT",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        Some(json!({"description": "better"})),
    )
    .await;
    assert_eq!(updated["description"], "better");
    assert_eq!(updated["meta"]["focus"]["y"], 0.2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_video_upload_processes_in_background_and_federates_metadata(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // 202 + url-less entity; a soundless video is a gifv, like Mastodon.
    let (code, queued) = upload(&state, &token, "/api/v2/media", &soundless_mp4(), &[]).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["type"], "gifv");
    assert_eq!(queued["url"], Value::Null);
    let media_id = queued["id"].as_str().unwrap().to_owned();

    // Polling answers 206 while queued.
    let (code, _) = api(
        router(&state),
        "GET",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::PARTIAL_CONTENT);

    // Unprocessed media cannot attach, with Mastodon's exact wording.
    let (code, body) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "too soon", "media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Cannot attach files that have not finished processing. Try again in a moment!"
    );

    // The spooled original is never served.
    assert_eq!(
        fetch_media_file(&state, &format!("/media/{media_id}.orig")).await,
        StatusCode::NOT_FOUND
    );

    // The worker transcodes it.
    assert_eq!(media_worker::run_due(&state).await, 1);
    let (code, done) = api(
        router(&state),
        "GET",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{done}");
    let url = done["url"].as_str().unwrap().to_owned();
    assert!(has_extension(&done["url"], "mp4"), "{url}");
    assert!(done["blurhash"].is_string());
    assert_eq!(done["meta"]["original"]["width"], 64);
    assert_eq!(done["meta"]["original"]["frame_rate"], "10/1");
    assert!(done["meta"]["original"]["duration"].as_f64().unwrap() > 0.5);
    assert!(done["meta"]["original"]["bitrate"].as_i64().unwrap() > 0);
    // The poster frame is the small style.
    assert_eq!(done["meta"]["small"]["width"], 64);
    let preview_url = done["preview_url"].as_str().unwrap();
    assert!(preview_url.ends_with(".small.avif"), "{preview_url}");
    assert_eq!(fetch_media_file(&state, preview_url).await, StatusCode::OK);
    assert_eq!(fetch_media_file(&state, &url).await, StatusCode::OK);

    // Attaching now works, and the AP Note carries the metadata.
    let (code, posted) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "clip", "media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    assert_eq!(posted["media_attachments"][0]["type"], "gifv");

    let status_id = posted["id"].as_str().unwrap();
    let request = Request::builder()
        .uri(format!("/users/alice/statuses/{status_id}"))
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = router(&state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let note: Value = serde_json::from_slice(&bytes).unwrap();
    let attachment = &note["attachment"][0];
    assert_eq!(attachment["type"], "Document");
    assert_eq!(attachment["mediaType"], "video/mp4");
    assert_eq!(attachment["width"], 64);
    assert_eq!(attachment["height"], 64);
    assert!(attachment["blurhash"].is_string());
    let duration = attachment["duration"].as_str().unwrap();
    assert!(
        duration.starts_with("PT") && duration.ends_with('S'),
        "{duration}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v1_av_uploads_process_synchronously(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // A video with an audio track is a `video` and finishes in-request.
    let (code, video) = upload(&state, &token, "/api/v1/media", &mp4_with_audio(), &[]).await;
    assert_eq!(code, StatusCode::OK, "{video}");
    assert_eq!(video["type"], "video");
    assert!(has_extension(&video["url"], "mp4"));
    assert!(video["meta"]["original"]["duration"].as_f64().unwrap() > 0.5);

    // Audio becomes an mp3 with no preview or blurhash.
    let (code, audio) = upload(&state, &token, "/api/v1/media", &wav_audio(), &[]).await;
    assert_eq!(code, StatusCode::OK, "{audio}");
    assert_eq!(audio["type"], "audio");
    assert!(has_extension(&audio["url"], "mp3"));
    assert_eq!(audio["preview_url"], Value::Null);
    assert_eq!(audio["blurhash"], Value::Null);
    assert!(audio["meta"]["original"]["duration"].as_f64().unwrap() > 0.5);
    assert!(audio["meta"]["original"].get("width").is_none());

    // A VP8 webm is re-encoded to mp4 (no passthrough).
    let (code, webm) = upload(&state, &token, "/api/v1/media", &vp8_webm(), &[]).await;
    assert_eq!(code, StatusCode::OK, "{webm}");
    assert!(has_extension(&webm["url"], "mp4"));
    assert_eq!(webm["type"], "gifv"); // soundless

    // Mixing a real video with an image is refused, Mastodon's wording.
    let (_, picture) = upload(&state, &token, "/api/v1/media", &sample_png(16, 16), &[]).await;
    let (code, body) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "mixed",
            "media_ids": [video["id"], picture["id"]],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Cannot attach a video to a post that already contains images"
    );

    // ... but a gifv may mix with images, like Mastodon.
    let (code, posted) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "gif and pic",
            "media_ids": [webm["id"], picture["id"]],
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn animated_gifs_become_gifv(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    let (code, uploaded) = upload(&state, &token, "/api/v1/media", &animated_gif(), &[]).await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["type"], "gifv");
    assert!(has_extension(&uploaded["url"], "mp4"));
    assert!(uploaded["blurhash"].is_string());

    // A still image posing as a GIF stays an image.
    let mut still = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3])))
        .write_to(
            &mut std::io::Cursor::new(&mut still),
            image::ImageFormat::Gif,
        )
        .unwrap();
    let (code, uploaded) = upload(&state, &token, "/api/v1/media", &still, &[]).await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["type"], "image");

    // A transparent animated GIF skips the gifv conversion (H.264 cannot
    // carry alpha) and stays a GIF, animated and transparent (M39).
    let (code, uploaded) =
        upload(&state, &token, "/api/v1/media", &alpha_animated_gif(), &[]).await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["type"], "image");
    assert!(has_extension(&uploaded["url"], "gif"));
    assert!(uploaded["blurhash"].is_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn media_delete_and_in_use_rules(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // Deleting an unattached upload removes the row and its files.
    let (_, uploaded) = upload(&state, &token, "/api/v1/media", &sample_png(16, 16), &[]).await;
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let url = uploaded["url"].as_str().unwrap().to_owned();
    let (code, body) = api(
        router(&state),
        "DELETE",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));
    assert_eq!(fetch_media_file(&state, &url).await, StatusCode::NOT_FOUND);
    let (code, _) = api(
        router(&state),
        "GET",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Attached media cannot be deleted (or shown/edited — out of scope).
    let (_, uploaded) = upload(&state, &token, "/api/v1/media", &sample_png(16, 16), &[]).await;
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let (code, _) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "with pic", "media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, body) = api(
        router(&state),
        "DELETE",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Media attachment is currently used by a status"
    );
    let (code, _) = api(
        router(&state),
        "GET",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn upload_and_attach_validations(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // Garbage is neither an image nor anything ffprobe recognizes.
    let (code, body) = upload(&state, &token, "/api/v1/media", b"not media at all", &[]).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: File content type is not supported"
    );

    // Unknown ids are named in Mastodon's not-found wording.
    let (code, body) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "x", "media_ids": ["11", "22"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Media 11, 22 not found or already attached to another post"
    );

    // Over the default local limit (6) is its own error, checked first.
    let (code, body) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "x", "media_ids": ["1", "2", "3", "4", "5", "6", "7"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Cannot attach more than 6 files");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_attachment_metadata_is_stored(pool: PgPool) {
    const GOOD_HASH: &str = "UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of";
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/300", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>media metadata</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "video/mp4",
                    "url": "https://remote.example/files/clip.mp4",
                    // Mastodon's parser prefers summary over name.
                    "summary": "alt from summary",
                    "name": "ignored",
                    "blurhash": GOOD_HASH,
                    "focalPoint": [-0.7, 0.25],
                    "width": 640,
                    "height": 480,
                    "icon": { "type": "Image", "url": "https://remote.example/files/clip_thumb.png" },
                },
                {
                    "type": "Document",
                    "mediaType": "image/png",
                    "url": "https://remote.example/files/pic.png",
                    "name": "plain",
                    // Malformed: rejected, stored as null.
                    "blurhash": "!!!! not a hash",
                },
            ],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/300", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let (_, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    let attachments = shown["media_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 2);

    let clip = attachments
        .iter()
        .find(|a| a["description"] == "alt from summary")
        .unwrap();
    assert_eq!(clip["type"], "video");
    // Not yet downloaded, but no client-facing URL points at the origin:
    // everything routes through our media proxy.
    let clip_url = clip["url"].as_str().unwrap();
    assert!(clip_url.starts_with("https://plamenu.test/media/proxy/attachment/"));
    assert!(!clip_url.contains("remote.example"));
    assert_eq!(clip["blurhash"], GOOD_HASH);
    assert_eq!(clip["meta"]["focus"]["x"], -0.7);
    assert_eq!(clip["meta"]["original"]["width"], 640);
    assert_eq!(clip["meta"]["original"]["height"], 480);
    // The federated thumbnail is proxied too (the small variant), never hot-linked.
    for key in ["preview_url", "preview_remote_url"] {
        let url = clip[key].as_str().unwrap_or_default();
        let parsed = url::Url::parse(url).unwrap();
        assert!(parsed.path().ends_with("/small"), "{key} = {url}");
        assert_eq!(
            parsed.query(),
            Some("d=1"),
            "default allows origin fallback"
        );
        assert!(!url.contains("remote.example"), "{key} leaks origin: {url}");
    }

    let pic = attachments
        .iter()
        .find(|a| a["description"] == "plain")
        .unwrap();
    assert_eq!(pic["blurhash"], Value::Null);
    assert_eq!(pic["description"], "plain");
    assert!(
        pic["url"]
            .as_str()
            .unwrap()
            .starts_with("https://plamenu.test/media/proxy/attachment/")
    );
}

/// Inbound federated media beyond the local posting limit is preserved, not
/// truncated the way Mastodon (and Plamenu before this) capped it at 4. Remote
/// servers legitimately allow far more, so a 6-image Note keeps all 6.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_media_beyond_local_limit_is_kept(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);

    let attachment = |n: usize| {
        json!({
            "type": "Document",
            "mediaType": "image/png",
            "url": format!("https://remote.example/files/pic{n}.png"),
            "name": format!("image {n}"),
        })
    };
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/301", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>six images</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": (0..6).map(attachment).collect::<Vec<_>>(),
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/301", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let (_, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    // All six survive — no silent truncation at 4.
    assert_eq!(shown["media_attachments"].as_array().unwrap().len(), 6);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_image_is_downloaded_and_served_locally(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/pic.png";
    stub.serve_media(pic_url, "image/png", sample_png(800, 600));
    let state = test_state_with(pool.clone(), stub.clone());

    // The remote AVIF default is independent of the local-upload setting.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_full_processing: "passthrough",
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/400", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>cached image</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                { "type": "Document", "mediaType": "image/png", "url": pic_url, "name": "a cat" },
            ],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/400", bob.actor.id))
        .await
        .unwrap()
        .unwrap();

    // The download is queued; the worker caches it (one image, no ffmpeg).
    assert_eq!(media_worker::run_due(&state).await, 1);
    assert_eq!(stub.media_fetches(), vec![pic_url.to_owned()]);

    let (_, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    let pic = &shown["media_attachments"].as_array().unwrap()[0];
    assert_eq!(pic["type"], "image");
    assert_eq!(pic["description"], "a cat");
    // url + preview point at our cached copy. The full rendition follows
    // `media_remote_full_processing` (default `avif`, regardless of
    // the local-upload knob set above), so the PNG source becomes AVIF.
    assert!(
        pic["url"]
            .as_str()
            .unwrap()
            .starts_with("https://plamenu.test/media/"),
        "url should be local: {}",
        pic["url"]
    );
    assert!(has_extension(&pic["url"], "avif"));
    assert!(
        pic["preview_url"]
            .as_str()
            .unwrap()
            .starts_with("https://plamenu.test/media/")
    );
    // remote_url is re-pointed at our proxy (never the origin), so a client
    // hotlinking it still cannot reach the remote host.
    let remote_url = pic["remote_url"].as_str().unwrap();
    assert!(remote_url.starts_with("https://plamenu.test/media/proxy/attachment/"));
    assert!(!remote_url.contains("remote.example"));
    // Our own blurhash, dimensions and small style now exist.
    assert!(pic["blurhash"].is_string());
    assert_eq!(pic["meta"]["original"]["width"], 800);
    assert_eq!(pic["meta"]["original"]["height"], 600);
    assert!(pic["meta"]["small"]["width"].is_number());

    // The cached files are actually served from our store.
    assert_eq!(
        fetch_media_file(&state, pic["url"].as_str().unwrap()).await,
        StatusCode::OK
    );
    assert_eq!(
        fetch_media_file(&state, pic["preview_url"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

/// A remote download that keeps failing must not strand the attachment in
/// `processing = 'queued'` after the job runs out of retries — the row
/// resolves to `complete` without a cached file (this left 20 permanently-
/// "queued" rows on staging). The client still only ever sees a proxy URL,
/// never the origin.
#[sqlx::test(migrations = "../db/migrations")]
async fn exhausted_remote_download_still_only_proxies(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    // The stub never serves this URL, so every fetch attempt fails.
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/blocked.png";
    let state = test_state_with(pool.clone(), stub.clone());

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/401", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>uncacheable image</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                { "type": "Document", "mediaType": "image/png", "url": pic_url },
            ],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // Drive the job through every attempt, pulling each backoff forward.
    for _ in 0..5 {
        sqlx::query!("UPDATE media_processing_jobs SET run_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(media_worker::run_due(&state).await, 1);
    }

    // The job is gone and the row is resolved, not stranded in `queued`.
    let jobs = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM media_processing_jobs"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0, "no retry job may remain");
    let row = sqlx::query!(
        "SELECT processing, file_name FROM media_attachments WHERE remote_url = $1",
        pic_url,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.processing, "complete");
    assert_eq!(row.file_name, None);

    // The API keeps serving the attachment through the proxy — never the
    // origin, even though we could not cache it.
    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/401", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let (_, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    let pic = &shown["media_attachments"].as_array().unwrap()[0];
    for key in ["url", "remote_url"] {
        let url = pic[key].as_str().unwrap();
        assert!(
            url.starts_with("https://plamenu.test/media/proxy/attachment/"),
            "{key} = {url}"
        );
        assert!(!url.contains("remote.example"), "{key} leaks origin: {url}");
    }
}

/// Ingests one federated animated GIF under the given
/// `media_remote_gif_handling` mode and returns the state plus the cached
/// attachment entity. `label` disambiguates the local user, remote actor and
/// attachment URL so several ingests can share one test database.
async fn inbound_gif_under_mode(
    pool: &PgPool,
    label: &str,
    gif: Vec<u8>,
    mode: Option<&str>,
) -> (plamenu::state::AppState, serde_json::Value) {
    let token = user_with_token(pool, &format!("alice{label}")).await;
    if let Some(mode) = mode {
        let current = plamenu_db::instance_settings::get(pool).await.unwrap();
        plamenu_db::instance_settings::save(
            pool,
            plamenu_db::instance_settings::SettingsUpdate {
                media_remote_gif_handling: mode,
                ..current.as_update()
            },
        )
        .await
        .unwrap();
    }
    let bob = RemoteUser::new("remote.example", &format!("bob{label}"));
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let gif_url = format!("https://remote.example/files/anim{label}.gif");
    stub.serve_media(&gif_url, "image/gif", gif);
    let state = test_state_with(pool.clone(), stub.clone());

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/401", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>look a gif</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                { "type": "Document", "mediaType": "image/gif", "url": gif_url },
            ],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = plamenu_db::status::find_by_uri(pool, &format!("{}/statuses/401", bob.actor.id))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(media_worker::run_due(&state).await, 1);

    let (_, shown) = api(
        router(&state),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&token),
        None,
    )
    .await;
    let attachment = shown["media_attachments"].as_array().unwrap()[0].clone();
    (state, attachment)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_animated_gif_is_kept_as_arrived(pool: PgPool) {
    // Under the explicit `keep` setting, a federated GIF keeps the origin's exact
    // bytes — re-encoding to gifv cost pixel art its quality and rendered
    // transparency as a solid background. Browsers animate the <img>, so the
    // attachment presents as an image with a first-frame preview.
    let (state, gif) = inbound_gif_under_mode(&pool, "keep", animated_gif(), Some("keep")).await;
    assert_eq!(gif["type"], "image");
    assert!(has_extension(&gif["url"], "gif"));
    assert!(gif["preview_url"].as_str().unwrap().contains("/media/"));
    assert!(gif["blurhash"].is_string());
    assert_eq!(
        fetch_media_file(&state, gif["url"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_animated_gif_converts_when_opted_in(pool: PgPool) {
    // The pre-M39 behavior survives as the opt-in `gifv` mode: an opaque
    // remote GIF re-encodes to a looping soundless mp4…
    let (opaque_state, opaque) =
        inbound_gif_under_mode(&pool, "opaque", animated_gif(), Some("gifv")).await;
    assert_eq!(opaque["type"], "gifv");
    assert!(has_extension(&opaque["url"], "mp4"));
    assert!(opaque["blurhash"].is_string());
    assert_eq!(
        fetch_media_file(&opaque_state, opaque["url"].as_str().unwrap()).await,
        StatusCode::OK
    );

    // …but even under `gifv` a GIF with transparency stays a GIF —
    // H.264/yuv420p cannot carry alpha and would paint the transparent
    // regions as a solid background.
    let (alpha_state, alpha) =
        inbound_gif_under_mode(&pool, "alpha", alpha_animated_gif(), Some("gifv")).await;
    assert_eq!(alpha["type"], "image");
    assert!(has_extension(&alpha["url"], "gif"));
    assert_eq!(
        fetch_media_file(&alpha_state, alpha["url"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_animated_gif_reencodes_to_webp_by_default(pool: PgPool) {
    // The default re-encodes to animated WebP, preserving transparency.
    // (Skipped gracefully in code — falls back to keeping the GIF — when
    // ffmpeg lacks libwebp_anim; the dev/CI ffmpeg has it.)
    let (state, gif) = inbound_gif_under_mode(&pool, "webp", alpha_animated_gif(), None).await;
    assert_eq!(gif["type"], "image");
    assert!(has_extension(&gif["url"], "webp"));
    assert!(gif["blurhash"].is_string());
    assert_eq!(
        fetch_media_file(&state, gif["url"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_actor_avatar_and_header_are_cached_locally(pool: PgPool) {
    let mut bob = RemoteUser::new("remote.example", "bob");
    let avatar_origin = "https://remote.example/avatars/bob.png";
    let header_origin = "https://remote.example/headers/bob.png";
    bob.actor.icon = Some(json!({ "type": "Image", "url": avatar_origin }));
    bob.actor.image = Some(json!({ "type": "Image", "url": header_origin }));
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.serve_media(avatar_origin, "image/png", sample_png(500, 500));
    stub.serve_media(header_origin, "image/png", sample_png(1600, 600));
    let state = test_state_with(pool.clone(), stub.clone());

    let account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    // Both slots are queued; the worker caches them (images, no ffmpeg).
    assert_eq!(media_worker::run_due_account_media(&state).await, 2);

    let refreshed = plamenu_db::account::find_by_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    let avatar = plamenu::entities::avatar_url(TEST_DOMAIN, &refreshed, false).unwrap();
    let header = plamenu::entities::header_url(TEST_DOMAIN, &refreshed, false).unwrap();
    assert!(avatar.starts_with("https://plamenu.test/media/"));
    // Still images recode to AVIF under the default cached-image setting —
    // safe for these copies, which serve local clients only (unlike *local*
    // avatars, which other servers fetch and which stay JPEG/PNG).
    assert!(
        avatar.contains(&format!("{}.avatar.avif", account.id)),
        "{avatar}"
    );
    assert!(header.starts_with("https://plamenu.test/media/"));
    assert!(
        header.contains(&format!("{}.header.avif", account.id)),
        "{header}"
    );
    assert_eq!(fetch_media_file(&state, &avatar).await, StatusCode::OK);
    assert_eq!(fetch_media_file(&state, &header).await, StatusCode::OK);

    // Re-ingesting the unchanged actor queues no new downloads.
    plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    assert_eq!(media_worker::run_due_account_media(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cached_avatars_keep_animation_and_honour_passthrough(pool: PgPool) {
    let mut bob = RemoteUser::new("remote.example", "bob");
    let bob_origin = "https://remote.example/avatars/bob.gif";
    bob.actor.icon = Some(json!({ "type": "Image", "url": bob_origin }));
    let mut carol = RemoteUser::new("remote.example", "carol");
    let carol_origin = "https://remote.example/avatars/carol.png";
    carol.actor.icon = Some(json!({ "type": "Image", "url": carol_origin }));
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);
    stub.serve_media(bob_origin, "image/gif", animated_gif());
    stub.serve_media(carol_origin, "image/png", sample_png(500, 500));
    let state = test_state_with(pool.clone(), stub.clone());

    // An animated GIF avatar is cached as arrived — never frozen to its
    // first frame by a still re-encode.
    let account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    assert_eq!(media_worker::run_due_account_media(&state).await, 1);
    let refreshed = plamenu_db::account::find_by_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    let file = refreshed.avatar_file_name.unwrap();
    assert!(file.ends_with(".avatar.gif"), "{file}");
    let bytes = state.media.get(&file).await.unwrap();
    assert!(plamenu::media_processing::is_animated_image(&bytes));

    // Under the passthrough setting a still avatar also stays as arrived.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_cached_image_processing: "passthrough",
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    let account = plamenu::remote::store_remote_actor(&pool, &carol.actor)
        .await
        .unwrap();
    assert_eq!(media_worker::run_due_account_media(&state).await, 1);
    let refreshed = plamenu_db::account::find_by_id(&pool, account.id)
        .await
        .unwrap()
        .unwrap();
    let file = refreshed.avatar_file_name.unwrap();
    assert!(file.ends_with(".avatar.png"), "{file}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn evicted_remote_media_is_redownloaded_on_demand(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/ev.png";
    stub.serve_media(pic_url, "image/png", sample_png(400, 300));
    let state = common::test_state_retention(pool.clone(), stub.clone(), 1).await;

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/402", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>cache me</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [{ "type": "Document", "mediaType": "image/png", "url": pic_url }],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/402", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let status_uri = format!("/api/v1/statuses/{}", stored.id);

    // Initial download caches it locally.
    assert_eq!(media_worker::run_due(&state).await, 1);
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    let local_url = shown["media_attachments"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(local_url.starts_with("https://plamenu.test/media/"));
    assert_eq!(fetch_media_file(&state, &local_url).await, StatusCode::OK);

    // Age the cache and sweep: the local file is evicted from the store.
    sqlx::query("UPDATE media_attachments SET cached_at = now() - interval '10 days'")
        .execute(&pool)
        .await
        .unwrap();
    media_worker::run_retention(&state).await;
    assert_eq!(
        fetch_media_file(&state, &local_url).await,
        StatusCode::NOT_FOUND
    );

    // Rendering serves the proxy URL (never the origin) and re-queues a download...
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    let evicted_url = shown["media_attachments"][0]["url"].as_str().unwrap();
    assert!(evicted_url.starts_with("https://plamenu.test/media/proxy/attachment/"));
    assert!(!evicted_url.contains("remote.example"));
    // ...which the worker fulfils, restoring the local copy.
    assert_eq!(media_worker::run_due(&state).await, 1);
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    assert!(
        shown["media_attachments"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://plamenu.test/media/")
    );
    assert_eq!(stub.media_fetches().len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_retention_setting_overrides_the_config_default(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/setting.png";
    stub.serve_media(pic_url, "image/png", sample_png(400, 300));
    // Env bootstrap default: keep cached media 5 days.
    let state = common::test_state_retention(pool.clone(), stub.clone(), 5).await;

    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/403", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>admin will evict me</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [{ "type": "Document", "mediaType": "image/png", "url": pic_url }],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = plamenu_db::status::find_by_uri(&pool, &format!("{}/statuses/403", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    let status_uri = format!("/api/v1/statuses/{}", stored.id);
    assert_eq!(media_worker::run_due(&state).await, 1);
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    let local_url = shown["media_attachments"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2 days old: under the 5-day config default, the sweep keeps it.
    sqlx::query("UPDATE media_attachments SET cached_at = now() - interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();
    media_worker::run_retention(&state).await;
    assert_eq!(fetch_media_file(&state, &local_url).await, StatusCode::OK);

    // The admin shortens retention to 1 day (the settings form invalidates
    // the cache the same way): the next sweep evicts, no restart needed.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_cache_retention_days: 1,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    media_worker::run_retention(&state).await;
    assert_eq!(
        fetch_media_file(&state, &local_url).await,
        StatusCode::NOT_FOUND
    );

    // An explicit 0 turns eviction off even though the config default is 5
    // days: a 10-day-old copy survives the sweep.
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    assert!(
        shown["media_attachments"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://plamenu.test/media/proxy/attachment/")
    );
    assert_eq!(media_worker::run_due(&state).await, 1);
    let (_, shown) = api(router(&state), "GET", &status_uri, Some(&token), None).await;
    let local_url = shown["media_attachments"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_cache_retention_days: 0,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    sqlx::query("UPDATE media_attachments SET cached_at = now() - interval '10 days'")
        .execute(&pool)
        .await
        .unwrap();
    media_worker::run_retention(&state).await;
    assert_eq!(fetch_media_file(&state, &local_url).await, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn form_encoded_media_ids_attach(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    let (_, uploaded) = upload(&state, &token, "/api/v1/media", &sample_png(16, 16), &[]).await;
    let media_id = uploaded["id"].as_str().unwrap();

    // Rails-style repeated array keys, as form-encoding clients send them.
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/statuses")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "status=form%20pic&media_ids[]={media_id}"
        )))
        .unwrap();
    let response = router(&state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let posted: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(posted["media_attachments"][0]["id"], *media_id);
}

/// A `GET` returning the raw (status, `Location`) pair — the media proxy
/// answers with 302s we want to inspect without following.
async fn proxy_get(state: &AppState, path: &str) -> (StatusCode, Option<String>) {
    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (response.status(), location)
}

/// Ingests a remote note carrying a single image attachment and returns the
/// stored attachment id, without downloading it (no worker run).
async fn seed_remote_attachment(
    state: &AppState,
    pool: &PgPool,
    bob: &RemoteUser,
    note: u32,
    pic_url: &str,
) -> i64 {
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/{note}", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>proxy me</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [{ "type": "Document", "mediaType": "image/png", "url": pic_url }],
        },
    });
    assert_eq!(
        post_signed(router(state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE remote_url = $1",
        pic_url
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The proxy fetches an uncached remote attachment on demand (the server, not
/// the client, contacts the origin) and 302s to our immutable copy; a second
/// hit serves the cache without re-fetching.
#[sqlx::test(migrations = "../db/migrations")]
async fn media_proxy_fetches_on_demand_then_serves_cache(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/ondemand.png";
    stub.serve_media(pic_url, "image/png", sample_png(400, 300));
    let state = test_state_with(pool.clone(), stub.clone());
    let media_id = seed_remote_attachment(&state, &pool, &bob, 500, pic_url).await;

    let (status, location) =
        proxy_get(&state, &format!("/media/proxy/attachment/{media_id}")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    let location = location.unwrap();
    assert!(
        location.starts_with("https://plamenu.test/media/"),
        "{location}"
    );
    assert!(!location.contains("/proxy/"), "{location}");
    assert!(!location.contains("remote.example"), "{location}");
    // The server fetched the origin — the client never did.
    assert_eq!(stub.media_fetches(), vec![pic_url.to_owned()]);
    assert_eq!(fetch_media_file(&state, &location).await, StatusCode::OK);

    // Second hit is served from cache: no new origin fetch.
    let (status, _) = proxy_get(&state, &format!("/media/proxy/attachment/{media_id}")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(stub.media_fetches().len(), 1);
}

/// Cached legacy proxy URLs for both split and muxed HLS videos upgrade to the
/// compatibility gateway. Even `?d=1` never bypasses it to a silent origin.
#[sqlx::test(migrations = "../db/migrations")]
async fn legacy_hls_proxy_urls_upgrade_to_the_compatibility_gateway(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);

    // A real separated-audio PeerTube Video (its stored row carries
    // `remote_audio_url`) and a muxed one (no companion audio).
    let separated: Value = serde_json::from_str(include_str!(
        "fixtures/peertube/create_video_separated_audio.json"
    ))
    .unwrap();
    let muxed: Value =
        serde_json::from_str(include_str!("fixtures/peertube/create_video.json")).unwrap();
    for activity in [&separated, &muxed] {
        assert_eq!(
            post_signed(router(&state), activity, &bob.signer()).await,
            StatusCode::ACCEPTED
        );
    }
    let sep_id =
        sqlx::query_scalar!("SELECT id FROM media_attachments WHERE remote_audio_url IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    let muxed_id = sqlx::query_scalar!(
        "SELECT id FROM media_attachments
         WHERE remote_audio_url IS NULL AND remote_url LIKE '%e7946124%'"
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    for id in [sep_id, muxed_id] {
        let (status, location) =
            proxy_get(&state, &format!("/media/proxy/attachment/{id}?d=1")).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            location.as_deref(),
            Some(format!("https://plamenu.test/media/play/{id}/video.mp4").as_str())
        );
    }
}

/// Concurrent proxy hits for the same uncached attachment coalesce into one
/// origin download: the loser of the single-flight race waits for the winner
/// and serves its result (the eager-drop regression of 2026-07-12 instead
/// deadlocked the runtime here). Both requests must succeed off a single
/// origin fetch.
#[sqlx::test(migrations = "../db/migrations")]
async fn media_proxy_concurrent_requests_coalesce_into_one_fetch(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/coalesce.png";
    stub.serve_media(pic_url, "image/png", sample_png(400, 300));
    let state = test_state_with(pool.clone(), stub.clone());
    let media_id = seed_remote_attachment(&state, &pool, &bob, 501, pic_url).await;

    let path = format!("/media/proxy/attachment/{media_id}");
    let (first, second) = tokio::join!(proxy_get(&state, &path), proxy_get(&state, &path));
    assert_eq!(first.0, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(second.0, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(stub.media_fetches(), vec![pic_url.to_owned()]);
}

/// When the proxy cannot cache a file it fails closed with a 404 by default,
/// and only redirects to the origin when the viewer opted in (`?d=1`).
#[sqlx::test(migrations = "../db/migrations")]
async fn media_proxy_fails_closed_without_optin(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    // The stub never serves this URL, so every download attempt fails.
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let pic_url = "https://remote.example/files/blocked.png";
    let state = test_state_with(pool.clone(), stub.clone());
    let media_id = seed_remote_attachment(&state, &pool, &bob, 501, pic_url).await;

    // Default: no origin URL ever reaches the client — a 404, not a redirect.
    let (status, location) =
        proxy_get(&state, &format!("/media/proxy/attachment/{media_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(location, None);

    // Opted in (`?d=1`): the origin is the documented last resort.
    let (status, location) =
        proxy_get(&state, &format!("/media/proxy/attachment/{media_id}?d=1")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location.as_deref(), Some(pic_url));
}

/// `PeerTube` may regenerate a Video's preview during transcoding, leaving the
/// icon URL from its initial Create at a permanent 404. The first failed poster
/// request refreshes the canonical object, persists its replacement icon and
/// serves that image without exposing either origin URL to the browser.
#[sqlx::test(migrations = "../db/migrations")]
async fn video_poster_recovers_from_a_rotated_icon(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let stale = "https://remote.example/lazy-static/previews/stale.jpg";
    let current = "https://remote.example/lazy-static/previews/current.jpg";
    stub.serve_media(current, "image/png", sample_png(640, 360));

    let mut create: Value =
        serde_json::from_str(include_str!("fixtures/peertube/create_video.json")).unwrap();
    create["object"]["icon"] = json!([{
        "type": "Image",
        "url": stale,
        "mediaType": "image/jpeg",
        "width": 850,
        "height": 480,
    }]);
    let object_id = create["object"]["id"].as_str().unwrap().to_owned();
    let mut refreshed = create["object"].clone();
    refreshed["icon"] = json!([
        {
            "type": "Image",
            "url": stale,
            "mediaType": "image/jpeg",
            "width": 850,
            "height": 480,
        },
        {
            "type": "Image",
            "url": current,
            "mediaType": "image/jpeg",
            "width": 280,
            "height": 157,
        }
    ]);
    stub.objects
        .lock()
        .unwrap()
        .insert(object_id.clone(), refreshed);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let media_id = sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE thumbnail_remote_url = $1",
        stale
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // Model the broken icon already having been rendered once before the
    // producer rotated it. Recovery must bypass that stale URL's backoff.
    plamenu_db::media_fetch_failure::record(&pool, "poster", media_id)
        .await
        .unwrap();

    let (status, location) =
        proxy_get(&state, &format!("/media/proxy/attachment/{media_id}/small")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert!(
        location
            .as_deref()
            .is_some_and(|url| url.starts_with("https://plamenu.test/media/"))
    );
    assert_eq!(stub.media_fetches(), vec![current.to_owned()]);
    assert!(stub.fetches().contains(&object_id));
    let stored = &plamenu_db::media::find_by_ids(&pool, &[media_id])
        .await
        .unwrap()[0];
    assert_eq!(stored.thumbnail_remote_url.as_deref(), Some(current));
    assert!(
        stored.small_file_name.is_some(),
        "the replacement was cached"
    );
}

/// An unknown proxy kind or id is a 404, never a redirect.
#[sqlx::test(migrations = "../db/migrations")]
async fn media_proxy_unknown_is_not_found(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let (status, _) = proxy_get(&state, "/media/proxy/attachment/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = proxy_get(&state, "/media/proxy/bogus/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A separated-audio HLS ingest remains ready but never enters the detached
/// whole-file worker lane. Format/range/sound are covered by `hls_proxy`.
#[sqlx::test(migrations = "../db/migrations")]
async fn separated_audio_video_never_queues_the_whole_file_lane(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let activity: Value = serde_json::from_str(include_str!(
        "fixtures/peertube/create_video_separated_audio.json"
    ))
    .unwrap();
    assert_eq!(
        post_signed(router(&state), &activity, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let stored = plamenu_db::status::find_by_uri(&pool, activity["object"]["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    let row = plamenu_db::media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap()
        .remove(0);
    assert!(row.download_on_demand);
    assert!(row.file_name.is_none(), "nothing downloaded at ingest");

    // Even an old caller explicitly invoking the former play trigger cannot
    // queue it; the worker has no HLS job to claim.
    plamenu_db::media::enqueue_on_demand_download(&pool, row.id)
        .await
        .unwrap();
    assert_eq!(media_worker::run_due_on_demand(&state).await, 0);
    let current = plamenu_db::media::find_by_ids(&pool, &[row.id])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(current.processing, "complete");
    assert!(current.file_name.is_none());

    // A legacy proxy hit upgrades to the sparse/virtual gateway without
    // fetching either whole origin stream — and never queues a download job.
    let (status, location) =
        proxy_get(&state, &format!("/media/proxy/attachment/{}", row.id)).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        location.as_deref(),
        Some(format!("https://plamenu.test/media/play/{}/video.mp4", row.id).as_str())
    );
    assert!(stub.media_fetches().is_empty());
    let jobs = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM media_processing_jobs WHERE media_id = $1"#,
        row.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 0, "the legacy proxy redirect queues no download");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn retention_grades_evict_profile_images_and_size_capped_attachments(pool: PgPool) {
    // M39 retention grades: cached remote avatars/headers evict on their own
    // clock (refetched on demand), and the attachment size cap evicts oldest
    // first regardless of age.
    let mut bob = RemoteUser::new("remote.example", "bob");
    let avatar_origin = "https://remote.example/avatars/bob.png";
    bob.actor.icon = Some(json!({ "type": "Image", "url": avatar_origin }));
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.serve_media(avatar_origin, "image/png", sample_png(200, 200));
    let pic_url = "https://remote.example/files/capped.png";
    stub.serve_media(pic_url, "image/png", sample_png(300, 200));
    let state = test_state_with(pool.clone(), stub.clone());

    // Cache bob's avatar plus one remote attachment.
    let create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{}/statuses/900", bob.actor.id),
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>retention fodder</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [{ "type": "Document", "mediaType": "image/png", "url": pic_url }],
        },
    });
    assert_eq!(
        post_signed(router(&state), &create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    while media_worker::run_due(&state).await > 0 {}
    while media_worker::run_due_account_media(&state).await > 0 {}
    let account = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(account.avatar_file_name.is_some(), "avatar cached");

    // 40 days old under a 30-day profile retention: the sweep evicts it.
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_profile_retention_days: 30,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    sqlx::query("UPDATE accounts SET avatar_cached_at = now() - interval '40 days'")
        .execute(&pool)
        .await
        .unwrap();
    media_worker::run_retention(&state).await;
    let account = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(account.avatar_file_name.is_none(), "avatar evicted");
    assert!(
        account.avatar_remote_url.is_some(),
        "remote url survives for the on-demand refetch"
    );

    // Size cap: pretend the cached attachment holds 2 GiB; a 1 GiB cap
    // evicts it even though it is brand new.
    let cached = sqlx::query_scalar::<_, Option<String>>(
        "SELECT file_name FROM media_attachments WHERE remote_url IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cached.is_some(), "attachment cached before the cap");
    sqlx::query("UPDATE media_attachments SET file_size = 2147483648 WHERE remote_url IS NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            media_cache_max_gb: 1,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    media_worker::run_retention(&state).await;
    let cached = sqlx::query_scalar::<_, Option<String>>(
        "SELECT file_name FROM media_attachments WHERE remote_url IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cached.is_none(), "size cap evicted the oldest attachment");
}

/// Deleting a status removes its media files from the store, so a
/// formerly public `/media/...` URL 404s at the origin instead of serving the
/// deleted attachment's bytes forever. The capture-and-enqueue happens in the
/// delete transaction; the leased cleanup worker then removes the files.
#[sqlx::test(migrations = "../db/migrations")]
async fn deleting_a_status_makes_its_media_urls_404(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // Upload and attach an image to a status.
    let (code, uploaded) = upload(
        &state,
        &token,
        "/api/v1/media",
        &sample_png(64, 48),
        &[("description", "secret")],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let url = uploaded["url"].as_str().unwrap().to_owned();
    let preview_url = uploaded["preview_url"].as_str().unwrap().to_owned();

    let (code, posted) = api(
        router(&state),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "a private pic", "media_ids": [media_id]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    let status_id = posted["id"].as_str().unwrap().to_owned();

    // Both the original and its preview are publicly served while the post lives.
    assert_eq!(fetch_media_file(&state, &url).await, StatusCode::OK);
    assert_eq!(fetch_media_file(&state, &preview_url).await, StatusCode::OK);

    // Delete the status.
    let (code, _) = api(
        router(&state),
        "DELETE",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // The delete captured the files into the durable cleanup queue; the worker
    // then removes them from the store.
    assert!(
        plamenu_db::media_cleanup::pending_count(&pool)
            .await
            .unwrap()
            >= 1,
        "the delete scheduled the files for cleanup"
    );
    assert!(plamenu::media_cleanup_worker::run_due(&state).await >= 1);
    assert_eq!(
        plamenu_db::media_cleanup::pending_count(&pool)
            .await
            .unwrap(),
        0,
        "the worker drained the queue"
    );

    // The formerly public URLs now 404 — the bytes are gone from the origin.
    assert_eq!(fetch_media_file(&state, &url).await, StatusCode::NOT_FOUND);
    assert_eq!(
        fetch_media_file(&state, &preview_url).await,
        StatusCode::NOT_FOUND
    );
}

/// The reconciliation sweep enqueues a file no database row
/// references (an orphan from a pre-fix deletion) while sparing every file that
/// is still referenced.
#[sqlx::test(migrations = "../db/migrations")]
async fn reconcile_enqueues_orphans_and_spares_referenced_files(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    // A referenced file: an uploaded, attached-to-nothing media row still counts.
    let (code, uploaded) = upload(&state, &token, "/api/v1/media", &sample_png(32, 32), &[]).await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    let referenced_url = uploaded["url"].as_str().unwrap().to_owned();
    let referenced_key = referenced_url.rsplit('/').next().unwrap().to_owned();

    // An orphan file on disk that no row references (as if a pre-fix delete left
    // it behind).
    state
        .media
        .put("orphan-abc.jpg", vec![1, 2, 3])
        .await
        .unwrap();

    // Dry run: it is counted but not enqueued.
    let report = plamenu::media_cleanup_worker::reconcile(&state, false)
        .await
        .unwrap();
    assert!(report.orphans >= 1, "the orphan is found: {report:?}");
    assert_eq!(report.enqueued, 0, "dry run enqueues nothing");
    assert_eq!(
        plamenu_db::media_cleanup::pending_count(&pool)
            .await
            .unwrap(),
        0
    );

    // Apply: the orphan is enqueued and the worker removes it; the referenced
    // upload is untouched.
    let applied = plamenu::media_cleanup_worker::reconcile(&state, true)
        .await
        .unwrap();
    assert_eq!(applied.enqueued, applied.orphans);
    plamenu::media_cleanup_worker::run_due(&state).await;
    assert_eq!(
        fetch_media_file(&state, "/media/orphan-abc.jpg").await,
        StatusCode::NOT_FOUND,
        "the orphan is gone"
    );
    assert_eq!(
        fetch_media_file(&state, &referenced_url).await,
        StatusCode::OK,
        "the referenced upload is spared"
    );
    // Sanity: the referenced key really is in the store's referenced set.
    assert!(
        plamenu_db::media_cleanup::referenced_keys(&pool)
            .await
            .unwrap()
            .contains(&referenced_key)
    );
}

/// Knowing an attachment's id must not be enough to fetch its bytes.
///
/// `/media/{file}` is public and unauthenticated by necessity (remote servers
/// fetch our attachments without credentials), so for a followers-only or
/// direct post the file name is the only thing protecting the attachment. When
/// the name was `{media_id}.{ext}` it was not protection at all: snowflake ids
/// are `unix_millis << 16 | counter`, drawn from one counter shared with every
/// public status id and strictly increasing, so an attacker who sees two
/// public status ids bracketing the upload can walk the range against the
/// handful of extensions we emit. The stored name now carries 128 bits of
/// randomness.
#[sqlx::test(migrations = "../db/migrations")]
async fn attachment_file_names_are_not_derivable_from_the_media_id(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let state = test_state_with(pool.clone(), Arc::default());

    let (code, uploaded) = upload(&state, &token, "/api/v1/media", &sample_png(64, 48), &[]).await;
    assert_eq!(code, StatusCode::OK, "{uploaded}");
    let media_id = uploaded["id"].as_str().unwrap().to_owned();
    let url = uploaded["url"].as_str().unwrap().to_owned();
    let preview_url = uploaded["preview_url"].as_str().unwrap().to_owned();

    // The names we actually handed out serve.
    assert_eq!(
        fetch_media_file(&state, &url).await,
        StatusCode::OK,
        "{url}"
    );
    assert_eq!(
        fetch_media_file(&state, &preview_url).await,
        StatusCode::OK,
        "{preview_url}"
    );

    // Every name derivable from the id alone does not. These are exactly the
    // requests the old scheme answered.
    for extension in ["png", "jpg", "jpeg", "webp", "avif", "gif", "mp4", "orig"] {
        for candidate in [
            format!("/media/{media_id}.{extension}"),
            format!("/media/{media_id}.small.{extension}"),
        ] {
            assert_eq!(
                fetch_media_file(&state, &candidate).await,
                StatusCode::NOT_FOUND,
                "{candidate} must not be guessable from the media id",
            );
        }
    }

    // And the served name really is the id plus entropy, inside the character
    // class `/media/{file}` accepts.
    let stem = url
        .rsplit('/')
        .next()
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .to_owned();
    let entropy = stem.strip_prefix(&media_id).expect("id-prefixed stem");
    assert_eq!(entropy.len(), 32, "128 bits of hex entropy: {stem}");
    assert!(
        entropy.bytes().all(|b| b.is_ascii_hexdigit()),
        "the suffix must stay inside the served character class: {stem}"
    );
}
