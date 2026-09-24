//! Durable jobs exercised with real `FFmpeg` and isolated PostgreSQL databases.
#![cfg(unix)]
mod common;

use std::{path::Path, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::storage::{LocalDiskStore, MediaRead, MediaStore};
use plamenu::{AppState, federation::BoxFuture, media_limits::FileSizeLimit};
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, id, media, video_draft};
use tokio::sync::Semaphore;
use tower::ServiceExt;

struct GatedStore {
    disk: LocalDiskStore,
    gate_read: bool,
    gate_write: bool,
    fail_poster: bool,
    entered: Semaphore,
    release: Semaphore,
}

impl MediaStore for GatedStore {
    fn put(&self, name: &str, bytes: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        if self.fail_poster
            && Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("avif"))
        {
            return Box::pin(async { Err(std::io::Error::other("fixture poster failure")) });
        }
        self.disk.put(name, bytes)
    }

    fn put_file(&self, name: &str, path: &Path) -> BoxFuture<'_, std::io::Result<()>> {
        let name = name.to_owned();
        let path = path.to_owned();
        Box::pin(async move {
            if self.gate_write {
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
            }
            self.disk.put_file(&name, &path).await
        })
    }

    fn open(&self, name: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        let name = name.to_owned();
        Box::pin(async move {
            if self.gate_read {
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
            }
            self.disk.open(&name).await
        })
    }

    fn get(&self, name: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.disk.get(name)
    }
    fn delete(&self, name: &str) -> BoxFuture<'_, std::io::Result<()>> {
        self.disk.delete(name)
    }
    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        self.disk.list()
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    state: AppState,
    store: Arc<GatedStore>,
    owner: i64,
    media_id: i64,
    source: String,
    original: Vec<u8>,
    expected_duration: f64,
}

#[derive(Clone, Copy)]
enum SourceShape {
    Video,
    Audio,
    SilentPortrait,
    SilentUltrawide,
    ColourCut,
}

#[allow(
    clippy::too_many_lines,
    reason = "single fixture assembles one fenced video job"
)]
async fn fixture(pool: PgPool, gate_read: bool, gate_write: bool, fail_poster: bool) -> Fixture {
    fixture_with_source(pool, gate_read, gate_write, fail_poster, SourceShape::Video).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one fixture assembles a fenced native-video job"
)]
async fn fixture_with_source(
    pool: PgPool,
    gate_read: bool,
    gate_write: bool,
    fail_poster: bool,
    source_shape: SourceShape,
) -> Fixture {
    fixture_with_source_and_time(pool, gate_read, gate_write, fail_poster, source_shape, 0.0).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one fixture assembles a fenced native-video job"
)]
async fn fixture_with_source_and_time(
    pool: PgPool,
    gate_read: bool,
    gate_write: bool,
    fail_poster: bool,
    source_shape: SourceShape,
    poster_time_seconds: f64,
) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let input = temp
        .path()
        .join(if matches!(source_shape, SourceShape::Audio) {
            "input.m4a"
        } else {
            "input.mp4"
        });
    let mut encoder = std::process::Command::new("ffmpeg");
    encoder.args(["-nostdin", "-v", "error", "-f", "lavfi"]);
    if matches!(source_shape, SourceShape::Audio) {
        encoder.args(["-i", "sine=frequency=440:duration=9", "-c:a", "aac"]);
    } else if matches!(source_shape, SourceShape::ColourCut) {
        encoder.args([
            "-i",
            "color=c=red:size=64x64:rate=5:duration=2",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:size=64x64:rate=5:duration=2",
            "-filter_complex",
            "[0:v][1:v]concat=n=2:v=1:a=0[v]",
            "-map",
            "[v]",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]);
    } else {
        let size = match source_shape {
            SourceShape::SilentPortrait => "270x480",
            SourceShape::SilentUltrawide => "1000x250",
            _ => "64x64",
        };
        encoder.args([
            "-i",
            &format!("testsrc2=size={size}:rate=10"),
            "-t",
            "1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]);
    }
    assert!(encoder.arg("-y").arg(&input).status().unwrap().success());
    let original = std::fs::read(input).unwrap();
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let store = Arc::new(GatedStore {
        disk: LocalDiskStore::new(config.media_dir.clone()).unwrap(),
        gate_read,
        gate_write,
        fail_poster,
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let state = AppState::new(
        pool.clone(),
        config,
        common::StubFederation::with_actors([]),
        store.clone(),
    )
    .unwrap();
    let owner = common::create_local_account(&pool, "author", "Author").await;
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let media_id = id::next();
    let source = format!("{media_id}.orig");
    let bytes = i64::try_from(original.len()).unwrap();
    let accepted = video_draft::reserve(
        &pool,
        &video_draft::NewDraft {
            media_id,
            account_id: owner.id,
            channel_id: channel.id,
            title: "Durable film",
            description: "Description",
            language: Some("en"),
            visibility: "unlisted",
            source_file: &source,
            source_fingerprint: None,
            poster_time_seconds,
            media_description: None,
            media_transcript: None,
            caption_vtt: None,
            reserved_bytes: bytes,
        },
        &video_draft::AdmissionLimits {
            unfinished_per_account: 2,
            retained_source_bytes_per_account: 1024 * 1024,
            pending_source_bytes_global: 1024 * 1024,
            storage: storage_limits(),
            disk_available_bytes: u64::MAX,
            min_free_bytes: 0,
            processing_reserved_bytes: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(accepted, video_draft::Admission::Accepted);
    let mut tx = pool.begin().await.unwrap();
    assert!(
        video_draft::lock_receiving(&mut tx, media_id, owner.id)
            .await
            .unwrap()
    );
    store.put(&source, original.clone()).await.unwrap();
    assert!(
        video_draft::finish_receiving(&mut tx, media_id, owner.id, bytes)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    Fixture {
        _temp: temp,
        state,
        store,
        owner: owner.id,
        media_id,
        source,
        original,
        expected_duration: match source_shape {
            SourceShape::Audio => 9.0,
            SourceShape::ColourCut => 4.0,
            _ => 1.0,
        },
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn audio_only_source_gets_bounded_waveform_video_and_poster(pool: PgPool) {
    let f = fixture_with_source(pool, false, false, false, SourceShape::Audio).await;
    let source_path = f.store.disk.local_path(&f.source).unwrap();
    let ordinary = plamenu::media_transcode::process_av_file(
        &f.state.config,
        &source_path,
        &plamenu::media_transcode::TranscodeParams::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        ordinary.content_type, "audio/mpeg",
        "ordinary attachments stay Audio"
    );
    drop(ordinary);

    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let media = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let delivery = f
        .store
        .disk
        .local_path(media.file_name.as_deref().unwrap())
        .unwrap();
    let probe = plamenu::media_transcode::probe(&f.state.config, &delivery)
        .await
        .unwrap();
    assert_eq!(probe.video_codec.as_deref(), Some("h264"));
    assert_eq!(probe.audio_codec.as_deref(), Some("aac"));
    assert_eq!((probe.width, probe.height), (Some(640), Some(360)));
    let renditions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM native_video_hls_renditions WHERE media_id = $1",
    )
    .bind(f.media_id)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(renditions, 1);
    let playlist: String =
        sqlx::query_scalar("SELECT playlist FROM native_video_hls_renditions WHERE media_id = $1")
            .bind(f.media_id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert!(
        playlist.matches("#EXTINF:").count() >= 2,
        "audio-only HLS must have multiple seekable segments: {playlist}"
    );
}

async fn assert_silent_shape(pool: PgPool, shape: SourceShape, dimensions: (i64, i64)) {
    let f = fixture_with_source(pool, false, false, false, shape).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let media = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let delivery = f
        .store
        .disk
        .local_path(media.file_name.as_deref().unwrap())
        .unwrap();
    let probe = plamenu::media_transcode::probe(&f.state.config, &delivery)
        .await
        .unwrap();
    assert_eq!(
        (probe.width, probe.height),
        (Some(dimensions.0), Some(dimensions.1))
    );
    assert_eq!(probe.video_codec.as_deref(), Some("h264"));
    assert_eq!(probe.audio_codec, None, "silent sources remain silent");
    let renditions: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT width, height, playlist FROM native_video_hls_renditions WHERE media_id = $1 ORDER BY height DESC",
    )
    .bind(f.media_id)
    .fetch_all(&f.state.pool)
    .await
    .unwrap();
    assert!(!renditions.is_empty());
    assert_eq!(
        (renditions[0].0, renditions[0].1),
        (
            i32::try_from(dimensions.0).unwrap(),
            i32::try_from(dimensions.1).unwrap()
        )
    );
    assert!(
        renditions
            .iter()
            .all(|(_, _, playlist)| playlist.contains("#EXT-X-ENDLIST"))
    );
    if dimensions.1 > 360 {
        assert_eq!(renditions.len(), 2);
        assert_eq!(renditions[1].1, 360);
        assert!(renditions[1].0 < i32::try_from(dimensions.0).unwrap());
    } else {
        assert_eq!(renditions.len(), 1);
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn silent_portrait_video_keeps_its_aspect_ratio_and_seekable_hls(pool: PgPool) {
    assert_silent_shape(pool, SourceShape::SilentPortrait, (270, 480)).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn silent_ultrawide_video_avoids_pointless_upscale(pool: PgPool) {
    assert_silent_shape(pool, SourceShape::SilentUltrawide, (1000, 250)).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn chosen_poster_frame_survives_durable_claim_and_processing(pool: PgPool) {
    let f =
        fixture_with_source_and_time(pool, false, false, false, SourceShape::ColourCut, 3.0).await;
    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert!((draft.poster_time_seconds - 3.0).abs() < f64::EPSILON);
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let media = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let poster = f
        .store
        .get(media.small_file_name.as_deref().unwrap())
        .await
        .unwrap();
    let poster_dir = tempfile::tempdir().unwrap();
    let poster_path = poster_dir.path().join("chosen-poster.avif");
    std::fs::write(&poster_path, poster).unwrap();
    let pixel = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(poster_path)
        .args([
            "-vf",
            "scale=1:1",
            "-pix_fmt",
            "rgb24",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap();
    assert!(pixel.status.success());
    assert_eq!(pixel.stdout.len(), 3);
    assert!(i16::from(pixel.stdout[2]) > i16::from(pixel.stdout[0]) + 100);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn poster_frame_beyond_duration_records_the_actual_time(pool: PgPool) {
    let f =
        fixture_with_source_and_time(pool, false, false, false, SourceShape::ColourCut, 99.0).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert!((draft.poster_time_seconds - 3.0).abs() < f64::EPSILON);
}

fn still_poster(colour: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replacement.png");
    assert!(
        std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("color=c={colour}:size=64x64"))
            .args(["-frames:v", "1", "-y"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    std::fs::read(path).unwrap()
}

fn poster_pixel(bytes: &[u8], extension: &str) -> [u8; 3] {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("poster.{extension}"));
    std::fs::write(&path, bytes).unwrap();
    let output = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-vf",
            "scale=1:1",
            "-pix_fmt",
            "rgb24",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout.try_into().unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn custom_poster_replacement_is_private_journaled_and_repeatable(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let before = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let original_poster = before.small_file_name.unwrap();
    let blue = still_poster("blue");
    plamenu::video_poster::replace_draft_poster(
        &f.state,
        f.media_id,
        f.owner,
        blue,
        "A blue square".into(),
    )
    .await
    .unwrap();
    let after = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let blue_poster = after.small_file_name.unwrap();
    assert_ne!(blue_poster, original_poster);
    assert!(
        std::path::Path::new(&blue_poster)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("avif"))
    );
    let app = plamenu::build_router(f.state.clone());
    for key in [&original_poster, &blue_poster] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "private poster {key}"
        );
    }
    let blue_bytes = f.store.get(&blue_poster).await.unwrap();
    let pixel = poster_pixel(&blue_bytes, "avif");
    assert!(i16::from(pixel[2]) > i16::from(pixel[0]) + 100);
    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.poster_alt.as_deref(), Some("A blue square"));
    let jpeg: String =
        sqlx::query_scalar("SELECT file_name FROM video_compatibility_posters WHERE media_id = $1")
            .bind(f.media_id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert!(
        std::path::Path::new(&jpeg)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jpg"))
    );
    let jpeg_pixel = poster_pixel(&f.store.get(&jpeg).await.unwrap(), "jpg");
    assert!(i16::from(jpeg_pixel[2]) > i16::from(jpeg_pixel[0]) + 100);
    let retired: bool =
        sqlx::query_scalar("SELECT retired FROM video_draft_outputs WHERE file_name = $1")
            .bind(&original_poster)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert!(retired);
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs WHERE file_name = $1")
            .bind(&original_poster)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(queued, 1);
    let charged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM video_storage_reservations WHERE file_name = $1")
            .bind(&original_poster)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(
        charged, 1,
        "old poster stays charged until cleanup acknowledges deletion"
    );
    assert!(
        video_draft::preview(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .is_some()
    );

    plamenu::video_poster::replace_draft_poster(
        &f.state,
        f.media_id,
        f.owner,
        still_poster("red"),
        "A red square".into(),
    )
    .await
    .unwrap();
    let current = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_ne!(
        current.small_file_name.as_deref(),
        Some(blue_poster.as_str())
    );
    assert_eq!(
        video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .unwrap()
            .poster_alt
            .as_deref(),
        Some("A red square")
    );
    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    let (status, _) = actions::post_video(
        &f.state,
        PostParams {
            username: "author",
            media_ids: &[f.media_id],
            title: Some("A custom poster"),
            text: "A film with a custom image",
            visibility: "unlisted",
            language: Some("en"),
            kind: PostKind::Video,
            ..Default::default()
        },
        draft.channel_id,
    )
    .await
    .unwrap();
    let owner = plamenu_db::account::find_by_ids(&f.state.pool, &[f.owner])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let object = plamenu::note::note_for_status(&f.state, &status, &owner)
        .await
        .unwrap();
    assert_eq!(object["icon"][0]["name"], "A red square");
    let published_poster = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .small_file_name
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{published_poster}"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{original_poster}"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        plamenu::video_poster::replace_draft_poster(
            &f.state,
            f.media_id,
            f.owner,
            still_poster("blue"),
            "Blue again".into(),
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn custom_poster_rejects_other_owners_and_invalid_images(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let original = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .small_file_name;
    assert!(
        plamenu::video_poster::replace_draft_poster(
            &f.state,
            f.media_id,
            f.owner + 1,
            still_poster("blue"),
            "Blue".into(),
        )
        .await
        .is_err()
    );
    assert!(
        plamenu::video_poster::replace_draft_poster(
            &f.state,
            f.media_id,
            f.owner,
            b"not an image".to_vec(),
            "Blue".into(),
        )
        .await
        .is_err()
    );
    assert!(
        plamenu::video_poster::replace_draft_poster(
            &f.state,
            f.media_id,
            f.owner,
            still_poster("blue"),
            " ".into(),
        )
        .await
        .is_err()
    );
    let current = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(current.small_file_name, original);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn custom_poster_quota_refusal_keeps_existing_poster(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let original = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .small_file_name;
    let mut tx = f.state.pool.begin().await.unwrap();
    plamenu_db::video_storage::lock(&mut tx).await.unwrap();
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM video_storage_reservations WHERE account_id = $1",
    ).bind(f.owner).fetch_one(&mut *tx).await.unwrap();
    let remaining = f.state.config.native_video.storage_limits().per_account - used;
    sqlx::query(
        "INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes) VALUES ($1, $2, $3, $4)",
    ).bind("fixture-full-quota").bind(f.owner).bind(f.media_id).bind(remaining)
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let refusal = plamenu::video_poster::replace_draft_poster(
        &f.state,
        f.media_id,
        f.owner,
        still_poster("blue"),
        "Blue".into(),
    )
    .await;
    assert!(matches!(
        refusal,
        Err(plamenu::error::ApiError::Unprocessable(_))
    ));
    let current = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(current.small_file_name, original);
}

fn reservation(bytes: u64) -> plamenu::video_resources::Reservation {
    let pool = plamenu::video_resources::ProcessingPool::new(1, 16 * 1024 * 1024);
    reserve(&pool, bytes)
}

fn storage_limits() -> plamenu_db::video_storage::Limits {
    plamenu_db::video_storage::Limits {
        per_account: 100_000_000,
        global: 100_000_000,
    }
}

async fn run_one(
    state: &AppState,
    slot: plamenu::video_resources::Reservation,
) -> Result<bool, plamenu::error::ApiError> {
    plamenu::video_worker::run_one(state, slot, &storage_limits()).await
}

fn reserve(
    pool: &plamenu::video_resources::ProcessingPool,
    bytes: u64,
) -> plamenu::video_resources::Reservation {
    pool.try_reserve(
        FileSizeLimit {
            launcher: env!("CARGO_BIN_EXE_plamenu").into(),
            bytes,
        },
        1024 * 1024,
    )
    .unwrap()
}

async fn assert_ready(f: &Fixture) {
    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.state, "ready");
    let rows = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap();
    assert_eq!(rows[0].processing, "complete");
    assert_eq!(rows[0].kind.as_deref(), Some("video"));
    assert!(rows[0].status_id.is_none());
    assert!(
        (rows[0].duration.unwrap() - f.expected_duration).abs() < 0.1,
        "processed duration: {}",
        rows[0].duration.unwrap()
    );
    let preview = video_draft::preview(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert!(f.store.get(&preview.file_name).await.unwrap().len() > 100);
    assert!(
        !f.store
            .get(&preview.small_file_name)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        media::public_access_blocked(&f.state.pool, f.media_id)
            .await
            .unwrap()
    );
    assert!(
        video_draft::preview(&f.state.pool, f.media_id, f.owner + 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.store.get(&f.source).await.unwrap(), f.original);
    let hls = plamenu_db::native_video_hls::public_ladder(&f.state.pool, f.media_id)
        .await
        .unwrap();
    assert!(
        hls.is_empty(),
        "unpublished drafts cannot expose HLS playlists"
    );
    let private_hls = sqlx::query_scalar::<_, String>(
        "SELECT file_name FROM native_video_hls_renditions WHERE media_id = $1",
    )
    .bind(f.media_id)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert!(
        !media::public_file_allowed(&f.state.pool, &private_hls)
            .await
            .unwrap()
    );
    assert!(!f.store.get(&private_hls).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(f.state.config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn real_job_is_private_ready_and_preserves_original(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    let resources = plamenu::video_resources::ProcessingPool::new(1, 16 * 1024 * 1024);
    assert!(
        run_one(&f.state, reserve(&resources, 1024 * 1024))
            .await
            .unwrap()
    );
    assert_ready(&f).await;
    assert_eq!(resources.usage(), (0, 0));
    assert!(
        !run_one(&f.state, reserve(&resources, 1024 * 1024))
            .await
            .unwrap()
    );
    assert_eq!(
        resources.usage(),
        (0, 0),
        "an empty queue releases admission too"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn published_hls_is_range_playable_and_revoked_on_suspension(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    let key: String =
        sqlx::query_scalar("SELECT file_name FROM native_video_hls_renditions WHERE media_id = $1")
            .bind(f.media_id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let master_path = format!("/media/hls/{}/master.m3u8", f.media_id);
    let request = |path: &str| Request::builder().uri(path).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(&master_path))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.clone()
            .oneshot(request(&format!("/media/{key}")))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let draft = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    let media_ids = [f.media_id];
    let (status, _) = actions::post_video(
        &f.state,
        PostParams {
            username: "author",
            media_ids: &media_ids,
            title: Some("HLS test"),
            text: "Range-playable film",
            visibility: "unlisted",
            language: Some("en"),
            kind: PostKind::Video,
            ..Default::default()
        },
        draft.channel_id,
    )
    .await
    .unwrap();
    assert_eq!(status.object_type.as_deref(), Some("Video"));
    let owner = plamenu_db::account::find_by_ids(&f.state.pool, &[f.owner])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let object = plamenu::note::note_for_status(&f.state, &status, &owner)
        .await
        .unwrap();
    let hls_link = object["url"]
        .as_array()
        .unwrap()
        .iter()
        .find(|link| link["mediaType"] == "application/x-mpegURL")
        .unwrap();
    assert!(hls_link["href"].as_str().unwrap().ends_with(&master_path));
    assert_eq!(hls_link["tag"].as_array().unwrap().len(), 1);
    assert!(
        hls_link["tag"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/media/{key}"))
    );

    let master = app.clone().oneshot(request(&master_path)).await.unwrap();
    assert_eq!(master.status(), StatusCode::OK);
    let master_body = master.into_body().collect().await.unwrap().to_bytes();
    let master_text = std::str::from_utf8(&master_body).unwrap();
    assert!(master_text.contains("#EXT-X-STREAM-INF"));
    assert!(master_text.contains(&format!("/media/hls/{}/native/64/index.m3u8", f.media_id)));
    let playlist_path = format!("/media/hls/{}/native/64/index.m3u8", f.media_id);
    let playlist = app.clone().oneshot(request(&playlist_path)).await.unwrap();
    assert_eq!(playlist.status(), StatusCode::OK);
    let body = playlist.into_body().collect().await.unwrap().to_bytes();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains(&format!("/media/{key}"))
    );

    let segment = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/media/{key}"))
                .header(header::RANGE, "bytes=0-31")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(segment.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        segment
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len(),
        32
    );
    let rows = media::find_by_ids(&f.state.pool, &media_ids).await.unwrap();
    let ladders = media::renditions_for(&f.state.pool, &media_ids)
        .await
        .unwrap();
    let json = plamenu::entities::media_json_hls(
        &f.state.config.domain,
        &rows[0],
        false,
        ladders.get(&f.media_id).map(Vec::as_slice),
    );
    assert!(json["url"].as_str().unwrap().ends_with(".draft.mp4"));
    assert!(
        json["hls"]["master"]
            .as_str()
            .unwrap()
            .ends_with(&master_path)
    );

    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(&master_path))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.oneshot(request(&format!("/media/{key}")))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_larger_than_reserved_ceiling_is_deferred_without_spooling(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    let resources = plamenu::video_resources::ProcessingPool::new(1, 16 * 1024 * 1024);
    let slot = resources
        .try_reserve(
            FileSizeLimit {
                launcher: env!("CARGO_BIN_EXE_plamenu").into(),
                bytes: 1024 * 1024,
            },
            1,
        )
        .unwrap();
    assert!(!run_one(&f.state, slot).await.unwrap());
    assert_eq!(resources.usage(), (0, 0));
    assert_eq!(
        video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    assert_eq!(f.store.list().await.unwrap(), vec![f.source.clone()]);
    assert_eq!(f.store.get(&f.source).await.unwrap(), f.original);
    assert!(
        !f.state.config.media_dir.join("tmp").exists(),
        "reject before scratch allocation"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oversized_encode_can_retry_same_draft_without_reupload(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1)).await.is_err());
    let failed = video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(f.store.get(&f.source).await.unwrap(), f.original);
    assert_eq!(f.store.list().await.unwrap(), vec![f.source.clone()]);
    assert!(
        video_draft::retry(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cancellation_during_source_read_stops_before_output_write(pool: PgPool) {
    let f = fixture(pool, true, false, false).await;
    let state = f.state.clone();
    let worker = tokio::spawn(async move { run_one(&state, reservation(1024 * 1024)).await });
    tokio::time::timeout(Duration::from_secs(10), f.store.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        video_draft::cancel(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(f.store.list().await.unwrap(), vec![f.source.clone()]);
    let outputs: i64 = sqlx::query_scalar("SELECT count(*) FROM video_draft_outputs")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(outputs, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn aborted_caller_cannot_release_final_write_fence(pool: PgPool) {
    let f = fixture(pool, false, true, false).await;
    let state = f.state.clone();
    let resources = plamenu::video_resources::ProcessingPool::new(1, 16 * 1024 * 1024);
    let slot = reserve(&resources, 1024 * 1024);
    let worker = tokio::spawn(async move { run_one(&state, slot).await });
    tokio::time::timeout(Duration::from_secs(20), f.store.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(
        resources.usage().0,
        1,
        "detached storage still owns its processing slot"
    );
    assert!(resources.usage().1 > 0);
    // A cancellation must wait for the actual storage operation, even when
    // its original caller has disappeared. Probe the lock without cancelling
    // the real deletion and then release the fake slow storage backend.
    let mut tx = f.state.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '200ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query("SELECT id FROM media_attachments WHERE id = $1 FOR UPDATE")
            .bind(f.media_id)
            .fetch_one(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    // Finalization stores both the progressive MP4 and its HLS fMP4 body.
    f.store.release.add_permits(2);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
                .await
                .unwrap()
                .unwrap()
                .state
                == "ready"
                && resources.usage() == (0, 0)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_ready(&f).await;
    assert!(
        video_draft::cancel(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert!(
        pending >= 4,
        "original, progressive, HLS and preview outputs must be durably queued for deletion"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn partial_store_failure_keeps_private_journal_and_original(pool: PgPool) {
    let f = fixture(pool, false, false, true).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.is_err());
    assert_eq!(
        video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
    assert_eq!(f.store.get(&f.source).await.unwrap(), f.original);
    assert!(
        media::public_access_blocked(&f.state.pool, f.media_id)
            .await
            .unwrap()
    );
    assert!(
        video_draft::preview(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .is_none()
    );
    let outputs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM video_draft_outputs WHERE NOT retired")
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(outputs, 4);
    assert_eq!(
        f.store.list().await.unwrap().len(),
        2,
        "source plus journaled partial output"
    );
    assert!(
        video_draft::cancel(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stored_output_quota_refuses_writes_and_refunds_only_after_cleanup(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    let tight = plamenu_db::video_storage::Limits {
        per_account: i64::try_from(f.original.len()).unwrap(),
        global: 100_000_000,
    };
    assert!(
        plamenu::video_worker::run_one(&f.state, reservation(1024 * 1024), &tight)
            .await
            .is_err()
    );
    assert_eq!(f.store.list().await.unwrap(), vec![f.source.clone()]);
    assert!(
        video_draft::retry(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    assert_ready(&f).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_storage_reservations")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 5,
        "original, movie, HLS, preview and compatibility poster are charged"
    );
    let media = media::find_by_ids(&f.state.pool, &[f.media_id])
        .await
        .unwrap();
    plamenu::video::ensure_compatible_poster(&f.state, &media[0])
        .await
        .unwrap();
    let poster: String =
        sqlx::query_scalar("SELECT file_name FROM video_compatibility_posters WHERE media_id = $1")
            .bind(f.media_id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    let reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM video_storage_reservations WHERE file_name = $1",
    )
    .bind(&poster)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(
        reserved,
        i64::try_from(f.store.get(&poster).await.unwrap().len()).unwrap()
    );
    assert!(
        video_draft::cancel(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_storage_reservations")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 5, "deleting a draft is not deleting stored bytes");
    assert!(plamenu::media_cleanup_worker::run_due(&f.state).await >= 5);
    assert!(f.store.list().await.unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_storage_reservations")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_sizes_recover_from_storage_after_the_writer_is_fenced(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(run_one(&f.state, reservation(1024 * 1024)).await.unwrap());
    sqlx::query("UPDATE video_storage_reservations SET reserved_bytes = NULL WHERE file_name = $1")
        .bind(&f.source)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        plamenu::video_worker::recover_storage_accounting(&f.state)
            .await
            .unwrap(),
        1
    );
    let measured: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM video_storage_reservations WHERE file_name = $1",
    )
    .bind(&f.source)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(measured, i64::try_from(f.original.len()).unwrap());
    assert_eq!(
        plamenu::video_worker::recover_storage_accounting(&f.state)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scheduler_keeps_jobs_queued_while_an_active_legacy_size_is_unknown(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    let channel: i64 =
        sqlx::query_scalar("SELECT channel_id FROM video_drafts WHERE media_id = $1")
            .bind(f.media_id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    let old_id = id::next();
    let source = format!("{old_id}.orig");
    let output = format!("{old_id}ABCDEF.draft.mp4");
    sqlx::query(
        "INSERT INTO media_attachments (id, account_id, file_name, content_type, kind, processing)
        VALUES ($1, $2, $3, 'video/mp4', 'video', 'queued')",
    )
    .bind(old_id)
    .bind(f.owner)
    .bind(&source)
    .execute(&f.state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video_drafts
        (media_id, account_id, channel_id, title, description, visibility, source_file,
         source_cleanup_job_id, source_bytes, reserved_bytes, state, generation, lease_until)
        VALUES ($1, $2, $3, 'Older film', '', 'public', $4, $5, 1, 1,
                'processing', 1, now() + interval '1 hour')",
    )
    .bind(old_id)
    .bind(f.owner)
    .bind(channel)
    .bind(&source)
    .bind(id::next())
    .execute(&f.state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video_draft_outputs (file_name, media_id, generation, cleanup_job_id)
        VALUES ($1, $2, 1, $3)",
    )
    .bind(&output)
    .bind(old_id)
    .bind(id::next())
    .execute(&f.state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video_storage_reservations
        (file_name, account_id, media_id, reserved_bytes) VALUES ($1, $2, $3, NULL)",
    )
    .bind(&output)
    .bind(f.owner)
    .bind(old_id)
    .execute(&f.state.pool)
    .await
    .unwrap();

    let worker = plamenu::video_worker::spawn(f.state.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        video_draft::find_owned(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
            .unwrap()
            .state,
        "queued",
        "an unknown grandfathered size must not turn a new job into failed"
    );
    let unknown: Option<i64> = sqlx::query_scalar(
        "SELECT reserved_bytes FROM video_storage_reservations WHERE file_name = $1",
    )
    .bind(&output)
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(unknown, None, "the active writer cannot be measured yet");
    f.state.shutdown.cancel();
    worker.await.unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reconciliation_recovers_a_charge_even_when_the_file_is_already_missing(pool: PgPool) {
    let f = fixture(pool, false, false, false).await;
    assert!(
        video_draft::cancel(&f.state.pool, f.media_id, f.owner)
            .await
            .unwrap()
    );
    f.store.delete(&f.source).await.unwrap();
    let jobs = plamenu_db::media_cleanup::claim_due(&f.state.pool, 10)
        .await
        .unwrap();
    plamenu_db::media_cleanup::complete_many(
        &f.state.pool,
        &jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let report = plamenu::media_cleanup_worker::reconcile(&f.state, true)
        .await
        .unwrap();
    assert_eq!(report.scanned, 0);
    assert_eq!(report.enqueued, 1);
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 1);
    assert!(
        plamenu_db::video_storage::keys(&f.state.pool)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Exercise the real upgrade, not a current-schema fixture with its ledger
/// removed. Payloads here test accounting only; codec tests above use real AVIF.
async fn legacy_storage_fixture(pool: &PgPool) -> (AppState, i64, i64) {
    sqlx::migrate!("../db/migrations")
        .run_to(88, pool)
        .await
        .unwrap();
    let store = Arc::new(plamenu::storage::MemoryStore::default());
    let state = AppState::new(
        pool.clone(),
        common::test_config(),
        common::StubFederation::with_actors([]),
        store,
    )
    .unwrap();
    let owner = common::create_local_account(pool, "upgradeauthor", "Upgrade author").await;
    let channel = plamenu::video::create_channel(&state, &owner, "upgradefilms", "Upgrade films")
        .await
        .unwrap();
    let media_id = id::next();
    sqlx::query("INSERT INTO media_attachments
        (id, account_id, file_name, small_file_name, content_type, kind, processing, file_size, thumbnail_file_size)
        VALUES ($1, $2, 'legacy.draft.mp4', 'legacy.draft.small.avif', 'video/mp4', 'video', 'complete', 10, 5)")
        .bind(media_id).bind(owner.id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO video_drafts
        (media_id, account_id, channel_id, title, description, visibility, source_file,
         source_cleanup_job_id, source_bytes, reserved_bytes, state, generation)
        VALUES ($1, $2, $3, 'Legacy film', '', 'unlisted', 'legacy.orig', $4, 20, 20, 'ready', 2)",
    )
    .bind(media_id)
    .bind(owner.id)
    .bind(channel.id)
    .bind(id::next())
    .execute(pool)
    .await
    .unwrap();
    for (key, generation, retired) in [
        ("legacy.draft.mp4", 2, false),
        ("legacy.draft.small.avif", 2, false),
        ("old.draft.mp4", 1, true),
        ("old.draft.small.avif", 1, true),
    ] {
        sqlx::query(
            "INSERT INTO video_draft_outputs
            (file_name, media_id, generation, retired, cleanup_job_id) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(key)
        .bind(media_id)
        .bind(generation)
        .bind(retired)
        .bind(id::next())
        .execute(pool)
        .await
        .unwrap();
    }
    for (key, size) in [
        ("legacy.orig", 20),
        ("legacy.draft.mp4", 10),
        ("legacy.draft.small.avif", 5),
        ("old.draft.mp4", 7),
    ] {
        state.media.put(key, vec![0; size]).await.unwrap();
    }
    sqlx::migrate!("../db/migrations").run(pool).await.unwrap();
    (state, owner.id, media_id)
}

#[sqlx::test(migrations = false)]
async fn schema_88_upgrade_recovers_unknown_files_and_missing_poster_once(pool: PgPool) {
    let (state, owner, media_id) = legacy_storage_fixture(&pool).await;
    let unknown: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM video_storage_reservations WHERE reserved_bytes IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unknown, 2, "old attempts have no guessed sizes");
    let (a, b) = tokio::join!(
        plamenu::video_worker::recover_storage_accounting(&state),
        plamenu::video_worker::recover_storage_accounting(&state),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(
        plamenu::video_worker::recover_storage_accounting(&state)
            .await
            .unwrap(),
        0
    );
    let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT file_name, reserved_bytes FROM video_storage_reservations ORDER BY file_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("legacy.draft.mp4".into(), Some(10)),
            (
                "legacy.draft.poster.jpg".into(),
                Some(plamenu_db::video_storage::COMPAT_POSTER_BYTES)
            ),
            ("legacy.draft.small.avif".into(), Some(5)),
            ("legacy.orig".into(), Some(20)),
            ("old.draft.mp4".into(), Some(7)),
            ("old.draft.small.avif".into(), Some(0)),
        ]
    );
    let poster_generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM video_draft_outputs WHERE file_name = 'legacy.draft.poster.jpg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(poster_generation, 2);
    assert!(video_draft::cancel(&pool, media_id, owner).await.unwrap());
    assert_eq!(plamenu::media_cleanup_worker::run_due(&state).await, 6);
    assert!(
        plamenu_db::video_storage::keys(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = false)]
async fn upgrade_does_not_allocate_posters_to_failed_or_live_attempts(pool: PgPool) {
    let (state, _, media_id) = legacy_storage_fixture(&pool).await;
    sqlx::query("UPDATE video_drafts SET state = 'processing', lease_until = now() + interval '1 hour' WHERE media_id = $1")
        .bind(media_id).execute(&pool).await.unwrap();
    // Make current files unknown, as after a pre-ledger crash. A live claim
    // must not have its allowance measured down ahead of the writer.
    sqlx::query("UPDATE video_storage_reservations SET reserved_bytes = NULL WHERE file_name = 'legacy.draft.mp4'")
        .execute(&pool).await.unwrap();
    assert_eq!(
        plamenu::video_worker::recover_storage_accounting(&state)
            .await
            .unwrap(),
        2
    );
    let measured: Option<i64> = sqlx::query_scalar(
        "SELECT reserved_bytes FROM video_storage_reservations WHERE file_name = 'legacy.draft.mp4'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(measured, None);
    sqlx::query("UPDATE video_drafts SET state = 'failed', lease_until = NULL WHERE media_id = $1")
        .bind(media_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        plamenu::video_worker::recover_storage_accounting(&state)
            .await
            .unwrap(),
        1
    );
    assert!(
        !plamenu_db::video_storage::keys(&pool)
            .await
            .unwrap()
            .iter()
            .any(|k| k.ends_with(".poster.jpg"))
    );
}
