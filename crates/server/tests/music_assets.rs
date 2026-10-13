mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::{StubFederation, create_local_account, test_state_with_store};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    music_assets::{self, Edit},
    storage::MemoryStore,
};
use plamenu_db::{PgPool, music::Metadata};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    _dir: tempfile::TempDir,
    alice: i64,
    bob: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let alice = create_local_account(&pool, "alice", "Alice").await.id;
    let bob = create_local_account(&pool, "bob", "Bob").await.id;
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state_with_store(
        pool,
        StubFederation::with_actors([]),
        Arc::new(MemoryStore::default()),
    );
    dir.path()
        .clone_into(&mut Arc::make_mut(&mut state.config).media_dir);
    Arc::make_mut(&mut state.config).music.min_free_mib = 0;
    Fixture {
        state,
        _dir: dir,
        alice,
        bob,
    }
}
fn wav(tags: bool) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("track.wav");
    let mut command = std::process::Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=437:duration=0.5",
        "-c:a",
        "pcm_s16le",
    ]);
    if tags {
        command.args([
            "-metadata",
            "title=Tag title",
            "-metadata",
            "artist=Tag artist",
            "-metadata",
            "album=Tag album",
            "-metadata",
            "genre=Ambient",
            "-metadata",
            "date=2026-10-09",
            "-metadata",
            "track=3/8",
        ]);
    }
    assert!(command.arg(&file).status().unwrap().success());
    std::fs::read(file).unwrap()
}
async fn import(f: &Fixture, owner: i64, tags: bool) -> music_assets::Asset {
    music_assets::import_with_limit(
        &f.state,
        owner,
        wav(tags),
        "upload.wav",
        plamenu::media_limits::FileSizeLimit {
            launcher: env!("CARGO_BIN_EXE_plamenu").into(),
            bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap()
}
async fn request(f: &Fixture, path: &str, range: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().uri(path);
    if let Some(range) = range {
        req = req.header(header::RANGE, range);
    }
    plamenu::build_router(f.state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn tagged_originals_are_private_reusable_deduplicated_and_range_streamed(pool: PgPool) {
    let f = fixture(pool).await;
    let a = import(&f, f.alice, true).await;
    assert_eq!(a.title, "Tag title");
    assert_eq!(a.artist, "Tag artist");
    assert_eq!(a.album, "Tag album");
    assert_eq!(a.genre, "Ambient");
    assert_eq!(a.release_year, Some(2026));
    assert_eq!(a.track_number, Some(3));
    assert_eq!(a.visibility, "private");
    assert_eq!(
        import(&f, f.alice, false).await.id,
        a.id,
        "metadata changes do not change decoded audio identity"
    );
    let b = import(&f, f.bob, true).await;
    assert_ne!(a.id, b.id);
    assert_eq!(
        a.audio_hash, b.audio_hash,
        "same bytes do not grant cross-owner access"
    );
    assert!(music_assets::get(&f.state, a.id, None).await.is_err());
    assert!(
        music_assets::get(&f.state, a.id, Some(f.bob))
            .await
            .is_err()
    );
    assert!(
        !plamenu_db::media::public_file_allowed(&f.state.pool, &a.stream_file)
            .await
            .unwrap()
    );
    let response = request(&f, &format!("/media/{}", a.stream_file), None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let stream = format!("/music/assets/{}/stream", a.id);
    assert_eq!(
        request(&f, &stream, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let changed = music_assets::update(
        &f.state,
        f.alice,
        a.id,
        &Edit {
            title: a.title.clone(),
            visibility: "public".into(),
            music: a.metadata(),
            revision: Some(a.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(changed.artist_id, a.artist_id);
    assert_eq!(changed.album_id, a.album_id);
    let response = request(&f, &stream, Some("bytes=2-11")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        format!("bytes 2-11/{}", a.stream_bytes)
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 10);
    assert_eq!(
        body.as_ref(),
        &f.state.media.get(&a.stream_file).await.unwrap()[2..12]
    );
    let hidden = music_assets::update(
        &f.state,
        f.alice,
        a.id,
        &Edit {
            title: a.title.clone(),
            visibility: "private".into(),
            music: a.metadata(),
            revision: Some(changed.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(hidden.id, a.id);
    assert_eq!(
        request(&f, &stream, Some("bytes=2-11")).await.status(),
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn grants_revoke_metadata_and_bytes_without_social_follow_access(pool: PgPool) {
    let f = fixture(pool).await;
    let a = import(&f, f.alice, true).await;
    assert!(
        music_assets::grant(&f.state, f.bob, a.id, f.bob, true)
            .await
            .is_err()
    );
    music_assets::grant(&f.state, f.alice, a.id, f.bob, true)
        .await
        .unwrap();
    assert_eq!(
        music_assets::get(&f.state, a.id, Some(f.bob))
            .await
            .unwrap()
            .id,
        a.id
    );
    plamenu_db::block::create(&f.state.pool, f.alice, f.bob, None)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, a.id, Some(f.bob))
            .await
            .is_err()
    );
    plamenu_db::block::delete(&f.state.pool, f.alice, f.bob)
        .await
        .unwrap();
    music_assets::grant(&f.state, f.alice, a.id, f.bob, false)
        .await
        .unwrap();
    assert!(
        music_assets::get(&f.state, a.id, Some(f.bob))
            .await
            .is_err()
    );
    let follows: i64 = sqlx::query_scalar("SELECT count(*) FROM follows")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(follows, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn stale_edits_foreign_deletes_and_cleanup_debt_are_atomic(pool: PgPool) {
    let f = fixture(pool).await;
    let a = import(&f, f.alice, true).await;
    let edit = Edit {
        title: "Renamed".into(),
        visibility: "local".into(),
        music: Metadata {
            artist: "New artist".into(),
            ..a.metadata()
        },
        revision: Some(a.revision.to_string()),
    };
    music_assets::update(&f.state, f.alice, a.id, &edit)
        .await
        .unwrap();
    assert!(matches!(
        music_assets::update(&f.state, f.alice, a.id, &edit).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert!(music_assets::delete(&f.state, f.bob, a.id).await.is_err());
    assert!(music_assets::get(&f.state, a.id, None).await.is_err());
    assert!(music_assets::get(&f.state, a.id, Some(f.bob)).await.is_ok());
    let refs = plamenu_db::media_cleanup::referenced_keys(&f.state.pool)
        .await
        .unwrap();
    assert!(refs.contains(&a.original_file));
    assert!(refs.contains(&a.stream_file));
    music_assets::delete(&f.state, f.alice, a.id).await.unwrap();
    assert!(
        music_assets::get(&f.state, a.id, Some(f.alice))
            .await
            .is_err()
    );
    let debt: i64 = sqlx::query_scalar("SELECT sum(size_bytes)::bigint FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, a.original_bytes + a.stream_bytes);
    let jobs = plamenu_db::media_cleanup::claim_due(&f.state.pool, 10)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        f.state.media.delete(&job.file_name).await.unwrap();
        plamenu_db::media_cleanup::complete(&f.state.pool, job.id)
            .await
            .unwrap();
    }
    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn import_failures_leave_no_files_and_quota_rejects_before_writes(pool: PgPool) {
    let mut f = fixture(pool).await;
    Arc::make_mut(&mut f.state.config).music.per_account_mib = 0;
    assert!(
        music_assets::import_with_limit(
            &f.state,
            f.alice,
            wav(true),
            "upload.wav",
            plamenu::media_limits::FileSizeLimit {
                launcher: env!("CARGO_BIN_EXE_plamenu").into(),
                bytes: 1024 * 1024
            }
        )
        .await
        .is_err()
    );
    assert!(f.state.media.list().await.unwrap().is_empty());
    let assets: i64 = sqlx::query_scalar("SELECT count(*) FROM music_assets")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(assets, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn account_deletion_revokes_assets_and_retains_global_deletion_debt(pool: PgPool) {
    let f = fixture(pool).await;
    let a = import(&f, f.alice, true).await;
    assert!(
        plamenu_db::account::delete_by_id(&f.state.pool, f.alice)
            .await
            .unwrap()
    );
    let assets: i64 = sqlx::query_scalar("SELECT count(*) FROM music_assets WHERE owner_id=$1")
        .bind(f.alice)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(assets, 0);
    let debt: i64 = sqlx::query_scalar(
        "SELECT sum(size_bytes)::bigint FROM music_storage_debt WHERE owner_id IS NULL",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(debt, a.original_bytes + a.stream_bytes);
    assert_eq!(
        request(&f, &format!("/music/assets/{}/download", a.id), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_asset_commit_retains_private_files_and_durable_cleanup_journal(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::raw_sql("CREATE FUNCTION reject_music_asset() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'asset store unavailable'; END; $$; CREATE TRIGGER reject_music_asset BEFORE INSERT ON music_assets FOR EACH ROW EXECUTE FUNCTION reject_music_asset();").execute(&f.state.pool).await.unwrap();
    assert!(
        music_assets::import_with_limit(
            &f.state,
            f.alice,
            wav(true),
            "upload.wav",
            plamenu::media_limits::FileSizeLimit {
                launcher: env!("CARGO_BIN_EXE_plamenu").into(),
                bytes: 1024 * 1024
            }
        )
        .await
        .is_err()
    );
    let files = f.state.media.list().await.unwrap();
    assert_eq!(files.len(), 2);
    for file in &files {
        assert!(
            !plamenu_db::media::public_file_allowed(&f.state.pool, file)
                .await
                .unwrap()
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_assets")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_artists")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, 2);
    sqlx::query("UPDATE media_cleanup_jobs SET run_at=now()")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 2);
    assert!(f.state.media.list().await.unwrap().is_empty());
    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn standalone_tracks_keep_release_year_without_creating_a_fake_album(pool: PgPool) {
    let f = fixture(pool).await;
    let a = import(&f, f.alice, true).await;
    let updated = music_assets::update(
        &f.state,
        f.alice,
        a.id,
        &Edit {
            title: "Standalone single".into(),
            visibility: "private".into(),
            revision: Some(a.revision.to_string()),
            music: Metadata {
                album: String::new(),
                release_year: Some(2023),
                ..a.metadata()
            },
        },
    )
    .await
    .unwrap();
    assert!(updated.album_id.is_none());
    assert_eq!(updated.release_year, Some(2023));
    assert_eq!(updated.metadata().release_year, Some(2023));
    let loaded = music_assets::get(&f.state, a.id, Some(f.alice))
        .await
        .unwrap();
    assert_eq!(loaded.release_year, Some(2023));
}

fn pcm24_variant(low_bits: i32, channels: u16, rate: u32) -> Vec<u8> {
    let mut data = Vec::new();
    for frame in 0..2400 {
        let sample = (frame % 100) * 256 + low_bits;
        let encoded = sample.to_le_bytes();
        for _ in 0..channels {
            data.extend_from_slice(&encoded[..3]);
        }
    }
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(36 + u32::try_from(data.len()).unwrap()).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * u32::from(channels) * 3).to_le_bytes());
    wav.extend_from_slice(&(channels * 3).to_le_bytes());
    wav.extend_from_slice(&24_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    wav.extend(data);
    wav
}
#[sqlx::test(migrations = "../db/migrations")]
async fn audio_deduplication_preserves_low_sample_bits_channels_and_sample_rate(pool: PgPool) {
    let f = fixture(pool).await;
    let mut ids = Vec::new();
    for (bits, channels, rate) in [
        (1, 1, 48_000),
        (2, 1, 48_000),
        (1, 2, 48_000),
        (1, 1, 44_100),
    ] {
        let asset = music_assets::import_with_limit(
            &f.state,
            f.alice,
            pcm24_variant(bits, channels, rate),
            "quality.wav",
            plamenu::media_limits::FileSizeLimit {
                launcher: env!("CARGO_BIN_EXE_plamenu").into(),
                bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();
        ids.push(asset.id);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        4,
        "deduplication must retain distinct precision, channel and rate variants"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn vorbis_and_opus_stream_tags_are_imported_with_the_original_container(pool: PgPool) {
    let f = fixture(pool).await;
    let dir = tempfile::tempdir().unwrap();
    for (index, codec) in ["libvorbis", "libopus"].iter().enumerate() {
        let file = dir.path().join(format!("{codec}.ogg"));
        assert!(
            std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("sine=frequency={}:duration=0.5", 437 + index * 100))
                .args([
                    "-c:a",
                    codec,
                    "-metadata",
                    "title=Stream tag title",
                    "-metadata",
                    "artist=Stream tag artist",
                    "-metadata",
                    "album=Stream tag album",
                    "-metadata",
                    "genre=Ambient",
                    "-metadata",
                    "date=2026",
                    "-metadata",
                    "track=3/8"
                ])
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        let imported = music_assets::import_with_limit(
            &f.state,
            f.alice,
            std::fs::read(file).unwrap(),
            "upload.ogg",
            plamenu::media_limits::FileSizeLimit {
                launcher: env!("CARGO_BIN_EXE_plamenu").into(),
                bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();
        assert_eq!(imported.title, "Stream tag title");
        assert_eq!(imported.artist, "Stream tag artist");
        assert_eq!(imported.album, "Stream tag album");
        assert_eq!(imported.genre, "Ambient");
        assert_eq!(imported.release_year, Some(2026));
        assert_eq!(imported.track_number, Some(3));
        assert_eq!(imported.original_format, "ogg");
    }
}
