//! Remote profile text must retain its format without escaping plain fields.
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, test_state_with};
use http_body_util::BodyExt;
use plamenu::{build_router, remote};
use plamenu_db::PgPool;
use serde_json::Value;
use tower::ServiceExt;

#[sqlx::test(migrations = "../db/migrations")]
async fn peertube_channel_profile_renders_name_and_markdown_in_api_and_no_js_page(pool: PgPool) {
    let mut channel = RemoteUser::channel("video.example", "forum_demos");
    channel.actor.name = Some("Forum Demos & <b>Keynotes</b>".into());
    channel.actor.summary = Some(
        "Demos **and keynotes**.\nEach event has [its own playlist](https://video.example/c/forum_demos/video-playlists).\n\n<script>evil()</script>\n\n[bad](javascript:evil()) ![tracker](https://tracker.example/pixel)".into(),
    );
    let stub = StubFederation::with_users(&[&channel]);
    let state = test_state_with(pool.clone(), stub);
    let stored = remote::refresh_remote_actor(&state, &channel.actor)
        .await
        .unwrap();
    assert_eq!(stored.display_name, "Forum Demos & Keynotes");
    assert!(stored.note.contains("<strong>and keynotes</strong>"));
    assert!(
        stored.note.contains("<br"),
        "PeerTube preserves single newlines"
    );
    assert!(
        stored
            .note
            .contains("href=\"https://video.example/c/forum_demos/video-playlists\"")
    );
    for forbidden in [
        "<script",
        "evil()",
        "javascript:",
        "<img",
        "tracker.example",
    ] {
        assert!(!stored.note.contains(forbidden), "{}", stored.note);
    }
    let app = build_router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/accounts/{}", stored.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let account: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(account["display_name"], "Forum Demos & Keynotes");
    assert_eq!(account["note"], stored.note);

    let response = app
        .oneshot(
            Request::get("/!forum_demos@video.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Forum Demos &amp; Keynotes"), "{html}");
    assert!(!html.contains("Forum Demos &amp;amp; Keynotes"));
    assert!(html.contains("<strong>and keynotes</strong>"));
    assert!(html.contains("href=\"https://video.example/c/forum_demos/video-playlists\""));

    // A refresh repairs already-imported presentation text using the same path.
    channel.actor.name = Some("Updated &amp; Keynotes".into());
    channel.actor.summary = Some("Updated [playlist](https://video.example/new)".into());
    let updated = remote::refresh_remote_actor(&state, &channel.actor)
        .await
        .unwrap();
    assert_eq!(updated.id, stored.id);
    assert_eq!(updated.display_name, "Updated & Keynotes");
    assert!(updated.note.contains("href=\"https://video.example/new\""));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn actor_summary_format_is_explicit_or_scoped_to_peertube(pool: PgPool) {
    let mut actor = RemoteUser::new("other.example", "someone").actor;
    actor.summary = Some("<p>Literal **stars** &amp; safe</p>".into());
    let stored = remote::store_remote_actor(&pool, &actor).await.unwrap();
    assert_eq!(stored.note, "<p>Literal **stars** &amp; safe</p>");

    actor.media_type = Some("text/markdown".into());
    actor.summary = Some("**Markdown** [link](https://other.example/link)".into());
    let stored = remote::store_remote_actor(&pool, &actor).await.unwrap();
    assert!(stored.note.contains("<strong>Markdown</strong>"));

    actor.media_type = Some("text/plain".into());
    actor.summary = Some("Literal **stars** <b>text</b> & safe".into());
    let stored = remote::store_remote_actor(&pool, &actor).await.unwrap();
    assert_eq!(
        stored.note,
        "<p>Literal **stars** &lt;b&gt;text&lt;/b&gt; &amp; safe</p>"
    );

    let mut channel = RemoteUser::channel("video.example", "html_channel").actor;
    channel.media_type = Some("text/html".into());
    channel.summary = Some("<p>Literal **stars**</p>".into());
    let stored = remote::store_remote_actor(&pool, &channel).await.unwrap();
    assert_eq!(stored.note, "<p>Literal **stars**</p>");

    channel.media_type = None;
    channel.playlists = Some(serde_json::json!("https://other.example/playlists"));
    let stored = remote::store_remote_actor(&pool, &channel).await.unwrap();
    assert_eq!(stored.note, "<p>Literal **stars**</p>");
}
