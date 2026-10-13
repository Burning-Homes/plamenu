#![cfg(unix)]
mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    auth::hash_password,
    music_assets,
    music_local::{config::RootConfig, folders, worker},
};
use plamenu_db::{PgPool, user};
use std::sync::Arc;
fn wave() -> Vec<u8> {
    let pcm = vec![0u8; 88200];
    let len = u32::try_from(pcm.len()).unwrap();
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((len + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(44100u32.to_le_bytes());
    bytes.extend(88200u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(len.to_le_bytes());
    bytes.extend(pcm);
    bytes
}
async fn fixture(pool: PgPool) -> (AppState, i64, tempfile::TempDir) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    user::create(
        &pool,
        owner,
        Some("alice@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let mut state = test_state_with(pool, StubFederation::with_actors([]));
    let root = tempfile::tempdir().unwrap();
    Arc::make_mut(&mut state.config)
        .music_local_roots
        .push(RootConfig {
            id: "archive".into(),
            label: "Private archive".into(),
            path: root.path().to_path_buf(),
            allow_tag_writeback: false,
            allow_rename: false,
            allow_delete: false,
        });
    (state, owner, root)
}
async fn scan(state: &AppState) -> folders::Folder {
    let folder = worker::claim(state).await.unwrap().unwrap();
    worker::process(state, &folder).await.unwrap();
    folders::get(state, folder.owner_id, folder.id)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn scanner_deduplicates_pcm_without_copying_and_watches_missing_files_without_resurrecting_removals(
    pool: PgPool,
) {
    use std::os::unix::fs::symlink;
    let (state, owner, root) = fixture(pool).await;
    std::fs::create_dir(root.path().join("album")).unwrap();
    let bytes = wave();
    std::fs::write(root.path().join("album/one.wav"), &bytes).unwrap();
    std::fs::write(root.path().join("album/two.wav"), &bytes).unwrap();
    std::fs::write(root.path().join("album/broken.mp3"), b"not audio").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.wav"), &bytes).unwrap();
    symlink(outside.path(), root.path().join("album/linked")).unwrap();
    symlink(
        outside.path().join("secret.wav"),
        root.path().join("album/link.wav"),
    )
    .unwrap();
    let assigned = folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: "album".into(),
            name: "My mounted music".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(
        (current.discovered, current.indexed, current.failed),
        (3, 2, 1)
    );
    let asset: i64 = sqlx::query_scalar("SELECT id FROM music_assets WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let row = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert!(row.indexed_original);
    assert_eq!(row.stream_bytes, 0);
    assert_eq!(row.original_bytes, i64::try_from(bytes.len()).unwrap());
    assert_eq!(row.audio_hash.len(), 64);
    assert!(state.media.list().await.unwrap().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_collection_entries WHERE collection_id=$1"
        )
        .bind(assigned.collection_id)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        1
    );
    let refs = plamenu_db::media_cleanup::referenced_keys(&state.pool)
        .await
        .unwrap();
    assert!(!refs.contains(&row.original_file));
    assert!(!refs.contains(&row.stream_file));
    std::fs::remove_file(root.path().join("album/one.wav")).unwrap();
    folders::refresh(&state, owner, current.id, current.revision, true, true)
        .await
        .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 1);
    std::fs::remove_file(root.path().join("album/two.wav")).unwrap();
    folders::refresh(&state, owner, current.id, current.revision, true, true)
        .await
        .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_collection_entries WHERE collection_id=$1"
        )
        .bind(assigned.collection_id)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        0
    );
    std::fs::write(root.path().join("album/two.wav"), &bytes).unwrap();
    folders::refresh(&state, owner, current.id, current.revision, true, true)
        .await
        .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 1);
    music_assets::delete(&state, owner, asset).await.unwrap();
    folders::refresh(&state, owner, current.id, current.revision, true, true)
        .await
        .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 0);
    assert_eq!(
        std::fs::read(root.path().join("album/two.wav")).unwrap(),
        bytes
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_assets")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_storage_debt")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn assignments_reject_unknown_roots_traversal_and_symlinks_and_scan_generations_fence_disable(
    pool: PgPool,
) {
    let (state, owner, root) = fixture(pool).await;
    let mut input = folders::New {
        owner_id: owner,
        root_id: "unknown".into(),
        directory: String::new(),
        name: "Library".into(),
        watch: false,
        options: plamenu::music_local::options::Options::default(),
    };
    assert!(folders::assign(&state, &input).await.is_err());
    input.root_id = "archive".into();
    input.directory = "../".into();
    assert!(folders::assign(&state, &input).await.is_err());
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    input.directory = "link".into();
    assert!(folders::assign(&state, &input).await.is_err());
    input.directory = String::new();
    let assigned = folders::assign(&state, &input).await.unwrap();
    assert!(
        !serde_json::to_string(&assigned)
            .unwrap()
            .contains(root.path().to_str().unwrap())
    );
    let claimed = worker::claim(&state).await.unwrap().unwrap();
    assert!(worker::claim(&state).await.unwrap().is_none());
    assert!(
        folders::refresh(
            &state,
            owner + 1,
            assigned.id,
            assigned.revision,
            false,
            false
        )
        .await
        .is_err()
    );
    folders::refresh(&state, owner, assigned.id, assigned.revision, false, false)
        .await
        .unwrap();
    assert!(worker::process(&state, &claimed).await.is_err());
    assert!(worker::claim(&state).await.unwrap().is_none());
    assert!(
        folders::refresh(&state, owner, assigned.id, assigned.revision, false, true)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_local_files")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
async fn token(state: &AppState, owner: i64, scopes: &str) -> String {
    use plamenu::auth::{generate_secret, hash_secret};
    use plamenu_db::oauth;
    let user = user::find_by_account_id(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Folder test",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write admin:read admin:write",
        },
    )
    .await
    .unwrap();
    let secret = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&secret),
        app.id,
        Some(user.id),
        scopes,
    )
    .await
    .unwrap();
    secret
}
async fn api(
    app: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn mount_assignment_api_requires_operator_role_and_scopes_and_never_exposes_absolute_paths(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    let (state, owner, root) = fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    let scopes_only = token(&state, owner, "read write admin:read admin:write").await;
    let input = serde_json::json!({"owner_id":owner,"root_id":"archive","directory":"","name":"Private music"});
    assert_eq!(
        api(
            &app,
            &scopes_only,
            "POST",
            "/api/v1/admin/music/folders",
            input.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let role = plamenu_db::role::find_by_name(&state.pool, "Admin")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::role::assign_to_account(&state.pool, owner, Some(role.id))
        .await
        .unwrap();
    let no_scope = token(&state, owner, "read write").await;
    assert_eq!(
        api(
            &app,
            &no_scope,
            "POST",
            "/api/v1/admin/music/folders",
            input.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let created = api(
        &app,
        &scopes_only,
        "POST",
        "/api/v1/admin/music/folders",
        input,
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created = created.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&created).contains(root.path().to_str().unwrap()));
    let folder: serde_json::Value = serde_json::from_slice(&created).unwrap();
    let id = folder["id"].as_str().unwrap().parse::<i64>().unwrap();
    let read = token(&state, owner, "read:statuses").await;
    assert_eq!(
        api(
            &app,
            &read,
            "GET",
            "/api/v1/music/folders",
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        api(
            &app,
            &read,
            "PUT",
            &format!("/api/v1/music/folders/{id}"),
            serde_json::json!({"revision":1,"enabled":true,"watch":true})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let roots = api(
        &app,
        &scopes_only,
        "GET",
        "/api/v1/admin/music/roots",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(roots.status(), StatusCode::OK);
    let roots = roots.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&roots).contains(root.path().to_str().unwrap()));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_source_mutations_require_owner_and_operator_capabilities_and_preserve_pcm_tags_and_no_clobber(
    pool: PgPool,
) {
    use plamenu::music_local::files::{self, Mutation};
    let (mut state, owner, root) = fixture(pool).await;
    std::fs::write(root.path().join("recording.wav"), wave()).unwrap();
    let assigned = folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Source files".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    let files = files::list(&state, owner, assigned.id, None).await.unwrap();
    let file = files[0].id;
    let asset = files[0].asset_id.unwrap();
    let original = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    let rename = Mutation::Rename {
        revision: current.revision,
        name: "renamed.wav".into(),
    };
    assert!(
        files::mutate(&state, owner, assigned.id, file, &rename)
            .await
            .is_err()
    );
    let config = &mut Arc::make_mut(&mut state.config).music_local_roots[0];
    config.allow_rename = true;
    config.allow_tag_writeback = true;
    config.allow_delete = true;
    assert!(
        files::mutate(&state, owner + 1, assigned.id, file, &rename)
            .await
            .is_err()
    );
    assert!(
        files::mutate(
            &state,
            owner,
            assigned.id,
            file,
            &Mutation::Rename {
                revision: current.revision,
                name: "../escape.wav".into()
            }
        )
        .await
        .is_err()
    );
    std::fs::write(root.path().join("occupied.wav"), b"keep").unwrap();
    assert!(
        files::mutate(
            &state,
            owner,
            assigned.id,
            file,
            &Mutation::Rename {
                revision: current.revision,
                name: "occupied.wav".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read(root.path().join("occupied.wav")).unwrap(),
        b"keep"
    );
    files::mutate(&state, owner, assigned.id, file, &rename)
        .await
        .unwrap();
    assert!(!root.path().join("recording.wav").exists());
    assert!(root.path().join("renamed.wav").exists());
    assert!(
        files::mutate(&state, owner, assigned.id, file, &rename)
            .await
            .is_err()
    );
    let current = scan(&state).await;
    assert_eq!(current.indexed, 1);
    music_assets::update(
        &state,
        owner,
        asset,
        &music_assets::Edit {
            title: "Updated source title".into(),
            visibility: "private".into(),
            music: original.metadata(),
            revision: Some(original.revision.to_string()),
        },
    )
    .await
    .unwrap();
    files::mutate_with_launcher(
        &state,
        owner,
        assigned.id,
        file,
        &Mutation::WriteTags {
            revision: current.revision,
        },
        std::path::Path::new(env!("CARGO_BIN_EXE_plamenu")),
    )
    .await
    .unwrap();
    let probe = tokio::process::Command::new(&state.config.ffprobe_path)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format_tags=title",
            "-of",
            "json",
        ])
        .arg(root.path().join("renamed.wav"))
        .output()
        .await
        .unwrap();
    assert!(probe.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(metadata["format"]["tags"]["title"], "Updated source title");
    let current = scan(&state).await;
    assert_eq!(current.indexed, 1);
    let updated = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert_eq!(updated.audio_hash, original.audio_hash);
    assert!(
        files::mutate(
            &state,
            owner,
            assigned.id,
            file,
            &Mutation::Delete {
                revision: current.revision,
                confirm: false
            }
        )
        .await
        .is_err()
    );
    files::mutate(
        &state,
        owner,
        assigned.id,
        file,
        &Mutation::Delete {
            revision: current.revision,
            confirm: true,
        },
    )
    .await
    .unwrap();
    assert!(!root.path().join("renamed.wav").exists());
    assert_eq!(scan(&state).await.indexed, 0);
    assert!(state.media.list().await.unwrap().is_empty());
}
#[sqlx::test(migrations = "../db/migrations")]
async fn mounted_original_stream_and_publication_use_real_native_encoders_and_keep_original_out_of_storage(
    pool: PgPool,
) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (mut state, owner, root) = fixture(pool).await;
    std::fs::write(root.path().join("recording.wav"), wave()).unwrap();
    state.media =
        Arc::new(plamenu::storage::LocalDiskStore::new(root.path().join("store")).unwrap());
    let config = Arc::make_mut(&mut state.config);
    config.media_dir = root.path().join("store");
    config.music.min_free_mib = 0;
    config.music.max_rendition_mib = 1;
    config.music.rendition_scratch_mib = 4;
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Mounted library".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    scan(&state).await;
    let asset: i64 = sqlx::query_scalar("SELECT id FROM music_assets")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let row = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    let row = music_assets::update(
        &state,
        owner,
        asset,
        &music_assets::Edit {
            title: row.title.clone(),
            visibility: "public".into(),
            music: row.metadata(),
            revision: Some(row.revision.to_string()),
        },
    )
    .await
    .unwrap();
    let worker_state = state.clone();
    let work = tokio::spawn(async move {
        let job = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(job) = sqlx::query_scalar::<_, i64>(
                    "SELECT id FROM music_renditions WHERE state='queued' LIMIT 1",
                )
                .fetch_optional(&worker_state.pool)
                .await
                .unwrap()
                {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            plamenu::music_renditions::run_one(
                &worker_state,
                job,
                env!("CARGO_BIN_EXE_plamenu").into()
            )
            .await
            .unwrap()
        );
    });
    let app = plamenu::build_router(state.clone());
    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/music/assets/{asset}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_renditions")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/music/assets/{asset}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mp3 = response.into_body().collect().await.unwrap().to_bytes();
    work.await.unwrap();
    assert!(mp3.len() > 1000);
    std::fs::write(root.path().join("decoded.mp3"), &mp3).unwrap();
    let decoded = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-v", "error", "-i"])
        .arg(root.path().join("decoded.mp3"))
        .args(["-f", "null", "-"])
        .output()
        .await
        .unwrap();
    assert!(decoded.status.success());
    let account = plamenu_db::account::find_by_id(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    let status = plamenu::music_publications::publish(&state, &account, asset)
        .await
        .unwrap();
    assert_eq!(status.object_type.as_deref(), Some("Audio"));
    let source = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert!(source.stream_bytes > 0);
    assert!(state.media.open(&source.stream_file).await.is_ok());
    assert!(state.media.open(&row.original_file).await.is_err());
    assert_eq!(
        std::fs::read(root.path().join("recording.wav")).unwrap(),
        wave()
    );
    let live_status = plamenu::entities::render_statuses(
        &state.pool,
        &state.config.domain,
        std::slice::from_ref(&status),
        None,
    )
    .await
    .unwrap();
    let live_url = url::Url::parse(
        live_status[0]["media_attachments"][0]["url"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let live_attachment = app
        .clone()
        .oneshot(
            Request::get(live_url.path())
                .header("Range", "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        live_attachment.status(),
        StatusCode::PARTIAL_CONTENT,
        "live published attachment URL must be playable"
    );
    // Mount-backed playback remains source-bound even when publication retained
    // a portable cache internally. Internal music keys are permanently private.
    let held = app
        .clone()
        .oneshot(
            Request::get(format!("/music/assets/{asset}/stream"))
                .header("Range", "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::PARTIAL_CONTENT);
    std::fs::remove_file(root.path().join("recording.wav")).unwrap();
    // The shared byte-authority contract refreshes at most one second apart.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    if let Ok(bytes) = held.into_body().collect().await {
        assert!(bytes.to_bytes().is_empty());
    }
    let missing = app
        .clone()
        .oneshot(
            Request::get(format!("/music/assets/{asset}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let rendered = plamenu::entities::render_statuses(
        &state.pool,
        &state.config.domain,
        std::slice::from_ref(&status),
        None,
    )
    .await
    .unwrap();
    let media_url =
        url::Url::parse(rendered[0]["media_attachments"][0]["url"].as_str().unwrap()).unwrap();
    let published = app
        .clone()
        .oneshot(Request::get(media_url.path()).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(published.status(), StatusCode::NOT_FOUND);
    let stored_url = format!("/media/{}", source.stream_file);
    let cached = app
        .clone()
        .oneshot(Request::get(&stored_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_FOUND);
    assert!(state.media.open(&source.stream_file).await.is_ok());
    let original = app
        .clone()
        .oneshot(
            Request::get(format!("/music/assets/{asset}/download"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(original.status(), StatusCode::NOT_FOUND);
    let current = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    music_assets::update(
        &state,
        owner,
        asset,
        &music_assets::Edit {
            title: current.title.clone(),
            visibility: "private".into(),
            music: current.metadata(),
            revision: Some(current.revision.to_string()),
        },
    )
    .await
    .unwrap();
    let published_revoked = app
        .clone()
        .oneshot(Request::get(&stored_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(published_revoked.status(), StatusCode::NOT_FOUND);
    let revoked = app
        .oneshot(
            Request::get(format!("/music/assets/{asset}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NOT_FOUND);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_reindex_restores_no_copy_membership_and_disconnect_preserves_files_and_fences_workers(
    pool: PgPool,
) {
    let (state, owner, root) = fixture(pool).await;
    std::fs::write(root.path().join("song.wav"), wave()).unwrap();
    let assigned = folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Mounted music".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    let row = plamenu::music_local::files::list(&state, owner, assigned.id, None)
        .await
        .unwrap()
        .remove(0);
    let old = row.asset_id.unwrap();
    music_assets::delete(&state, owner, old).await.unwrap();
    assert!(
        plamenu::music_local::files::restore(
            &state,
            owner + 1,
            assigned.id,
            row.id,
            current.revision
        )
        .await
        .is_err()
    );
    plamenu::music_local::files::restore(&state, owner, assigned.id, row.id, current.revision)
        .await
        .unwrap();
    assert!(
        plamenu::music_local::files::restore(&state, owner, assigned.id, row.id, current.revision)
            .await
            .is_err()
    );
    let current = scan(&state).await;
    assert_eq!(current.indexed, 1);
    let restored = plamenu::music_local::files::list(&state, owner, assigned.id, None)
        .await
        .unwrap()
        .remove(0)
        .asset_id
        .unwrap();
    assert_ne!(old, restored);
    assert!(state.media.list().await.unwrap().is_empty());
    folders::refresh(&state, owner, current.id, current.revision, true, true)
        .await
        .unwrap();
    let claimed = worker::claim(&state).await.unwrap().unwrap();
    assert!(
        folders::disconnect(&state, owner + 1, assigned.id, claimed.revision)
            .await
            .is_err()
    );
    folders::disconnect(&state, owner, assigned.id, claimed.revision)
        .await
        .unwrap();
    assert!(worker::process(&state, &claimed).await.is_err());
    assert_eq!(std::fs::read(root.path().join("song.wav")).unwrap(), wave());
    assert!(folders::list(&state, owner).await.unwrap().is_empty());
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Reconnected".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(scan(&state).await.indexed, 1);
    let mut tx = state.pool.begin().await.unwrap();
    plamenu_db::music::retire_account_assets(&mut tx, owner)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(std::fs::read(root.path().join("song.wav")).unwrap(), wave());
    assert!(folders::list(&state, owner).await.unwrap().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_storage_debt")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn musical_probe_refuses_manifest_demuxers_and_indexes_only_the_bounded_audio_file(
    pool: PgPool,
) {
    let (state, owner, root) = fixture(pool).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("private.wav"), wave()).unwrap();
    std::fs::write(
        root.path().join("manifest.wav"),
        format!(
            "ffconcat version 1.0\nfile '{}'\n",
            outside.path().join("private.wav").display()
        ),
    )
    .unwrap();
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Local files".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(
        (current.discovered, current.indexed, current.failed),
        (1, 0, 1)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_assets")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(state.media.list().await.unwrap().is_empty());
}
async fn web(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    path: &str,
    body: String,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    app.clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn native_folder_forms_use_local_handles_current_csrf_and_explicit_no_copy_restore_disconnect(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    let (state, owner, root) = fixture(pool).await;
    std::fs::write(root.path().join("recording.wav"), wave()).unwrap();
    let role = plamenu_db::role::find_by_name(&state.pool, "Admin")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::role::assign_to_account(&state.pool, owner, Some(role.id))
        .await
        .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = web(
        &app,
        None,
        "POST",
        "/login",
        "email=alice%40example.test&password=fixture-password".into(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let page = web(&app, Some(&cookie), "GET", "/music/folders", String::new()).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("name=\"username\""));
    assert!(!html.contains(root.path().to_str().unwrap()));
    let body = serde_urlencoded::to_string([
        ("csrf", "wrong"),
        ("username", "alice"),
        ("root_id", "archive"),
        ("name", "Music archive"),
    ])
    .unwrap();
    assert_eq!(
        web(
            &app,
            Some(&cookie),
            "POST",
            "/web/music/folders/assign",
            body
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(folders::list(&state, owner).await.unwrap().is_empty());
    let body = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("username", "alice"),
        ("root_id", "archive"),
        ("name", "Music archive"),
    ])
    .unwrap();
    assert_eq!(
        web(
            &app,
            Some(&cookie),
            "POST",
            "/web/music/folders/assign",
            body
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let current = scan(&state).await;
    let file = plamenu::music_local::files::list(&state, owner, current.id, None)
        .await
        .unwrap()
        .remove(0);
    music_assets::delete(&state, owner, file.asset_id.unwrap())
        .await
        .unwrap();
    let path = format!("/music/folders/{}/files", current.id);
    let page = web(&app, Some(&cookie), "GET", &path, String::new()).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Reindex this source file"));
    assert!(!html.contains("Delete original file"));
    let path = format!(
        "/web/music/folders/{}/files/{}/restore",
        current.id, file.id
    );
    let body = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("revision", &current.revision.to_string()),
    ])
    .unwrap();
    assert_eq!(
        web(&app, Some(&cookie), "POST", &path, body).await.status(),
        StatusCode::SEE_OTHER
    );
    let restored = scan(&state).await;
    assert_eq!(restored.indexed, 1);
    assert!(state.media.list().await.unwrap().is_empty());
    let body = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("revision", &restored.revision.to_string()),
    ])
    .unwrap();
    assert_eq!(
        web(
            &app,
            Some(&cookie),
            "POST",
            &format!("/web/music/folders/{}/disconnect", current.id),
            body
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(folders::list(&state, owner).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read(root.path().join("recording.wav")).unwrap(),
        wave()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn owner_scan_choices_select_extensions_subfolders_and_exclusions_then_reconcile_current_membership(
    pool: PgPool,
) {
    use plamenu::music_local::options::Options;
    let (state, owner, root) = fixture(pool).await;
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::create_dir(root.path().join("skip")).unwrap();
    for path in ["one.WAV", "demo.wav", "nested/two.wav", "skip/three.wav"] {
        std::fs::write(root.path().join(path), wave()).unwrap();
    }
    std::fs::write(root.path().join("image.png"), b"not audio").unwrap();
    let assigned = folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Selected files".into(),
            watch: true,
            options: Options {
                recursive: false,
                extensions: vec![".WAV".into()],
                exclude: vec!["demo.*".into()],
            },
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(
        (current.discovered, current.indexed, current.failed),
        (1, 1, 0)
    );
    let choices = Options {
        recursive: true,
        extensions: vec!["wav".into()],
        exclude: vec!["demo.*".into(), "skip/*".into()],
    };
    folders::configure(
        &state,
        owner,
        assigned.id,
        current.revision,
        true,
        true,
        Some(&choices),
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 2);
    assert_eq!(current.failed, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_assets")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        1
    );
    let empty = Options {
        extensions: Vec::new(),
        ..Default::default()
    };
    folders::configure(
        &state,
        owner,
        assigned.id,
        current.revision,
        true,
        true,
        Some(&empty),
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.discovered, 0);
    assert_eq!(current.indexed, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_collection_entries WHERE collection_id=$1"
        )
        .bind(assigned.collection_id)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(std::fs::read(root.path().join("one.WAV")).unwrap(), wave());
    assert!(state.media.list().await.unwrap().is_empty());
    let unsafe_options = Options {
        extensions: vec!["m3u8".into()],
        ..Default::default()
    };
    assert!(
        folders::configure(
            &state,
            owner,
            assigned.id,
            current.revision,
            true,
            true,
            Some(&unsafe_options)
        )
        .await
        .is_err()
    );
}
async fn illustrated_mp3(state: &AppState, root: &std::path::Path) -> Vec<u8> {
    let picture = image::RgbImage::from_pixel(128, 64, image::Rgb([40, 120, 180]));
    picture.save(root.join("cover.jpg")).unwrap();
    std::fs::write(root.join("input.wav"), wave()).unwrap();
    let output = root.join("illustrated.mp3");
    let result = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(root.join("input.wav"))
        .arg("-i")
        .arg(root.join("cover.jpg"))
        .args([
            "-map",
            "0:a:0",
            "-map",
            "1:v:0",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "copy",
            "-disposition:v",
            "attached_pic",
            "-metadata",
            "title=Illustrated recording",
        ])
        .arg(&output)
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(output).unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn embedded_pictures_are_normalized_for_mounted_and_uploaded_audio_with_current_private_artwork_permissions(
    pool: PgPool,
) {
    use plamenu::music_artworks;
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();
    let (mut state, owner, root) = fixture(pool).await;
    let scratch = tempfile::tempdir().unwrap();
    let config = Arc::make_mut(&mut state.config);
    config.media_dir = scratch.path().join("store");
    config.music.min_free_mib = 0;
    std::fs::create_dir_all(&config.media_dir).unwrap();
    let source = illustrated_mp3(&state, scratch.path()).await;
    std::fs::write(root.path().join("recording.mp3"), &source).unwrap();
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Illustrated library".into(),
            watch: true,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!((current.indexed, current.failed), (1, 0));
    let asset: i64 = sqlx::query_scalar("SELECT id FROM music_assets WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let row = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    let image = row.artwork_id.expect("embedded source cover indexed");
    let art = music_artworks::get(&state, image, Some(owner))
        .await
        .unwrap();
    assert_eq!((art.width, art.height), (128, 64));
    assert!(art.size_bytes > 0);
    assert!(state.media.open(&row.original_file).await.is_err());
    let encoded = state.media.get(&art.file_name).await.unwrap();
    assert_eq!(image::load_from_memory(&encoded).unwrap().width(), 128);
    assert!(music_artworks::get(&state, image, None).await.is_err());
    let bob = create_local_account(&state.pool, "bob", "Bob").await.id;
    user::create(
        &state.pool,
        bob,
        Some("bob@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let imported = music_assets::import_with_limit(
        &state,
        bob,
        source.clone(),
        "Uploaded recording.mp3",
        plamenu::media_limits::FileSizeLimit {
            launcher: env!("CARGO_BIN_EXE_plamenu").into(),
            bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let own = imported
        .artwork_id
        .expect("embedded uploaded cover indexed");
    assert_ne!(image, own);
    assert!(music_artworks::get(&state, own, Some(owner)).await.is_err());
    assert!(music_artworks::get(&state, image, Some(bob)).await.is_err());
    assert_eq!(imported.audio_hash, row.audio_hash);
    let duplicate = music_assets::import_with_limit(
        &state,
        bob,
        source.clone(),
        "Same audio",
        plamenu::media_limits::FileSizeLimit {
            launcher: env!("CARGO_BIN_EXE_plamenu").into(),
            bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(duplicate.id, imported.id);
    assert_eq!(duplicate.artwork_id, Some(own));
    assert_eq!(
        std::fs::read(root.path().join("recording.mp3")).unwrap(),
        source
    );
}
fn selected_png() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(64, 32, image::Rgb([180, 60, 30]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn selected_track_cover_and_tags_write_back_to_five_real_containers_without_changing_pcm_or_copying_originals(
    pool: PgPool,
) {
    use plamenu::{music_artworks, music_local::files};
    let (mut state, owner, root) = fixture(pool).await;
    let scratch = tempfile::tempdir().unwrap();
    let config = Arc::make_mut(&mut state.config);
    config.media_dir = scratch.path().join("store");
    config.music.min_free_mib = 0;
    config.music_local_roots[0].allow_tag_writeback = true;
    std::fs::create_dir_all(&config.media_dir).unwrap();
    for (index, (format, codec)) in [
        ("mp3", "libmp3lame"),
        ("flac", "flac"),
        ("ogg", "libvorbis"),
        ("opus", "libopus"),
        ("m4a", "aac"),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = format!("container-{format}");
        std::fs::create_dir(root.path().join(&directory)).unwrap();
        let relative = format!("{directory}/recording.{format}");
        let path = root.path().join(&relative);
        let tone = format!(
            "sine=frequency={}:sample_rate=48000:duration=0.5",
            420 + index * 97
        );
        let created = tokio::process::Command::new(&state.config.ffmpeg_path)
            .args([
                "-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", &tone, "-c:a", codec,
            ])
            .arg(&path)
            .output()
            .await
            .unwrap();
        assert!(
            created.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        folders::assign(
            &state,
            &folders::New {
                owner_id: owner,
                root_id: "archive".into(),
                directory,
                name: format!("{format} source"),
                watch: false,
                options: plamenu::music_local::options::Options::default(),
            },
        )
        .await
        .unwrap();
        let current = scan(&state).await;
        assert_eq!(current.indexed, 1, "{format}");
        let file = files::list(&state, owner, current.id, None)
            .await
            .unwrap()
            .remove(0);
        let asset = file.asset_id.unwrap();
        let before = music_assets::get(&state, asset, Some(owner)).await.unwrap();
        music_artworks::upload(
            &state,
            owner,
            music_artworks::Target::Assets,
            asset,
            "image",
            &before.revision.to_string(),
            selected_png(),
        )
        .await
        .unwrap();
        files::mutate_with_launcher(
            &state,
            owner,
            current.id,
            file.id,
            &files::Mutation::WriteTags {
                revision: current.revision,
            },
            std::path::Path::new(env!("CARGO_BIN_EXE_plamenu")),
        )
        .await
        .unwrap_or_else(|error| panic!("{format}: {error:?}"));
        let probe = tokio::process::Command::new(&state.config.ffprobe_path)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=width,height:stream_disposition=attached_pic",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
            .await
            .unwrap();
        assert!(probe.status.success());
        let shape: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert!(
            shape["streams"].as_array().unwrap().iter().any(
                |stream| stream["disposition"]["attached_pic"] == 1
                    && stream["width"] == 64
                    && stream["height"] == 32
            ),
            "{format}: {shape}"
        );
        let current = scan(&state).await;
        assert_eq!(current.indexed, 1, "{format}");
        let after = music_assets::get(&state, asset, Some(owner)).await.unwrap();
        assert_eq!(after.audio_hash, before.audio_hash, "{format}");
        assert!(state.media.open(&after.original_file).await.is_err());
        assert!(path.exists());
    }
}
async fn external_tags(state: &AppState, path: &std::path::Path, title: &str) -> Vec<u8> {
    let temporary = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile_in(path.parent().unwrap())
        .unwrap();
    let result = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-c", "copy", "-metadata"])
        .arg(format!("title={title}"))
        .arg(temporary.path())
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::rename(temporary.path(), path).unwrap();
    std::fs::read(path).unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn external_tag_only_edits_sync_current_publication_until_owner_personalization_and_keep_the_audio_identity(
    pool: PgPool,
) {
    let (mut state, owner, root) = fixture(pool).await;
    let scratch = tempfile::tempdir().unwrap();
    state.media =
        Arc::new(plamenu::storage::LocalDiskStore::new(scratch.path().join("store")).unwrap());
    let config = Arc::make_mut(&mut state.config);
    config.media_dir = scratch.path().join("store");
    config.music.min_free_mib = 0;
    config.music.max_rendition_mib = 1;
    config.music.rendition_scratch_mib = 4;
    let path = root.path().join("recording.wav");
    std::fs::write(&path, wave()).unwrap();
    external_tags(&state, &path, "First source title").await;
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Source tags".into(),
            watch: false,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    let file = plamenu::music_local::files::list(&state, owner, current.id, None)
        .await
        .unwrap()
        .remove(0);
    let asset = file.asset_id.unwrap();
    let first = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert_eq!(first.title, "First source title");
    assert_eq!(first.indexed_tags_revision, Some(first.revision));
    let public = music_assets::update(
        &state,
        owner,
        asset,
        &music_assets::Edit {
            title: first.title.clone(),
            music: first.metadata(),
            visibility: "public".into(),
            revision: Some(first.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(public.indexed_tags_revision, Some(public.revision));
    let selection = plamenu::music_renditions::negotiate(&state, Some(owner), "mp3", Some(192))
        .await
        .unwrap();
    let rendition = plamenu::music_renditions::request(&state, asset, Some(owner), selection)
        .await
        .unwrap();
    assert!(
        plamenu::music_renditions::run_one(
            &state,
            rendition.id,
            env!("CARGO_BIN_EXE_plamenu").into()
        )
        .await
        .unwrap()
    );
    let account = plamenu_db::account::find_by_id(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    let published = plamenu::music_publications::publish(&state, &account, asset)
        .await
        .unwrap();
    external_tags(&state, &path, "Changed on disk").await;
    folders::refresh(&state, owner, current.id, current.revision, false, true)
        .await
        .unwrap();
    let current = scan(&state).await;
    let synced = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert_eq!(synced.title, "Changed on disk");
    assert_eq!(synced.audio_hash, first.audio_hash);
    assert_eq!(synced.indexed_tags_revision, Some(synced.revision));
    let updated = plamenu_db::status::find_by_id(&state.pool, published.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title.as_deref(), Some("Changed on disk"));
    let customized = music_assets::update(
        &state,
        owner,
        asset,
        &music_assets::Edit {
            title: "My title".into(),
            music: synced.metadata(),
            visibility: "public".into(),
            revision: Some(synced.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_ne!(customized.indexed_tags_revision, Some(customized.revision));
    external_tags(&state, &path, "Later disk edit").await;
    folders::refresh(&state, owner, current.id, current.revision, false, true)
        .await
        .unwrap();
    scan(&state).await;
    let protected = music_assets::get(&state, asset, Some(owner)).await.unwrap();
    assert_eq!(protected.title, "My title");
    assert_eq!(protected.audio_hash, first.audio_hash);
    let protected_post = plamenu_db::status::find_by_id(&state.pool, published.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(protected_post.title.as_deref(), Some("My title"));
    assert!(state.media.open(&first.original_file).await.is_err());
}
#[sqlx::test(migrations = "../db/migrations")]
async fn duplicate_pcm_original_fallback_uses_the_current_container_size_and_filename_and_pages_past_stale_locations(
    pool: PgPool,
) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (state, owner, root) = fixture(pool).await;
    let original = wave();
    std::fs::write(root.path().join("one.wav"), &original).unwrap();
    let encoded = root.path().join("two.flac");
    let made = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-v", "error", "-y", "-i"])
        .arg(root.path().join("one.wav"))
        .args(["-c:a", "flac"])
        .arg(&encoded)
        .output()
        .await
        .unwrap();
    assert!(made.status.success());
    let flac = std::fs::read(&encoded).unwrap();
    folders::assign(
        &state,
        &folders::New {
            owner_id: owner,
            root_id: "archive".into(),
            directory: String::new(),
            name: "Original encodings".into(),
            watch: false,
            options: plamenu::music_local::options::Options::default(),
        },
    )
    .await
    .unwrap();
    let current = scan(&state).await;
    assert_eq!(current.indexed, 2);
    let file = plamenu::music_local::files::list(&state, owner, current.id, None)
        .await
        .unwrap()
        .remove(0);
    let asset = file.asset_id.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_assets")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        1
    );
    // Add 101 stale entries before both real references to prove selection
    // continues after the first batch, not merely after one bad original.
    sqlx::query("INSERT INTO music_local_files(id,folder_id,owner_id,asset_id,relative_path,device,inode,size_bytes,modified_seconds,modified_nanos,seen_epoch,state,original_format) SELECT n,$1,$2,$3,'missing-'||n||'.wav',0,1,1,0,0,1,'ready','wav' FROM generate_series(1,101) n")
        .bind(current.id).bind(owner).bind(asset).execute(&state.pool).await.unwrap();
    std::fs::remove_file(root.path().join("one.wav")).unwrap();
    let secret = token(&state, owner, "read:statuses").await;
    let app = plamenu::build_router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/music/assets/{asset}/download"))
                .header("Authorization", format!("Bearer {secret}"))
                .header("Range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()["Content-Disposition"],
        format!("attachment; filename=\"track-{asset}.flac\"")
    );
    assert_eq!(
        response.headers()["Content-Range"],
        format!("bytes 0-3/{}", flac.len())
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"fLaC"
    );
    assert!(state.media.list().await.unwrap().is_empty());
}
