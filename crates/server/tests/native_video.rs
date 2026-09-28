//! Native Video is an owned publication, not a renamed attachment Note.
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, DeleteMode, EditParams, PostParams};
use plamenu::error::ApiError;
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, follow, media, scheduled_status};
use tower::ServiceExt;

#[sqlx::test(migrations = "../db/migrations")]
async fn absent_video_description_serializes_as_null_for_peertube(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let media = video_media(&pool, alice.id).await;
    let (stored, _) = actions::post_video(
        &state,
        PostParams {
            text: "",
            ..params("alice", &[media.id])
        },
        channel.id,
    )
    .await
    .unwrap();
    let served = plamenu::note::note_for_status(&state, &stored, &alice)
        .await
        .unwrap();
    assert!(served["content"].is_null());
    assert!(served["contentMap"]["en"].is_null());
    let edited = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some("A description"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        plamenu::note::note_for_status(&state, &edited, &alice)
            .await
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("A description")
    );
    let cleared = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some(""),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        plamenu::note::note_for_status(&state, &cleared, &alice)
            .await
            .unwrap()["content"]
            .is_null()
    );
}

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

async fn channel_change(
    state: &plamenu::AppState,
    actor: &plamenu_db::account::Account,
    channel: i64,
    operation: &str,
    username: Option<&str>,
    role: Option<&str>,
    confirm: Option<&str>,
) -> Result<(), ApiError> {
    plamenu::video_channels::change(
        state,
        actor,
        channel,
        &plamenu::video_channels::Change {
            operation: operation.into(),
            username: username.map(str::to_owned),
            role: role.map(str::to_owned),
            confirm: confirm.map(str::to_owned),
            ..Default::default()
        },
    )
    .await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_collaboration_consent_roles_revocation_and_signer(pool: PgPool) {
    use plamenu_db::video_channel_access as access;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
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
    let alice_media = video_media(&pool, alice.id).await;
    let (alice_video, _) =
        actions::post_video(&state, params("alice", &[alice_media.id]), channel.id)
            .await
            .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(access::role(&pool, channel.id, bob.id).await.unwrap(), None);
    let bob_media = video_media(&pool, bob.id).await;
    assert!(
        actions::post_video(&state, params("bob", &[bob_media.id]), channel.id)
            .await
            .is_err()
    );
    assert!(
        channel_change(&state, &carol, channel.id, "accept", None, None, None)
            .await
            .is_err()
    );
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    let (bob_video, _) = actions::post_video(&state, params("bob", &[bob_media.id]), channel.id)
        .await
        .unwrap();
    assert_eq!(bob_video.account_id, bob.id);
    assert!(
        actions::edit_status(
            &state,
            &bob,
            alice_video.id,
            EditParams {
                title: Some("Not permitted"),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        plamenu::video_metadata::update(
            &state,
            bob.id,
            alice_media.id,
            &plamenu::video_metadata::Input::default()
        )
        .await
        .is_err()
    );
    actions::edit_status(
        &state,
        &bob,
        bob_video.id,
        EditParams {
            title: Some("Own publisher edit"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Upgrade is not ambient new authority: the old role is revoked until consent.
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("editor"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(access::role(&pool, channel.id, bob.id).await.unwrap(), None);
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    let edited = actions::edit_status(
        &state,
        &bob,
        alice_video.id,
        EditParams {
            title: Some("Editor correction"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(edited.account_id, alice.id);
    assert!(
        channel_change(
            &state,
            &bob,
            channel.id,
            "invite",
            Some("carol"),
            Some("editor"),
            None
        )
        .await
        .is_err()
    );
    plamenu::video_metadata::update(
        &state,
        bob.id,
        alice_media.id,
        &plamenu::video_metadata::Input {
            category: Some(serde_json::json!(13)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    plamenu::delivery::run_due(&state).await;
    assert!(
        stub.deliveries()
            .iter()
            .any(|d| d.activity["type"] == "Update"
                && d.activity["object"]["name"] == "Editor correction"
                && d.activity["actor"] == "https://plamenu.test/users/alice")
    );
    channel_change(
        &state,
        &alice,
        channel.id,
        "revoke",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        actions::edit_status(
            &state,
            &bob,
            bob_video.id,
            EditParams {
                title: Some("After revocation"),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        actions::delete_status(&state, &bob, bob_video.id, DeleteMode::Stub)
            .await
            .is_err()
    );
    assert!(
        plamenu_db::video_captions::owned_publication(&pool, bob_media.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
    actions::delete_status(&state, &alice, bob_video.id, DeleteMode::Stub)
        .await
        .unwrap();
    assert!(
        plamenu_db::video::publications(&pool, &[bob_video.id])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_transfer_requires_recipient_and_preserves_authorship(pool: PgPool) {
    use plamenu_db::video_channel_access as access;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let viewer = RemoteUser::new("remote.test", "viewer");
    let viewer = plamenu::remote::store_remote_actor(&pool, &viewer.actor)
        .await
        .unwrap();
    follow::create(&pool, viewer.id, channel.id, None)
        .await
        .unwrap();
    let file = video_media(&pool, alice.id).await;
    let (video, _) = actions::post_video(&state, params("alice", &[file.id]), channel.id)
        .await
        .unwrap();
    let before = plamenu::note::note_for_status(&state, &video, &alice)
        .await
        .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("editor"),
        None,
    )
    .await
    .unwrap();
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "transfer",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        channel_change(
            &state,
            &alice,
            channel.id,
            "accept_transfer",
            None,
            None,
            Some("films")
        )
        .await
        .is_err()
    );
    assert!(
        channel_change(
            &state,
            &bob,
            channel.id,
            "accept_transfer",
            None,
            None,
            Some("wrong")
        )
        .await
        .is_err()
    );
    channel_change(
        &state,
        &bob,
        channel.id,
        "accept_transfer",
        None,
        None,
        Some("films"),
    )
    .await
    .unwrap();
    assert_eq!(
        access::role(&pool, channel.id, alice.id).await.unwrap(),
        None
    );
    assert_eq!(
        access::role(&pool, channel.id, bob.id)
            .await
            .unwrap()
            .as_deref(),
        Some("owner")
    );
    assert!(access::grants(&pool, channel.id).await.unwrap().is_empty());
    let actor = serde_json::to_value(
        plamenu::profile::local_actor(&state, &channel)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(actor["attributedTo"], "https://plamenu.test/users/bob");
    let edited = actions::edit_status(
        &state,
        &bob,
        video.id,
        EditParams {
            title: Some("New owner correction"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(edited.account_id, alice.id);
    let after = plamenu::note::note_for_status(&state, &edited, &alice)
        .await
        .unwrap();
    for field in ["id", "uuid", "attributedTo"] {
        assert_eq!(before[field], after[field]);
    }
    assert!(
        actions::edit_status(
            &state,
            &alice,
            video.id,
            EditParams {
                title: Some("Former owner"),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        actions::delete_status(&state, &alice, video.id, DeleteMode::Stub)
            .await
            .is_err()
    );
    actions::delete_status(&state, &bob, video.id, DeleteMode::Stub)
        .await
        .unwrap();
    let deletes: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        deletes
            .iter()
            .any(|a| a["actor"] == "https://plamenu.test/users/alice")
    );
    assert!(
        deletes
            .iter()
            .any(|a| a["actor"] == "https://plamenu.test/users/bob")
    );
    assert_eq!(
        deletes
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        2
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_deletion_retracts_collaborator_videos_not_accounts(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
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
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    let file = video_media(&pool, bob.id).await;
    let (video, _) = actions::post_video(&state, params("bob", &[file.id]), channel.id)
        .await
        .unwrap();
    assert!(
        channel_change(
            &state,
            &alice,
            channel.id,
            "delete",
            None,
            None,
            Some("wrong")
        )
        .await
        .is_err()
    );
    channel_change(
        &state,
        &alice,
        channel.id,
        "delete",
        None,
        None,
        Some("films"),
    )
    .await
    .unwrap();
    assert!(
        plamenu_db::account::find_by_id(&pool, channel.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
    assert!(
        !plamenu_db::account::find_by_id(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
    assert!(
        plamenu_db::video::publications(&pool, &[video.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        media::find_by_ids(&pool, &[file.id])
            .await
            .unwrap()
            .is_empty()
    );
    plamenu::delivery::run_due(&state).await;
    assert!(
        stub.deliveries()
            .iter()
            .any(|d| d.activity["type"] == "Delete"
                && d.activity["actor"] == "https://plamenu.test/users/bob")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_consent_refusal_suspension_and_quota_gate(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    assert!(
        channel_change(
            &state,
            &alice,
            channel.id,
            "invite",
            Some("films"),
            Some("editor"),
            None
        )
        .await
        .is_err()
    );
    assert!(
        channel_change(
            &state,
            &alice,
            channel.id,
            "invite",
            Some("bob"),
            Some("owner"),
            None
        )
        .await
        .is_err()
    );
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    channel_change(&state, &bob, channel.id, "refuse", None, None, None)
        .await
        .unwrap();
    assert!(
        plamenu_db::video_channel_access::invitations(&pool, bob.id)
            .await
            .unwrap()
            .is_empty()
    );
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    plamenu_db::account::suspend(&pool, bob.id, "local")
        .await
        .unwrap();
    assert!(
        channel_change(&state, &bob, channel.id, "accept", None, None, None)
            .await
            .is_err()
    );
    assert!(
        plamenu_db::video_channel_access::role(&pool, channel.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_revocation_waits_for_committed_publication_permission(pool: PgPool) {
    use plamenu_db::video_channel_access as access;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        access::lock_access(&mut tx, channel.id, bob.id)
            .await
            .unwrap()
            .is_some()
    );
    let state2 = state.clone();
    let alice2 = alice.clone();
    let mut revocation = tokio::spawn(async move {
        channel_change(
            &state2,
            &alice2,
            channel.id,
            "revoke",
            Some("bob"),
            None,
            None,
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut revocation)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), revocation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut after = pool.begin().await.unwrap();
    assert!(
        access::lock_access(&mut after, channel.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_profile_transfer_and_deletion_rollback_with_outbox(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
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
    channel_change(
        &state,
        &alice,
        channel.id,
        "transfer",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("CREATE FUNCTION reject_channel_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected channel outbox failure'; END; $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_channel_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_channel_delivery()").execute(&pool).await.unwrap();
    assert!(
        plamenu::video_channels::change(
            &state,
            &alice,
            channel.id,
            &plamenu::video_channels::Change {
                operation: "profile".into(),
                display_name: Some("Must roll back".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        plamenu_db::account::find_by_id(&pool, channel.id)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Films"
    );
    assert!(
        channel_change(
            &state,
            &bob,
            channel.id,
            "accept_transfer",
            None,
            None,
            Some("films")
        )
        .await
        .is_err()
    );
    assert_eq!(
        plamenu_db::video::channel(&pool, channel.id)
            .await
            .unwrap()
            .unwrap()
            .owner_account_id,
        alice.id
    );
    assert!(
        channel_change(
            &state,
            &alice,
            channel.id,
            "delete",
            None,
            None,
            Some("films")
        )
        .await
        .is_err()
    );
    assert!(
        !plamenu_db::account::find_by_id(&pool, channel.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_collaborator_live_revocation_rejects_keys_and_browser_sessions(pool: PgPool) {
    use plamenu_db::native_live;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "invite",
        Some("bob"),
        Some("publisher"),
        None,
    )
    .await
    .unwrap();
    channel_change(&state, &bob, channel.id, "accept", None, None, None)
        .await
        .unwrap();
    let live = plamenu::native_live::create(
        &state,
        &bob,
        channel.id,
        "Collaborator stream",
        "A live test",
        None,
        "public",
        true,
        native_live::PlaybackOptions {
            dvr_window_seconds: 600,
            latency_mode: 1,
        },
        &[],
    )
    .await
    .unwrap();
    let browser = native_live::claim_owned(&pool, live.status.id, bob.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        native_live::ingest_key_current(&pool, &browser)
            .await
            .unwrap()
    );
    channel_change(
        &state,
        &alice,
        channel.id,
        "revoke",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !native_live::ingest_key_current(&pool, &browser)
            .await
            .unwrap()
    );
    assert!(
        native_live::owned_by_status(&pool, live.status.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !native_live::rotate_key(&pool, live.status.id, bob.id, &[7; 32])
            .await
            .unwrap()
    );
    native_live::finish(&pool, &browser, native_live::FinishReason::Interrupted)
        .await
        .unwrap();
    assert!(
        native_live::claim_owned(&pool, live.status.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_transfer_refuses_scheduled_work_until_cancelled(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let file = video_media(&pool, alice.id).await;
    let queued = plamenu::create_scheduled_video(
        &state,
        &alice,
        time::OffsetDateTime::now_utc() + time::Duration::days(1),
        channel.id,
        file.id,
        "Scheduled video",
        "Later",
        "public",
        None,
        None,
    )
    .await
    .unwrap();
    channel_change(
        &state,
        &alice,
        channel.id,
        "transfer",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        channel_change(
            &state,
            &bob,
            channel.id,
            "accept_transfer",
            None,
            None,
            Some("films")
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert_eq!(
        plamenu_db::video::channel(&pool, channel.id)
            .await
            .unwrap()
            .unwrap()
            .owner_account_id,
        alice.id
    );
    assert!(
        scheduled_status::delete(
            &pool,
            alice.id,
            queued["id"].as_str().unwrap().parse().unwrap()
        )
        .await
        .unwrap()
    );
    channel_change(
        &state,
        &bob,
        channel.id,
        "accept_transfer",
        None,
        None,
        Some("films"),
    )
    .await
    .unwrap();
    // The unattached upload remains its creator's private media, never silently
    // transferred or published by the ownership invitation.
    assert_eq!(
        media::find_by_ids(&pool, &[file.id]).await.unwrap()[0].account_id,
        alice.id
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_transfer_enforces_active_channel_quota_and_deletion_releases_it(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let mut first = None;
    for number in 0..50 {
        let account = create_local_account(&pool, &format!("q_{number}"), "Quota channel").await;
        let mut tx = pool.begin().await.unwrap();
        plamenu_db::video::create_channel(&mut tx, account.id, bob.id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        if number == 0 {
            first = Some(account);
        }
    }
    channel_change(
        &state,
        &alice,
        channel.id,
        "transfer",
        Some("bob"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        channel_change(
            &state,
            &bob,
            channel.id,
            "accept_transfer",
            None,
            None,
            Some("films")
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
    let first = first.unwrap();
    channel_change(&state, &bob, first.id, "delete", None, None, Some("q_0"))
        .await
        .unwrap();
    channel_change(
        &state,
        &bob,
        channel.id,
        "accept_transfer",
        None,
        None,
        Some("films"),
    )
    .await
    .unwrap();
    assert_eq!(
        plamenu_db::video::owned_channels(&pool, bob.id)
            .await
            .unwrap()
            .len(),
        50
    );
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
async fn video_metadata_is_owned_sparse_editable_and_federated(pool: PgPool) {
    use plamenu::video_metadata::{self, Input};
    use serde_json::json;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let remote = RemoteUser::new("remote.test", "viewer");
    let state = test_state_with(pool.clone(), StubFederation::with_users(&[&remote]));
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
    let film = video_media(&pool, alice.id).await;
    assert!(
        video_metadata::draft(
            &state,
            bob.id,
            film.id,
            &Input {
                category: Some(json!(13)),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let metadata = video_metadata::draft(
        &state,
        alice.id,
        film.id,
        &Input {
            category: Some(json!(13)),
            licence: Some(json!(2)),
            spoiler_text: Some("Flashing imagery".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (posted, _) = actions::post_video(
        &state,
        PostParams {
            spoiler_text: &metadata.spoiler_text,
            sensitive: metadata.sensitive,
            text: "An #astronomy lesson.",
            ..params("alice", &[film.id])
        },
        channel.id,
    )
    .await
    .unwrap();
    let wire = plamenu::note::note_for_status(&state, &posted, &alice)
        .await
        .unwrap();
    assert_eq!(wire["category"]["identifier"], "13");
    assert_eq!(wire["licence"]["identifier"], "2");
    assert_eq!(
        wire["licence"]["url"],
        video_metadata::licence_url(2).unwrap()
    );
    assert_eq!(wire["sensitive"], true);
    assert_eq!(wire["summary"], "Flashing imagery");
    let created: serde_json::Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type' = 'Create' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for field in ["category", "licence", "tag", "sensitive", "summary"] {
        assert_eq!(created["object"][field], wire[field], "{field}");
    }
    assert!(
        wire["tag"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag["type"] == "Hashtag" && tag["name"] == "astronomy")
    );
    assert!(
        video_metadata::update(
            &state,
            bob.id,
            film.id,
            &Input {
                licence: Some(json!(9)),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        video_metadata::draft(
            &state,
            alice.id,
            film.id,
            &Input {
                licence: Some(json!(9)),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    video_metadata::update(
        &state,
        alice.id,
        film.id,
        &Input {
            category: Some(json!(15)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updated = plamenu_db::status::find_by_id(&pool, posted.id)
        .await
        .unwrap()
        .unwrap();
    let wire = plamenu::note::note_for_status(&state, &updated, &alice)
        .await
        .unwrap();
    assert_eq!(wire["category"]["identifier"], "15");
    assert_eq!(wire["licence"]["identifier"], "2");
    let before = updated.edited_at;
    let queued: serde_json::Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type' = 'Update' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(queued["object"]["category"]["identifier"], "15");
    assert_eq!(queued["object"]["licence"]["identifier"], "2");
    // Delivery signs the queued canonical payload with this owner's keys.
    assert_eq!(
        queued["actor"],
        format!("https://{}/users/alice", state.config.domain)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT account_id FROM delivery_jobs WHERE activity->>'type' = 'Update' LIMIT 1"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        alice.id
    );
    let job_count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    video_metadata::update(
        &state,
        alice.id,
        film.id,
        &Input {
            category: Some(json!(15)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        plamenu_db::status::find_by_id(&pool, posted.id)
            .await
            .unwrap()
            .unwrap()
            .edited_at,
        before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        job_count
    );
    sqlx::query("CREATE FUNCTION reject_metadata_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected metadata outbox failure'; END; $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_metadata_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_metadata_delivery()").execute(&pool).await.unwrap();
    assert!(
        video_metadata::update(
            &state,
            alice.id,
            film.id,
            &Input {
                licence: Some(json!(9)),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    sqlx::query("DROP TRIGGER reject_metadata_delivery ON delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    let preserved = plamenu_db::video_metadata::for_media(&pool, &[film.id])
        .await
        .unwrap()
        .remove(&film.id)
        .unwrap();
    assert_eq!(preserved.licence, Some(2));
    assert_eq!(
        plamenu_db::status::find_by_id(&pool, posted.id)
            .await
            .unwrap()
            .unwrap()
            .edited_at,
        before
    );
    video_metadata::update(
        &state,
        alice.id,
        film.id,
        &Input {
            licence: Some(json!(0)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updated = plamenu_db::status::find_by_id(&pool, posted.id)
        .await
        .unwrap()
        .unwrap();
    let wire = plamenu::note::note_for_status(&state, &updated, &alice)
        .await
        .unwrap();
    assert_eq!(wire["licence"]["identifier"], "0");
    assert!(wire["licence"].get("url").is_none());
    let entity =
        plamenu::entities::render_status(&pool, &state.config.domain, &updated, Some(alice.id))
            .await
            .unwrap();
    assert_eq!(
        entity["media_attachments"][0]["video_metadata"]["category"]["id"],
        15
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduled_video_reserves_media_and_publishes_once_when_due(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let film = video_media(&pool, alice.id).await;
    plamenu::video_metadata::draft(
        &state,
        alice.id,
        film.id,
        &plamenu::video_metadata::Input {
            category: Some(serde_json::json!(13)),
            licence: Some(serde_json::json!(2)),
            spoiler_text: Some("Flashing imagery".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let due = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let queued = plamenu::create_scheduled_video(
        &state,
        &alice,
        due,
        channel.id,
        film.id,
        "Scheduled film",
        "Coming soon.",
        "public",
        Some("en"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(queued["params"]["post_kind"], "video");
    assert_eq!(queued["params"]["channel_id"], channel.id.to_string());
    assert_eq!(queued["params"]["spoiler_text"], "Flashing imagery");
    assert_eq!(queued["params"]["sensitive"], true);
    assert_eq!(
        queued["media_attachments"][0]["video_metadata"]["licence"]["id"],
        2
    );
    let id: i64 = queued["id"].as_str().unwrap().parse().unwrap();
    assert!(matches!(
        plamenu::create_scheduled_video(
            &state,
            &alice,
            due,
            channel.id,
            film.id,
            "Another schedule",
            "Not allowed.",
            "public",
            Some("en"),
            None,
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
    assert!(matches!(
        actions::post_video(&state, params("alice", &[film.id]), channel.id).await,
        Err(ApiError::Unprocessable(_))
    ));
    assert!(
        media::delete_unattached(&pool, film.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
    let reserved: Option<i64> =
        sqlx::query_scalar("SELECT scheduled_status_id FROM media_attachments WHERE id = $1")
            .bind(film.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reserved, Some(id));
    sqlx::query(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 1);
    assert_eq!(plamenu::scheduled_status_publish::run_due(&state).await, 0);
    assert!(
        scheduled_status::find_for_account(&pool, alice.id, id)
            .await
            .unwrap()
            .is_none()
    );
    let status_id: i64 =
        sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id = $1")
            .bind(film.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let published = plamenu_db::video::publications(&pool, &[status_id])
        .await
        .unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].channel_id, channel.id);
    let status = plamenu_db::status::find_by_id(&pool, status_id)
        .await
        .unwrap()
        .unwrap();
    let wire = plamenu::note::note_for_status(&state, &status, &alice)
        .await
        .unwrap();
    assert_eq!(wire["category"]["identifier"], "13");
    assert_eq!(wire["licence"]["identifier"], "2");
    assert_eq!(wire["summary"], "Flashing imagery");
    assert_eq!(wire["sensitive"], true);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn imported_video_metadata_rejects_untrusted_labels_links_and_unknown_ids(pool: PgPool) {
    use serde_json::json;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let film = video_media(&pool, alice.id).await;
    let (posted, _) = actions::post_video(&state, params("alice", &[film.id]), channel.id)
        .await
        .unwrap();
    // Exercise the inbound metadata adapter separately from transport/signature
    // checks, which are covered by the real PeerTube federation suite.
    plamenu::video_metadata::ingest(
        &state,
        posted.id,
        &json!({"type":"Video",
            "category":{"identifier":"13", "name":"<script>not education</script>"},
            "licence":{"identifier":"2", "name":"All rights granted", "url":"https://evil.invalid/"}
        }),
    )
    .await
    .unwrap();
    let entity =
        plamenu::entities::render_status(&pool, &state.config.domain, &posted, Some(alice.id))
            .await
            .unwrap();
    let metadata = &entity["media_attachments"][0]["video_metadata"];
    assert_eq!(metadata["category"]["name"], "Education");
    assert_eq!(metadata["licence"]["name"], "CC BY-SA");
    assert!(metadata["licence"]["url"].is_null());
    plamenu::video_metadata::ingest(
        &state,
        posted.id,
        &json!({"type":"Video",
            "licence":{"identifier":"2", "url":plamenu::video_metadata::licence_url(2)}
        }),
    )
    .await
    .unwrap();
    let metadata = plamenu_db::video_metadata::for_media(&pool, &[film.id])
        .await
        .unwrap()
        .remove(&film.id)
        .unwrap();
    assert!(metadata.category.is_none());
    assert!(metadata.licence_version_known);
    plamenu::video_metadata::ingest(
        &state,
        posted.id,
        &json!({"type":"Video",
            "category":{"identifier":19}, "licence":{"identifier":"2junk"}
        }),
    )
    .await
    .unwrap();
    let metadata = plamenu_db::video_metadata::for_media(&pool, &[film.id])
        .await
        .unwrap()
        .remove(&film.id)
        .unwrap();
    assert!(metadata.category.is_none() && metadata.licence.is_none());
    assert!(!metadata.licence_version_known);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cancelling_scheduled_video_releases_its_media(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let film = video_media(&pool, alice.id).await;
    let due = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let queued = plamenu::create_scheduled_video(
        &state,
        &alice,
        due,
        channel.id,
        film.id,
        "Scheduled film",
        "Coming soon.",
        "public",
        Some("en"),
        None,
    )
    .await
    .unwrap();
    let id: i64 = queued["id"].as_str().unwrap().parse().unwrap();
    assert!(scheduled_status::delete(&pool, alice.id, id).await.unwrap());
    let (posted, _) = actions::post_video(&state, params("alice", &[film.id]), channel.id)
        .await
        .unwrap();
    assert_eq!(posted.object_type.as_deref(), Some("Video"));
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
