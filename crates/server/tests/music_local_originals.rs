mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{auth::hash_password, music_clients, music_collections};
use plamenu_db::{PgPool, id, user};
use tower::ServiceExt;
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn indexed_originals_use_approved_held_files_without_copy_or_operator_file_cleanup(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    user::create(
        &pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let mut state = test_state_with(pool, StubFederation::with_actors([]));
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("album")).unwrap();
    let path = root.path().join("album/recording.wav");
    std::fs::write(&path, b"RIFF original").unwrap();
    std::sync::Arc::make_mut(&mut state.config)
        .music_local_roots
        .push(plamenu::music_local::config::RootConfig {
            id: "library".into(),
            label: "Library".into(),
            path: root.path().to_path_buf(),
            allow_tag_writeback: false,
            allow_rename: false,
            allow_delete: false,
        });
    let opened = plamenu::music_local::fs::Root::open(root.path()).unwrap();
    let identity = opened.identity().unwrap();
    let fingerprint = opened.file("album/recording.wav").unwrap().fingerprint;
    let collection = music_collections::create(
        &state,
        owner,
        &music_collections::New {
            name: "Mounted library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let folder = id::next();
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_local_folders(id,owner_id,collection_id,root_id,directory,root_device,root_inode) VALUES($1,$2,$3,'library','album',$4,$5)").bind(folder).bind(owner).bind(collection.id).bind(i64::try_from(identity.0).unwrap()).bind(i64::try_from(identity.1).unwrap()).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Mounted artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration,indexed_original) VALUES($1,$2,$3,'Mounted recording',$4,$5,'wav',$6,0,$7,1,true)").bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(i64::try_from(fingerprint.bytes).unwrap()).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_local_files(id,folder_id,owner_id,asset_id,relative_path,device,inode,size_bytes,modified_seconds,modified_nanos,seen_epoch,state) VALUES($1,$2,$3,$4,'album/recording.wav',$5,$6,$7,$8,$9,1,'ready')").bind(id::next()).bind(folder).bind(owner).bind(asset).bind(i64::try_from(fingerprint.device).unwrap()).bind(i64::try_from(fingerprint.inode).unwrap()).bind(i64::try_from(fingerprint.bytes).unwrap()).bind(fingerprint.modified_seconds).bind(fingerprint.modified_nanos).execute(&state.pool).await.unwrap();
    assert!(
        state
            .media
            .open(&format!("music{asset}.orig"))
            .await
            .is_err()
    );
    let password = music_clients::create(
        &state,
        owner,
        None,
        &music_clients::Request {
            label: "Read client".into(),
            days: 90,
            can_manage: false,
            broadcast_playing: false,
        },
    )
    .await
    .unwrap()
    .password
    .clone();
    let app = plamenu::build_router(state.clone());
    let query = serde_urlencoded::to_string([
        ("u", "alice"),
        ("apiKey", password.as_str()),
        ("id", &asset.to_string()),
    ])
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/rest/stream.view?{query}"))
                .header("Range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"RIFF"
    );
    assert!(
        state
            .media
            .open(&format!("music{asset}.orig"))
            .await
            .is_err()
    );
    plamenu::music_assets::delete(&state, owner, asset)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"RIFF original");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_storage_debt WHERE owner_id=$1")
            .bind(owner)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn replacing_an_approved_mount_after_headers_stops_a_held_original_body(pool: PgPool) {
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
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("approved");
    std::fs::create_dir_all(root.join("album")).unwrap();
    std::fs::write(root.join("album/recording.wav"), b"RIFF original").unwrap();
    std::sync::Arc::make_mut(&mut state.config)
        .music_local_roots
        .push(plamenu::music_local::config::RootConfig {
            id: "library".into(),
            label: "Library".into(),
            path: root.clone(),
            allow_tag_writeback: false,
            allow_rename: false,
            allow_delete: false,
        });
    let opened = plamenu::music_local::fs::Root::open(&root).unwrap();
    let identity = opened.identity().unwrap();
    let fingerprint = opened.file("album/recording.wav").unwrap().fingerprint;
    let collection = music_collections::create(
        &state,
        owner,
        &music_collections::New {
            name: "Mounted".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let folder = id::next();
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_local_folders(id,owner_id,collection_id,root_id,directory,root_device,root_inode) VALUES($1,$2,$3,'library','album',$4,$5)").bind(folder).bind(owner).bind(collection.id).bind(i64::try_from(identity.0).unwrap()).bind(i64::try_from(identity.1).unwrap()).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Mounted artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration,indexed_original) VALUES($1,$2,$3,'Mounted recording','public',$4,$5,'wav',$6,0,$7,1,true)").bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(i64::try_from(fingerprint.bytes).unwrap()).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_local_files(id,folder_id,owner_id,asset_id,relative_path,device,inode,size_bytes,modified_seconds,modified_nanos,seen_epoch,state) VALUES($1,$2,$3,$4,'album/recording.wav',$5,$6,$7,$8,$9,1,'ready')").bind(id::next()).bind(folder).bind(owner).bind(asset).bind(i64::try_from(fingerprint.device).unwrap()).bind(i64::try_from(fingerprint.inode).unwrap()).bind(i64::try_from(fingerprint.bytes).unwrap()).bind(fingerprint.modified_seconds).bind(fingerprint.modified_nanos).execute(&state.pool).await.unwrap();
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::get(format!("/music/assets/{asset}/download"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    std::fs::rename(&root, parent.path().join("previous")).unwrap();
    std::fs::create_dir_all(root.join("album")).unwrap();
    std::fs::write(root.join("album/recording.wav"), b"RIFF original").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(response.into_body().collect().await.is_err());
}
