//! Caching HLS reverse-proxy (`PeerTube` long-form video): the master/media
//! playlist rewrite over HTTP, the per-range segment cache (N viewers = ONE
//! origin fetch, only watched parts), and the SSRF fence on proxied URLs.

mod common;

use std::process::Command;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::{StubFederation, create_local_account, test_app_with, test_state_with};
use http_body_util::BodyExt;
use plamenu::build_router;
use plamenu_db::{PgPool, id, instance_settings};
use tower::ServiceExt;

const MASTER: &str = "https://remote.example/static/streaming-playlists/hls/vid/master.m3u8";
const RENDITION: &str =
    "https://remote.example/static/streaming-playlists/hls/vid/vid-1080-fragmented.mp4";
const RENDITION_PLAYLIST: &str =
    "https://remote.example/static/streaming-playlists/hls/vid/vid-1080.m3u8";
const AUDIO: &str =
    "https://remote.example/static/streaming-playlists/hls/vid/vid-audio-fragmented.mp4";
const AUDIO_PLAYLIST: &str =
    "https://remote.example/static/streaming-playlists/hls/vid/vid-audio.m3u8";

/// Inserts a minimal HLS video media row (as ingest would), returning its id.
async fn hls_media_row(pool: &PgPool, account_id: i64) -> i64 {
    let media_id = id::next();
    sqlx::query!(
        r#"
        INSERT INTO media_attachments
            (id, account_id, content_type, processing, download_on_demand,
             remote_url, hls_master_url)
        VALUES ($1, $2, 'video/mp4', 'complete', true, $3, $4)
        "#,
        media_id,
        account_id,
        RENDITION,
        MASTER,
    )
    .execute(pool)
    .await
    .unwrap();
    media_id
}

async fn audio_media_row(pool: &PgPool, account_id: i64, origin: &str) -> i64 {
    let media_id = id::next();
    sqlx::query!(
        r#"
        INSERT INTO media_attachments
            (id, account_id, content_type, processing, download_on_demand, remote_url)
        VALUES ($1, $2, 'audio/mpeg', 'complete', true, $3)
        "#,
        media_id,
        account_id,
        origin,
    )
    .execute(pool)
    .await
    .unwrap();
    media_id
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    range: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

struct SplitHlsFixture {
    video: Vec<u8>,
    audio: Vec<u8>,
    video_playlist: Vec<u8>,
    audio_playlist: Vec<u8>,
}

struct MuxedHlsFixture {
    media: Vec<u8>,
    playlist: Vec<u8>,
}

fn muxed_hls_fixture() -> Option<MuxedHlsFixture> {
    if Command::new("ffmpeg").arg("-version").output().is_err()
        || Command::new("ffprobe").arg("-version").output().is_err()
    {
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let playlist = temp.path().join("muxed.m3u8");
    let status = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "5",
            "-c:v",
            "libx264",
            "-g",
            "10",
            "-keyint_min",
            "10",
            "-sc_threshold",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_segment_type",
            "fmp4",
            "-hls_flags",
            "single_file",
            "-hls_time",
            "1",
        ])
        .arg(&playlist)
        .status()
        .unwrap();
    assert!(status.success());
    Some(MuxedHlsFixture {
        media: std::fs::read(temp.path().join("muxed.m4s")).unwrap(),
        playlist: std::fs::read(playlist).unwrap(),
    })
}

fn split_hls_fixture() -> Option<SplitHlsFixture> {
    if Command::new("ffmpeg").arg("-version").output().is_err()
        || Command::new("ffprobe").arg("-version").output().is_err()
    {
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let video_playlist = temp.path().join("video.m3u8");
    let audio_playlist = temp.path().join("audio.m3u8");
    let video = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=10",
            "-t",
            "20",
            "-an",
            "-c:v",
            "libx264",
            "-g",
            "60",
            "-keyint_min",
            "60",
            "-sc_threshold",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hls",
            "-hls_segment_type",
            "fmp4",
            "-hls_flags",
            "single_file",
            "-hls_playlist_type",
            "vod",
            "-hls_time",
            "6",
        ])
        .arg(&video_playlist)
        .status()
        .unwrap();
    let audio = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "20",
            "-vn",
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_segment_type",
            "fmp4",
            "-hls_flags",
            "single_file",
            "-hls_playlist_type",
            "vod",
            "-hls_time",
            "4",
        ])
        .arg(&audio_playlist)
        .status()
        .unwrap();
    assert!(video.success() && audio.success());
    Some(SplitHlsFixture {
        video: std::fs::read(temp.path().join("video.m4s")).unwrap(),
        audio: std::fs::read(temp.path().join("audio.m4s")).unwrap(),
        video_playlist: std::fs::read(video_playlist).unwrap(),
        audio_playlist: std::fs::read(audio_playlist).unwrap(),
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn playlists_are_memoized_so_n_viewers_cause_one_origin_fetch(pool: PgPool) {
    // M39: the rewritten playlist is memoized for the 60 s the browser is
    // told to cache it, so a crowd of viewers costs the origin one playlist
    // fetch — the playlist counterpart of the per-range segment cache.
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let stub = StubFederation::with_actors([]);
    stub.serve_media(
        MASTER,
        "application/vnd.apple.mpegurl",
        concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\n",
            "vid-1080-fragmented.m3u8\n",
        )
        .as_bytes()
        .to_vec(),
    );
    let app = test_app_with(pool.clone(), stub.clone());

    let (status, first) = get(&app, &format!("/media/hls/{media_id}/master.m3u8")).await;
    assert_eq!(status, StatusCode::OK);
    // The rewrite: the variant playlist URI is proxied and the STREAM-INF line
    // is preserved verbatim.
    let rewritten = String::from_utf8_lossy(&first);
    assert!(
        rewritten.contains(&format!("/media/hls/{media_id}/pl?u=")),
        "variant proxied: {rewritten}"
    );
    assert!(
        rewritten
            .contains("BANDWIDTH=1200000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\""),
        "stream attributes verbatim: {rewritten}"
    );
    let (status, second) = get(&app, &format!("/media/hls/{media_id}/master.m3u8")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second, "memoized copy is byte-identical");
    assert_eq!(
        stub.media_fetches
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.as_str() == MASTER)
            .count(),
        1,
        "one origin fetch serves every viewer"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn segment_is_cached_so_n_viewers_cause_one_origin_fetch(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let stub = StubFederation::with_actors([]);
    // The rendition's fragmented mp4 as the origin sees it: 100 bytes.
    let full: Vec<u8> = (0..100u8).collect();
    stub.serve_media(RENDITION, "video/mp4", full.clone());
    let app = test_app_with(pool.clone(), stub.clone());

    let u = B64.encode(RENDITION.as_bytes());
    let uri = format!("/media/hls/{media_id}/seg?u={u}&s=10&l=5");

    // First play: fetched from the origin, cached, served.
    let (status, body) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, &full[10..15], "the requested byte range is served");

    // Second play (a second viewer): served from cache — no second origin hit.
    let (status2, body2) = get(&app, &uri).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, &full[10..15]);

    let fetches = stub.range_fetches();
    assert_eq!(
        fetches.len(),
        1,
        "N viewers of a segment cause exactly ONE origin fetch: {fetches:?}"
    );
    assert_eq!(fetches[0], (RENDITION.to_owned(), 10, 5));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn progressive_mp4_streams_and_caches_only_requested_blocks(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let full: Vec<u8> = (0..1_200_000_u32).map(|n| (n % 251) as u8).collect();
    sqlx::query!(
        r#"INSERT INTO media_renditions
           (id, media_id, height, width, frame_rate, size_bytes, origin_url, is_audio)
           VALUES ($1, $2, 480, 854, 30, $3, $4, false)"#,
        id::next(),
        media_id,
        i64::try_from(full.len()).unwrap(),
        RENDITION,
    )
    .execute(&pool)
    .await
    .unwrap();
    let stub = StubFederation::with_actors([]);
    stub.serve_media(RENDITION, "video/mp4", full.clone());
    let app = test_app_with(pool.clone(), stub.clone());
    let uri = format!("/media/play/{media_id}/video.mp4");

    // HEAD is metadata-only: it advertises a seekable MP4 and never opens the
    // origin body or creates the old detached AV job.
    let head = request(&app, "HEAD", &uri, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH],
        full.len().to_string()
    );
    assert_eq!(head.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(stub.range_fetches().is_empty());

    // An arbitrary seek fetches the one aligned 512 KiB block containing it,
    // not the prefix and certainly not the whole video.
    let response = request(&app, "GET", &uri, Some("bytes=700000-700099")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        format!("bytes 700000-700099/{}", full.len())
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], &full[700_000..700_100]);
    assert_eq!(
        stub.range_fetches(),
        [(RENDITION.to_owned(), 512 * 1024, 512 * 1024)],
        "one bounded aligned origin range"
    );

    // A second client reuses the cached block.
    let response = request(&app, "GET", &uri, Some("bytes=700050-700079")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], &full[700_050..700_080]);
    assert_eq!(
        stub.range_fetches().len(),
        1,
        "second viewer is a cache hit"
    );

    // Exercise a cached block at absolute offset zero too (the common startup
    // path, and an unsigned-arithmetic boundary worth pinning explicitly).
    let response = request(&app, "GET", &uri, Some("bytes=0-99")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        &full[..100]
    );
    let response = request(&app, "GET", &uri, Some("bytes=0-49")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        &full[..50]
    );
    assert_eq!(stub.range_fetches().len(), 2, "offset-zero block is reused");

    let jobs = sqlx::query_scalar!(
        "SELECT count(*) AS \"count!\" FROM media_processing_jobs WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 0, "playback never creates a detached media job");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn progressive_audio_plays_while_its_first_block_is_still_caching(pool: PgPool) {
    const PODCAST: &str = "https://podcast.example/episode.mp3";
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = audio_media_row(&pool, account.id, PODCAST).await;
    let full: Vec<u8> = (0..1_200_000_u32).map(|n| (n % 251) as u8).collect();
    let stub = StubFederation::with_actors([]);
    stub.serve_media(PODCAST, "audio/mpeg", full.clone());
    // Sixteen delayed chunks make it observable that the response starts
    // before the complete aligned block has landed in the cache.
    stub.slow_range_chunks(32 * 1024, 20);
    let app = test_app_with(pool.clone(), stub.clone());
    let uri = format!("/media/play/{media_id}/audio");

    let head = request(&app, "HEAD", &uri, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH],
        full.len().to_string()
    );
    assert_eq!(head.headers()[header::ACCEPT_RANGES], "bytes");

    let response = request(&app, "GET", &uri, Some("bytes=0-99")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&first[..], &full[..100]);
    let cached_during_playback = sqlx::query_scalar!(
        "SELECT count(*) AS \"count!\" FROM media_hls_segments WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        cached_during_playback, 0,
        "the listener received bytes before the block finished caching"
    );
    body.collect().await.unwrap();
    let cached_after_block = sqlx::query_scalar!(
        "SELECT count(*) AS \"count!\" FROM media_hls_segments WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cached_after_block, 1);

    let replay = request(&app, "GET", &uri, Some("bytes=0-49")).await;
    assert_eq!(replay.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &replay.into_body().collect().await.unwrap().to_bytes()[..],
        &full[..50]
    );
    assert_eq!(
        stub.range_fetches(),
        [
            (PODCAST.to_owned(), 0, 1),
            (PODCAST.to_owned(), 0, 512 * 1024),
        ],
        "one metadata probe and one bounded fetch serve every listener"
    );
    let jobs = sqlx::query_scalar!(
        "SELECT count(*) AS \"count!\" FROM media_processing_jobs WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 0, "audio playback never waits on a whole-file job");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oversized_audio_requires_the_direct_media_opt_in(pool: PgPool) {
    const PODCAST: &str = "https://podcast.example/huge.mp3";
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = audio_media_row(&pool, account.id, PODCAST).await;
    let stub = StubFederation::with_actors([]);
    stub.serve_media(PODCAST, "audio/mpeg", vec![0_u8; 1024 * 1024 + 1]);
    let state = test_state_with(pool.clone(), stub.clone());
    let current = instance_settings::get(&pool).await.unwrap();
    instance_settings::save(
        &pool,
        instance_settings::SettingsUpdate {
            remote_video_max_mb: 1,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    let app = build_router(state);
    let uri = format!("/media/play/{media_id}/audio");

    assert_eq!(
        request(&app, "HEAD", &uri, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let direct = request(&app, "HEAD", &format!("{uri}?d=1"), None).await;
    assert_eq!(direct.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(direct.headers()[header::LOCATION], PODCAST);
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT count(*) AS \"count!\" FROM media_hls_segments WHERE media_id = $1",
            media_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn progressive_disconnect_cancels_origin_stream(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let full = vec![7_u8; 2 * 1024 * 1024];
    sqlx::query!(
        r#"INSERT INTO media_renditions
           (id, media_id, height, width, frame_rate, size_bytes, origin_url, is_audio)
           VALUES ($1, $2, 480, 854, 30, $3, $4, false)"#,
        id::next(),
        media_id,
        i64::try_from(full.len()).unwrap(),
        RENDITION,
    )
    .execute(&pool)
    .await
    .unwrap();
    let stub = StubFederation::with_actors([]);
    stub.serve_media(RENDITION, "video/mp4", full);
    stub.slow_range_chunks(32 * 1024, 20);
    let app = test_app_with(pool, stub.clone());

    let response = request(
        &app,
        "GET",
        &format!("/media/play/{media_id}/video.mp4"),
        None,
    )
    .await;
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(!first.is_empty());
    drop(body); // viewer navigated away

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let stopped_at = stub.range_chunks_sent();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        stub.range_chunks_sent(),
        stopped_at,
        "no origin chunks arrive after downstream disconnect"
    );
    assert!(
        stopped_at < 16,
        "at most the bounded 512 KiB block was in flight, got {stopped_at} chunks"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stable_gateway_reuses_a_legacy_cached_copy(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let file = format!("{media_id}.mp4");
    let full: Vec<u8> = (0..20_000_u32).map(|n| (n % 251) as u8).collect();
    sqlx::query!(
        "UPDATE media_attachments SET file_name = $1 WHERE id = $2",
        file,
        media_id,
    )
    .execute(&pool)
    .await
    .unwrap();
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool, stub.clone());
    state.media.put(&file, full.clone()).await.unwrap();
    let app = build_router(state);
    let uri = format!("/media/play/{media_id}/video.mp4");

    let head = request(&app, "HEAD", &uri, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH],
        full.len().to_string()
    );
    let response = request(&app, "GET", &uri, Some("bytes=100-199")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        &full[100..200]
    );
    assert!(
        stub.range_fetches().is_empty(),
        "cached copy avoids PeerTube"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn muxed_hls_gateway_front_loads_the_complete_seek_map(pool: PgPool) {
    let Some(fixture) = muxed_hls_fixture() else {
        eprintln!("ffmpeg/ffprobe unavailable; skipping format validation");
        return;
    };
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    sqlx::query!(
        r#"INSERT INTO media_renditions
           (id, media_id, height, width, frame_rate, size_bytes, origin_url, is_audio)
           VALUES ($1, $2, 480, 854, 10, $3, $4, false)"#,
        id::next(),
        media_id,
        i64::try_from(fixture.media.len()).unwrap(),
        RENDITION,
    )
    .execute(&pool)
    .await
    .unwrap();
    let stub = StubFederation::with_actors([]);
    stub.serve_media(RENDITION, "video/mp4", fixture.media);
    stub.serve_media(
        RENDITION_PLAYLIST,
        "application/vnd.apple.mpegurl",
        fixture.playlist,
    );
    let app = test_app_with(pool.clone(), stub);
    let uri = format!("/media/play/{media_id}/video.mp4");

    let head = request(&app, "HEAD", &uri, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    let total = head.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let response = request(&app, "GET", &uri, Some("bytes=0-4095")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let prefix = response.into_body().collect().await.unwrap().to_bytes();
    let (sidx_start, sidx) = top_level_box(&prefix, *b"sidx").expect("one global seek index");
    let timescale = u32::from_be_bytes(sidx[16..20].try_into().unwrap());
    let count = u16::from_be_bytes(sidx[30..32].try_into().unwrap()) as usize;
    assert!(count >= 5, "one reference per muxed HLS fragment");
    let entries = sidx[32..].as_chunks::<12>().0;
    let indexed_duration = entries
        .iter()
        .map(|entry| u64::from(u32::from_be_bytes(entry[4..8].try_into().unwrap())))
        .sum::<u64>();
    assert!(indexed_duration > u64::from(timescale) * 49 / 10);
    let indexed_size = entries
        .iter()
        .map(|entry| (u32::from_be_bytes(entry[..4].try_into().unwrap()) & 0x7fff_ffff) as usize)
        .sum::<usize>();
    assert_eq!(sidx_start + sidx.len() + indexed_size, total);
    let first_fragment = sidx_start + sidx.len();
    let response = request(
        &app,
        "GET",
        &uri,
        Some(&format!("bytes={first_fragment}-{}", first_fragment + 127)),
    )
    .await;
    let first = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&first[4..8], b"free", "local track index is neutralized");

    let response = request(&app, "GET", &uri, None).await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.len(), total);
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("muxed-gateway.mp4");
    std::fs::write(&output, bytes).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration",
            "-of",
            "default=nw=1",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "ffprobe rejected indexed muxed MP4: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let metadata = String::from_utf8_lossy(&probe.stdout);
    assert!(metadata.contains("codec_type=video"), "{metadata}");
    assert!(metadata.contains("codec_type=audio"), "{metadata}");
    let seek = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", "3", "-i"])
        .arg(&output)
        .args([
            "-t", "1", "-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-",
        ])
        .output()
        .unwrap();
    assert!(
        seek.status.success(),
        "cold muxed seek failed: {}",
        String::from_utf8_lossy(&seek.stderr)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn split_hls_gateway_is_seekable_mp4_with_video_and_audio(pool: PgPool) {
    let Some(fixture) = split_hls_fixture() else {
        eprintln!("ffmpeg/ffprobe unavailable; skipping format validation");
        return;
    };
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    sqlx::query!(
        "UPDATE media_attachments SET remote_audio_url = $1 WHERE id = $2",
        AUDIO,
        media_id,
    )
    .execute(&pool)
    .await
    .unwrap();
    for (origin, size, height, is_audio) in [
        (
            RENDITION,
            i64::try_from(fixture.video.len()).unwrap(),
            480,
            false,
        ),
        (AUDIO, i64::try_from(fixture.audio.len()).unwrap(), 0, true),
    ] {
        sqlx::query!(
            r#"INSERT INTO media_renditions
               (id, media_id, height, width, frame_rate, size_bytes, origin_url, is_audio)
               VALUES ($1, $2, $3, NULL, 30, $4, $5, $6)"#,
            id::next(),
            media_id,
            height,
            size,
            origin,
            is_audio,
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let stub = StubFederation::with_actors([]);
    stub.serve_media(RENDITION, "video/mp4", fixture.video);
    stub.serve_media(AUDIO, "audio/mp4", fixture.audio);
    stub.serve_media(
        RENDITION_PLAYLIST,
        "application/vnd.apple.mpegurl",
        fixture.video_playlist,
    );
    stub.serve_media(
        AUDIO_PLAYLIST,
        "application/vnd.apple.mpegurl",
        fixture.audio_playlist,
    );
    let app = test_app_with(pool.clone(), stub.clone());
    let uri = format!("/media/play/{media_id}/video.mp4");

    let head = request(&app, "HEAD", &uri, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    let total = head.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(total > 1000);

    // A cold seek into the tail works without first downloading the prefix.
    let seek = format!("bytes={}-{}", total - 100, total - 1);
    let response = request(&app, "GET", &uri, Some(&seek)).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len(),
        100
    );

    // All duration and seek metadata fits in the prefix. A generic client can
    // learn the complete twenty-second timeline without walking fragment-local
    // indexes or downloading the media body.
    let response = request(&app, "GET", &uri, Some("bytes=0-2047")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let prefix = response.into_body().collect().await.unwrap().to_bytes();
    let (sidx_start, sidx) = top_level_box(&prefix, *b"sidx").expect("front-loaded global sidx");
    let count = u16::from_be_bytes(sidx[30..32].try_into().unwrap()) as usize;
    assert!(count >= 4, "one global reference per video HLS segment");
    let timescale = u32::from_be_bytes(sidx[16..20].try_into().unwrap());
    let entries = sidx[32..].as_chunks::<12>().0;
    assert_eq!(entries.len(), count);
    let indexed_duration: u64 = entries
        .iter()
        .map(|entry| u64::from(u32::from_be_bytes(entry[4..8].try_into().unwrap())))
        .sum();
    assert!(
        indexed_duration > u64::from(timescale) * 199 / 10,
        "prefix index advertises the complete presentation"
    );
    let indexed_size: usize = entries
        .iter()
        .map(|entry| (u32::from_be_bytes(entry[..4].try_into().unwrap()) & 0x7fff_ffff) as usize)
        .sum();
    assert_eq!(
        sidx_start + sidx.len() + indexed_size,
        total,
        "the front index covers every following byte exactly"
    );
    let last_reference = sidx_start
        + sidx.len()
        + entries[..entries.len() - 1]
            .iter()
            .map(|entry| {
                (u32::from_be_bytes(entry[..4].try_into().unwrap()) & 0x7fff_ffff) as usize
            })
            .sum::<usize>();
    let response = request(
        &app,
        "GET",
        &uri,
        Some(&format!("bytes={last_reference}-{}", last_reference + 99)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len(),
        100
    );

    // Reconstructing the entire virtual resource proves that conventional MP4
    // consumers see both tracks, including when playlist segment counts differ.
    let response = request(&app, "GET", &uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.len(), total);
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("gateway.mp4");
    std::fs::write(&output, bytes).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration",
            "-of",
            "default=nw=1",
        ])
        .arg(output)
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "ffprobe rejected gateway MP4: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(streams.contains("codec_type=video"), "{streams}");
    assert!(streams.contains("codec_type=audio"), "{streams}");
    let duration = streams
        .lines()
        .filter_map(|line| line.strip_prefix("duration="))
        .next_back()
        .and_then(|value| value.parse::<f64>().ok())
        .expect("ffprobe format duration");
    assert!(
        duration > 19.9,
        "full duration, not the first fragment: {duration}; {streams}"
    );
    let jobs = sqlx::query_scalar!(
        "SELECT count(*) AS \"count!\" FROM media_processing_jobs WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 0, "virtual playback never queues a whole-file job");
}

fn top_level_box(bytes: &[u8], wanted: [u8; 4]) -> Option<(usize, &[u8])> {
    let mut cursor = 0_usize;
    while cursor.checked_add(8)? <= bytes.len() {
        let size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().ok()?) as usize;
        if size < 8 || cursor.checked_add(size)? > bytes.len() {
            return None;
        }
        if bytes[cursor + 4..cursor + 8] == wanted {
            return Some((cursor, &bytes[cursor..cursor + size]));
        }
        cursor += size;
    }
    None
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_oversized_segment_range_is_refused(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let stub = StubFederation::with_actors([]);
    stub.serve_media(RENDITION, "video/mp4", vec![0u8; 100]);
    let app = test_app_with(pool.clone(), stub.clone());

    let u = B64.encode(RENDITION.as_bytes());
    let huge = 128u64 * 1024 * 1024; // over the 64 MiB per-segment cap
    let (status, _) = get(
        &app,
        &format!("/media/hls/{media_id}/seg?u={u}&s=0&l={huge}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an oversized byte range is refused"
    );
    assert!(
        stub.range_fetches().is_empty(),
        "a malicious range never reaches the origin"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_dead_master_origin_is_not_re_probed_within_the_fail_window(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let stub = StubFederation::with_actors([]);
    // MASTER is deliberately NOT registered → the origin fetch fails, as if the
    // PeerTube instance had vanished.
    let app = test_app_with(pool.clone(), stub.clone());

    let uri = format!("/media/hls/{media_id}/master.m3u8");
    let (first, _) = get(&app, &uri).await;
    let (second, _) = get(&app, &uri).await;
    assert_eq!(first, StatusCode::NOT_FOUND);
    assert_eq!(second, StatusCode::NOT_FOUND);
    // The second play attempt short-circuited — the dead origin was contacted
    // exactly once, not on every retry (no infinite/repeated hammering).
    let master_fetches = stub
        .media_fetches
        .lock()
        .unwrap()
        .iter()
        .filter(|u| u.ends_with("master.m3u8"))
        .count();
    assert_eq!(
        master_fetches, 1,
        "a failed master is suppressed, not re-probed"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn exhausted_hls_origin_is_never_automatically_retried(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    sqlx::query!(
        "INSERT INTO media_fetch_failures (kind, target_id, attempts, failed_at)
         VALUES ('hls-master', $1, $2, now() - interval '10 years')",
        media_id,
        plamenu_db::media_fetch_failure::MAX_ATTEMPTS,
    )
    .execute(&pool)
    .await
    .unwrap();
    let stub = StubFederation::with_actors([]);
    stub.serve_media(
        MASTER,
        "application/vnd.apple.mpegurl",
        b"#EXTM3U\n".to_vec(),
    );
    let app = test_app_with(pool, stub.clone());

    let (status, _) = get(&app, &format!("/media/hls/{media_id}/master.m3u8")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        stub.media_fetches.lock().unwrap().is_empty(),
        "terminal failure state must not probe the origin"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn media_json_carries_the_hls_extension_for_hls_video(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    // Two renditions + the separated audio track, as ingest would store.
    for (h, w, audio) in [
        (1080, Some(1920), false),
        (480, Some(854), false),
        (0, None, true),
    ] {
        sqlx::query!(
            r#"INSERT INTO media_renditions
               (id, media_id, height, width, frame_rate, size_bytes, origin_url, is_audio)
               VALUES ($1, $2, $3, $4, 30, 1000, $5, $6)"#,
            id::next(),
            media_id,
            h,
            w,
            format!("{RENDITION}#{h}"),
            audio,
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    let media = plamenu_db::media::find_by_ids(&pool, &[media_id])
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let ladder = plamenu_db::media::renditions_for(&pool, &[media_id])
        .await
        .unwrap();
    let rends = ladder.get(&media_id).unwrap();

    let json = plamenu::entities::media_json_hls("plamenu.test", &media, false, Some(rends));
    // `url` stays a single progressive URL for dumb Mastodon-API clients.
    assert_eq!(json["type"], "video");
    assert_eq!(
        json["url"],
        format!("https://plamenu.test/media/play/{media_id}/video.mp4"),
        "ordinary clients receive the sparse compatibility gateway"
    );
    assert_eq!(
        json["remote_url"],
        format!("https://plamenu.test/media/hls/{media_id}/master.m3u8"),
        "HLS-aware third-party clients receive the proxied master"
    );
    // The HLS extension exposes the proxied master + the ladder for capable clients.
    assert_eq!(
        json["hls"]["master"],
        format!("https://plamenu.test/media/hls/{media_id}/master.m3u8")
    );
    let listed = json["hls"]["renditions"].as_array().unwrap();
    assert_eq!(listed.len(), 3, "whole ladder exposed: {json}");
    assert_eq!(listed[0]["height"], 1080);
    assert_eq!(listed[0]["audio_only"], false);
    assert_eq!(listed[2]["audio_only"], true, "audio track flagged");

    // A non-HLS attachment keeps exactly the Mastodon shape (no `hls` key).
    let bare = plamenu::entities::media_json_hls("plamenu.test", &media, false, None);
    assert!(bare.get("hls").is_none(), "no hls key when not HLS: {bare}");
    assert!(
        bare.get("live").is_none(),
        "no live key on an ordinary attachment: {bare}"
    );
}

/// A live broadcast serializes differently in each of its states, and the
/// difference is the whole point: `url` is what a quality-blind client plays,
/// and off air there is nothing to play. Handing one a gateway URL that 404s
/// would render as a broken player; a null `url` with a preview is Mastodon's
/// own shape for "no bytes yet", and clients already show the poster for it.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_live_attachment_is_playable_only_while_it_is_on_air(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    // A live carries no rendition ladder — its qualities live in the playlist.
    let empty: [plamenu_db::media::MediaRendition; 0] = [];

    for (state, on_air) in [("waiting", false), ("live", true), ("ended", false)] {
        sqlx::query!(
            "UPDATE media_attachments SET live_state = $2, content_type = 'application/x-mpegURL' \
             WHERE id = $1",
            media_id,
            state,
        )
        .execute(&pool)
        .await
        .unwrap();
        let media = plamenu_db::media::find_by_ids(&pool, &[media_id])
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let json = plamenu::entities::media_json_hls("plamenu.test", &media, false, Some(&empty));

        assert_eq!(json["type"], "video", "a broadcast is a video: {json}");
        assert_eq!(json["live"]["state"], state);
        assert_eq!(json["live"]["permanent"], false);
        // The poster is always available, in every state.
        assert_eq!(
            json["preview_url"],
            format!("https://plamenu.test/media/proxy/attachment/{media_id}/small")
        );

        if on_air {
            assert_eq!(
                json["url"],
                format!("https://plamenu.test/media/live/{media_id}/stream.mp4"),
                "on air, a quality-blind client gets the remuxing gateway"
            );
            assert_eq!(
                json["hls"]["master"],
                format!("https://plamenu.test/media/hls/{media_id}/master.m3u8"),
                "and an HLS-aware one gets the proxied master"
            );
            assert!(
                json["hls"]["renditions"].as_array().unwrap().is_empty(),
                "a live's ladder is in the playlist, not the object: {json}"
            );
        } else {
            assert!(
                json["url"].is_null(),
                "off air there is nothing to play: {json}"
            );
            assert!(
                json.get("hls").is_none(),
                "and no playlist to advertise: {json}"
            );
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ssrf_fence_rejects_segments_outside_the_video_dir(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let media_id = hls_media_row(&pool, account.id).await;
    let stub = StubFederation::with_actors([]);
    // Register bytes for the evil URL too — proving it's the fence, not a 404.
    stub.serve_media(
        "https://remote.example/etc/passwd",
        "video/mp4",
        vec![1, 2, 3, 4, 5],
    );
    let app = test_app_with(pool.clone(), stub.clone());

    let evil = B64.encode("https://remote.example/etc/passwd".as_bytes());
    let (status, _) = get(&app, &format!("/media/hls/{media_id}/seg?u={evil}&s=0&l=5")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a URL outside the master's directory is refused"
    );
    assert!(
        stub.range_fetches().is_empty(),
        "the origin is never contacted for an out-of-fence URL"
    );
}
