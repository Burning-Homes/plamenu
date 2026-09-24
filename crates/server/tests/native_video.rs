//! Native Video is an owned publication, not a renamed attachment Note.
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, DeleteMode, EditParams, PostParams};
use plamenu::error::ApiError;
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, follow, media};
use tower::ServiceExt;

#[tokio::test]
async fn spooled_video_is_not_truncated_at_the_old_attachment_frame_limit() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("long.mkv");
    // 36,120 frames, just above the old silently truncating 36,000-frame
    // cutoff. Tiny lossless frames keep this real-encoder regression cheap.
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=16x16:rate=120",
                "-t",
                "301",
                "-c:v",
                "ffv1"
            ])
            .arg(&input)
            .status()
            .unwrap()
            .success()
    );
    let original_size = std::fs::metadata(&input).unwrap().len();
    let mut config = common::test_config();
    config.media_dir = dir.path().join("media");
    let mut params = plamenu::media_transcode::TranscodeParams {
        max_av_bytes: 1,
        ..Default::default()
    };
    assert!(matches!(
        plamenu::media_transcode::process_av_file(&config, &input, &params).await,
        Err(ApiError::Unprocessable(_))
    ));
    params.max_av_bytes = usize::try_from(original_size).unwrap();
    let output = plamenu::media_transcode::process_av_file(&config, &input, &params)
        .await
        .unwrap();
    assert!((output.duration.unwrap() - 301.0).abs() < 0.1);
    assert_eq!(output.kind, "video");
    assert_eq!(std::fs::metadata(&input).unwrap().len(), original_size);
    let path = output.file_path.clone();
    drop(output);
    assert!(!path.exists());
    assert!(input.exists());
}

#[tokio::test]
async fn native_hls_packages_copy_top_and_bounded_lower_rendition() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.mp4");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x480:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "9",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "48",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&input)
            .status()
            .unwrap()
            .success()
    );
    let mut config = common::test_config();
    config.media_dir = dir.path().join("media");
    let processed = plamenu::media_transcode::process_av_file(
        &config,
        &input,
        &plamenu::media_transcode::TranscodeParams::default(),
    )
    .await
    .unwrap();
    let renditions = plamenu::media_transcode::package_native_hls(
        &config,
        &processed,
        "A123",
        &plamenu::media_transcode::TranscodeParams::default(),
    )
    .await
    .unwrap();
    assert_eq!(renditions.len(), 2);
    assert_eq!(
        renditions.iter().map(|r| r.height).collect::<Vec<_>>(),
        [480, 360]
    );
    for rendition in &renditions {
        assert!(rendition.file_size > 1000);
        assert!(rendition.bandwidth > 0);
        assert!(rendition.playlist.contains("#EXT-X-BYTERANGE:"));
        assert!(
            rendition
                .playlist
                .contains(&format!("/media/{}", rendition.file_name))
        );
        assert!(
            !rendition
                .playlist
                .lines()
                .any(|line| line == "hls480.m4s" || line == "hls360.m4s")
        );
        let meta = plamenu::media_transcode::probe(&config, &rendition.file_path)
            .await
            .unwrap();
        assert_eq!(meta.height, Some(i64::from(rendition.height)));
    }
}

async fn ap_get(app: axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_channel_subscriptions_preserve_audience_filters_and_deletion(pool: PgPool) {
    use plamenu_db::{status, user::TimelineOrder};
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let channel_entity =
        plamenu::entities::account_json(&pool, &state.config.domain, &channel, Some(bob.id))
            .await
            .unwrap();
    assert_eq!(channel_entity["group"], true);
    assert_eq!(channel_entity["video_channel"], true);
    assert!(
        channel_entity["noindex"].is_null(),
        "channels have no login preference"
    );
    assert_eq!(channel_entity["roles"], serde_json::json!([]));
    let batch = plamenu::entities::render_accounts(
        &pool,
        &state.config.domain,
        &[channel.clone(), alice.clone()],
        Some(bob.id),
    )
    .await
    .unwrap();
    assert_eq!(batch[0], channel_entity);
    assert_eq!(batch[1]["video_channel"], false);
    follow::create(&pool, bob.id, channel.id, None)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for visibility in ["public", "unlisted"] {
        let media = video_media(&pool, alice.id).await;
        // The shared client upload API calls a short silent clip `gifv`.
        // Native publication must still expose a non-looping film.
        sqlx::query("UPDATE media_attachments SET kind = 'gifv' WHERE id = $1")
            .bind(media.id)
            .execute(&pool)
            .await
            .unwrap();
        let (posted, _) = actions::post_video(
            &state,
            PostParams {
                visibility,
                ..params("alice", &[media.id])
            },
            channel.id,
        )
        .await
        .unwrap();
        let entity =
            plamenu::entities::render_status(&pool, &state.config.domain, &posted, Some(alice.id))
                .await
                .unwrap();
        assert_eq!(entity["media_attachments"][0]["type"], "video");
        assert_eq!(entity["group_post"], false);
        assert_eq!(entity["groups"], serde_json::json!([]));
        assert!(
            !plamenu::groups::is_group_post(&state, posted.id)
                .await
                .unwrap()
        );
        assert!(
            plamenu_db::group::boosting_group_ids(&pool, posted.id)
                .await
                .unwrap()
                .is_empty()
        );
        let boost = status::find_reblog_by(&pool, channel.id, posted.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(boost.visibility, visibility);
        assert!(boost.uri.as_deref().unwrap().ends_with("/announce"));
        let boost_entity =
            plamenu::entities::render_status(&pool, &state.config.domain, &boost, Some(bob.id))
                .await
                .unwrap();
        assert_eq!(boost_entity["account"]["video_channel"], true);
        assert_eq!(boost_entity["reblog"]["account"]["video_channel"], false);
        ids.push(posted.id);
    }
    for order in [TimelineOrder::Published, TimelineOrder::Received] {
        let home = status::home_timeline(&pool, bob.id, order, None, 20)
            .await
            .unwrap();
        assert_eq!(home.len(), 2);
        assert!(
            home.iter()
                .all(|s| s.account_id == channel.id && ids.contains(&s.reblog_of_id.unwrap()))
        );
        let profile = status::by_account(
            &pool,
            channel.id,
            Some(bob.id),
            &status::AccountStatusesFilter::default(),
            order,
            20,
        )
        .await
        .unwrap();
        assert_eq!(profile.len(), 2);
        let discovery = status::public_timeline(&pool, true, Some(bob.id), false, order, None, 20)
            .await
            .unwrap();
        assert!(
            discovery
                .iter()
                .all(|s| s.id != ids[1] && s.reblog_of_id != Some(ids[1]))
        );
    }
    assert!(
        !plamenu_db::group::public_group_ids(&pool, 20, 0)
            .await
            .unwrap()
            .contains(&channel.id)
    );
    assert!(
        !plamenu_db::group::joined_group_ids(&pool, bob.id)
            .await
            .unwrap()
            .contains(&channel.id)
    );
    // Channel membership never bypasses the viewer's block of the creator.
    plamenu_db::block::create(&pool, bob.id, alice.id, None)
        .await
        .unwrap();
    for order in [TimelineOrder::Published, TimelineOrder::Received] {
        assert!(
            status::home_timeline(&pool, bob.id, order, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            status::by_account(
                &pool,
                channel.id,
                Some(bob.id),
                &status::AccountStatusesFilter::default(),
                order,
                20
            )
            .await
            .unwrap()
            .is_empty()
        );
    }
    actions::delete_status(&state, &alice, ids[0], DeleteMode::Wipe)
        .await
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, channel.id, ids[0])
            .await
            .unwrap()
            .is_none()
    );
    let app = plamenu::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/@films")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("A native video"));
    assert!(html.contains("Video channel"));
    assert!(html.contains("@films"));
    assert!(!html.contains("!films"));
    assert!(!html.contains("title=\"Group\""));
    assert!(html.contains("published on"));
    assert!(!html.contains("?sort=hot"));
    assert!(!html.contains("?sort=active"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn native_video_metadata_preserves_html_and_uses_a_long_description_budget(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let media = video_media(&pool, alice.id).await;
    for title in ["x", "xy", "   "] {
        assert!(matches!(
            actions::post_video(
                &state,
                PostParams {
                    title: Some(title),
                    ..params("alice", &[media.id])
                },
                channel.id
            )
            .await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    let text = format!("{} *literal asterisks*", "A long description. ".repeat(100));
    let (posted, _) = actions::post_video(
        &state,
        PostParams {
            text: &text,
            ..params("alice", &[media.id])
        },
        channel.id,
    )
    .await
    .unwrap();
    let object = plamenu::note::note_for_status(&state, &posted, &alice)
        .await
        .unwrap();
    assert_eq!(object["mediaType"], "text/html");
    assert_eq!(object["content"], posted.content);
    assert!(
        object["content"]
            .as_str()
            .unwrap()
            .contains("*literal asterisks*")
    );

    let escaped = "&".repeat(3000); // source fits, serialized HTML exceeds the peer's bound
    assert!(matches!(
        actions::edit_status(
            &state,
            &alice,
            posted.id,
            EditParams {
                text: Some(&escaped),
                ..Default::default()
            }
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
    assert_eq!(
        plamenu_db::status::find_by_id(&pool, posted.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        posted.content
    );
    let extra = video_media(&pool, alice.id).await;
    assert!(matches!(
        actions::post_video(
            &state,
            PostParams {
                text: &escaped,
                ..params("alice", &[extra.id])
            },
            channel.id
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
    assert!(
        media::find_owned(&pool, extra.id, alice.id)
            .await
            .unwrap()
            .unwrap()
            .status_id
            .is_none()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let edited = actions::edit_status(
        &state,
        &alice,
        posted.id,
        EditParams {
            text: Some("x"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let object = plamenu::note::note_for_status(&state, &edited, &alice)
        .await
        .unwrap();
    assert_eq!(object["content"], "<p>x</p>");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_history_requires_the_signature_to_cover_the_entire_path(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("peer.test", "viewer");
    let stub = StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), stub.clone());
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let app = common::test_app_secure(pool, stub);
    let path = format!("/ap/accounts/{}/outbox/page", channel.id);
    assert_eq!(ap_get(app.clone(), &path).await.0, StatusCode::UNAUTHORIZED);
    let signer = remote.signer();
    for (signed_path, requested_path, expected) in [
        (path.clone(), path.clone(), StatusCode::OK),
        (path.clone(), format!("{path}/1"), StatusCode::UNAUTHORIZED),
        (
            path.clone(),
            format!("{path}?max_id=1"),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let signature = signer.sign_get(
            common::TEST_DOMAIN,
            &signed_path,
            "application/activity+json",
            std::time::SystemTime::now(),
        );
        let request = Request::builder()
            .uri(requested_path)
            .header("accept", "application/activity+json")
            .header("host", common::TEST_DOMAIN)
            .header("date", signature.date)
            .header("signature", signature.signature)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn compatible_poster_is_cached_preserves_avif_and_follows_media_deletion(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let media = video_media(&pool, alice.id).await;
    // Use the actual video pipeline: its FFmpeg AVIF muxer strips temporal
    // delimiters, unlike the still-image serializer used by earlier tests.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("video.mp4");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&input)
            .status()
            .unwrap()
            .success()
    );
    let processed = plamenu::media_transcode::process_av(
        &state.config,
        &std::fs::read(input).unwrap(),
        &plamenu::media_transcode::TranscodeParams::default(),
    )
    .await
    .unwrap();
    let avif = processed.small.unwrap().bytes;
    state.media.put("video.avif", avif.clone()).await.unwrap();
    sqlx::query("UPDATE media_attachments SET small_file_name = 'video.avif', small_width = 320, small_height = 240 WHERE id = $1")
        .bind(media.id).execute(&pool).await.unwrap();
    let media = media::find_owned(&pool, media.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    let (a, b) = tokio::join!(
        plamenu::video::ensure_compatible_poster(&state, &media),
        plamenu::video::ensure_compatible_poster(&state, &media)
    );
    a.unwrap();
    b.unwrap();
    let key: String =
        sqlx::query_scalar("SELECT file_name FROM video_compatibility_posters WHERE media_id = $1")
            .bind(media.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state.media.get("video.avif").await.unwrap(), avif);
    let poster = state.media.get(&key).await.unwrap();
    assert_eq!(
        image::guess_format(&poster).unwrap(),
        image::ImageFormat::Jpeg
    );
    let decoded = image::load_from_memory(&poster).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (640, 360));
    assert!(decoded.get_pixel(0, 180).0.iter().all(|&c| c < 5));
    assert!(decoded.get_pixel(320, 180).0.iter().any(|&c| c > 50));
    assert!(
        plamenu_db::media_cleanup::referenced_keys(&pool)
            .await
            .unwrap()
            .contains(&key)
    );
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!media::public_file_allowed(&pool, &key).await.unwrap());
    media::delete_unattached(&pool, media.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu_db::media_cleanup::pending_count(&pool)
            .await
            .unwrap(),
        1
    );
    plamenu::media_cleanup_worker::run_due(&state).await;
    assert!(state.media.get(&key).await.is_err());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_outbox_uses_path_cursors_and_hides_suspended_owners(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..21 {
        let media = video_media(&pool, alice.id).await;
        ids.push(
            actions::post_video(&state, params("alice", &[media.id]), channel.id)
                .await
                .unwrap()
                .0
                .id,
        );
    }
    let app = plamenu::build_router(state);
    let path = format!("/ap/accounts/{}/outbox", channel.id);
    let (code, envelope) = ap_get(app.clone(), &path).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(envelope["totalItems"], 21);
    let first = envelope["first"]
        .as_str()
        .unwrap()
        .strip_prefix("https://plamenu.test")
        .unwrap();
    assert_eq!(first, format!("{path}/page"));
    let (code, page) = ap_get(app.clone(), first).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(page["orderedItems"].as_array().unwrap().len(), 20);
    assert_eq!(page["orderedItems"][0]["type"], "Announce");
    assert_eq!(
        page["orderedItems"][0]["actor"],
        channel.uri.as_deref().unwrap()
    );
    let next = page["next"]
        .as_str()
        .unwrap()
        .strip_prefix("https://plamenu.test")
        .unwrap();
    assert!(!next.contains('?'));
    let (_, last) = ap_get(app.clone(), next).await;
    assert_eq!(last["orderedItems"].as_array().unwrap().len(), 1);
    assert_eq!(
        last["orderedItems"][0]["object"],
        format!("https://plamenu.test/users/alice/statuses/{}", ids[0])
    );
    assert!(last.get("next").is_none());
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, hidden) = ap_get(app.clone(), first).await;
    assert_eq!(hidden["orderedItems"], serde_json::json!([]));
    let (_, hidden) = ap_get(app, &path).await;
    assert_eq!(hidden["totalItems"], 0);
}

async fn video_media(pool: &PgPool, account_id: i64) -> media::Media {
    media::create_local(
        pool,
        media::NewLocalMedia {
            kind: "video",
            width: Some(320),
            height: Some(240),
            duration: Some(5.1),
            small: Some(media::SmallStyle {
                file_name: "video.jpg",
                width: 320,
                height: 180,
            }),
            description: Some("A blue circle moving across the screen"),
            ..media::NewLocalMedia::new(
                account_id,
                plamenu_db::id::next(),
                "video.mp4",
                "video/mp4",
            )
        },
    )
    .await
    .unwrap()
}

fn params<'a>(username: &'a str, media_ids: &'a [i64]) -> PostParams<'a> {
    PostParams {
        username,
        media_ids,
        title: Some("A native video"),
        text: "A short description.",
        visibility: "public",
        language: Some("en"),
        kind: PostKind::Video,
        ..Default::default()
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_is_owned_group_not_community(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Alice's films")
        .await
        .unwrap();
    let actor = plamenu::profile::local_actor(&state, &channel)
        .await
        .unwrap();
    let document = serde_json::to_value(actor).unwrap();
    assert_eq!(document["type"], "Group");
    assert_eq!(document["attributedTo"], "https://plamenu.test/users/alice");
    assert!(document.get("postingPolicy").is_none());
    assert!(document.get("postingRestrictedToMods").is_none());
    assert!(
        plamenu_db::group::find(&pool, channel.id)
            .await
            .unwrap()
            .is_none()
    );
    let owned = plamenu_db::video::owned_channels(&pool, alice.id)
        .await
        .unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].account_id, channel.id);
    assert!(
        plamenu::video::create_channel(&state, &channel, "nested", "Nested")
            .await
            .is_err()
    );
    for bad in ["", "bad.name", "bad-name", "has space", "../channel"] {
        assert!(
            plamenu::video::create_channel(&state, &alice, bad, "Invalid")
                .await
                .is_err()
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_serve_edit_delete_agree_and_reach_channel_followers(pool: PgPool) {
    let alice = common::create_immutable_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("remote.test", "viewer");
    let stub = StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), stub.clone());
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let viewer = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    follow::create(&pool, viewer.id, channel.id, None)
        .await
        .unwrap();
    follow::create(&pool, viewer.id, alice.id, None)
        .await
        .unwrap();
    let media = video_media(&pool, alice.id).await;
    let (stored, _) = actions::post_video(&state, params("alice", &[media.id]), channel.id)
        .await
        .unwrap();
    assert_eq!(stored.object_type.as_deref(), Some("Video"));
    let served = plamenu::note::note_for_status(&state, &stored, &alice)
        .await
        .unwrap();
    assert_eq!(served["type"], "Video");
    assert_eq!(served["name"], "A native video");
    assert_eq!(served["duration"], "PT6S");
    assert_eq!(
        served["attributedTo"][1]["id"],
        channel.uri.as_deref().unwrap()
    );
    assert_eq!(served["url"][1]["mediaType"], "video/mp4");
    assert_eq!(served["icon"][0]["mediaType"], "image/jpeg");
    assert_eq!(served["attachment"][0]["mediaType"], "video/mp4");
    plamenu::delivery::run_due(&state).await;
    let deliveries = stub.deliveries();
    let create = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Create")
        .unwrap();
    assert_eq!(create.activity["actor"], alice.uri.as_deref().unwrap());
    for field in [
        "type",
        "uuid",
        "name",
        "url",
        "icon",
        "duration",
        "attributedTo",
        "content",
        "@context",
    ] {
        assert_eq!(create.activity["object"][field], served[field], "{field}");
    }
    let announce = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Announce")
        .unwrap();
    assert_eq!(announce.activity["actor"], channel.uri.as_deref().unwrap());
    assert_eq!(announce.activity["object"], served["id"]);
    // Only the channel subscription remains: updates/deletes must not rely on
    // the viewer also following the owning Person.
    sqlx::query("DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2")
        .bind(viewer.id)
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    let edited = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            title: Some("Corrected video title"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let revised = plamenu::note::note_for_status(&state, &edited, &alice)
        .await
        .unwrap();
    assert_eq!(revised["name"], "Corrected video title");
    assert_eq!(revised["uuid"], served["uuid"]);
    plamenu::delivery::run_due(&state).await;
    assert!(
        stub.deliveries()
            .iter()
            .any(|d| d.activity["type"] == "Update"
                && d.activity["object"]["name"] == "Corrected video title")
    );
    actions::delete_status(&state, &alice, stored.id, DeleteMode::Wipe)
        .await
        .unwrap();
    plamenu::delivery::run_due(&state).await;
    assert!(
        stub.deliveries()
            .iter()
            .any(|d| d.activity["type"] == "Delete" && d.activity["object"]["id"] == served["id"])
    );
    assert!(
        plamenu_db::video::publications(&pool, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn publication_is_owned_atomic_and_cannot_lose_its_media(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let bobs_media = video_media(&pool, bob.id).await;
    let refused = actions::post_video(&state, params("bob", &[bobs_media.id]), channel.id).await;
    assert!(matches!(refused, Err(ApiError::Forbidden(_))));
    let attached: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id = $1")
            .bind(bobs_media.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(attached.is_none());
    assert!(
        actions::post_status(&state, params("bob", &[bobs_media.id]))
            .await
            .is_err()
    );
    let media = video_media(&pool, alice.id).await;
    let (stored, _) = actions::post_video(&state, params("alice", &[media.id]), channel.id)
        .await
        .unwrap();
    let result = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            media_ids: Some(Vec::new()),
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(result, Err(ApiError::Unprocessable(_))));
    let huge = "x".repeat(121);
    assert!(
        actions::edit_status(
            &state,
            &alice,
            stored.id,
            EditParams {
                title: Some(&huge),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        actions::post_video(&state, params("alice", &[media.id]), channel.id)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn suspended_channel_cannot_publish_and_owner_deletion_retires_channels(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let media = video_media(&pool, alice.id).await;
    plamenu_db::account::suspend_conn(&mut pool.acquire().await.unwrap(), channel.id, "local")
        .await
        .unwrap();
    assert!(matches!(
        actions::post_video(&state, params("alice", &[media.id]), channel.id).await,
        Err(ApiError::Forbidden(_))
    ));
    plamenu::moderation::self_delete_account(&state, &alice)
        .await
        .unwrap();
    assert!(
        plamenu_db::account::is_deleted(&pool, channel.id)
            .await
            .unwrap()
    );
}
