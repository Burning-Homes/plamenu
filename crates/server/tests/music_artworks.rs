mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with_store,
};
use plamenu::{
    AppState,
    music_artworks::{self, Target},
    music_assets, music_collections,
    music_entities::{self, Kind},
    music_publications,
    storage::MemoryStore,
};
use plamenu_db::{PgPool, account::Account, id};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    alice: Account,
    bob: Account,
    asset: i64,
    artist: i64,
    album: i64,
    collection: i64,
    _dir: tempfile::TempDir,
}
async fn fixture(pool: PgPool) -> Fixture {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state_with_store(
        pool,
        StubFederation::with_actors([]),
        Arc::new(MemoryStore::default()),
    );
    dir.path()
        .clone_into(&mut Arc::make_mut(&mut state.config).media_dir);
    Arc::make_mut(&mut state.config).music.min_free_mib = 0;
    let artist = id::next();
    let album = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(alice.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,release_year) VALUES($1,$2,$3,'Album',2026)").bind(album).bind(alice.id).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Track',$3,$4,$5,$6,100,100,$7,10,'mp3')").bind(asset).bind(alice.id).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    let collection = music_collections::create(
        &state,
        alice.id,
        &music_collections::New {
            name: "Library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap()
    .id;
    Fixture {
        state,
        alice,
        bob,
        asset,
        artist,
        album,
        collection,
        _dir: dir,
    }
}
fn png() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(1440, 720, image::Rgb([60, 80, 120]));
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}
async fn upload(f: &Fixture, target: Target, entity: i64, slot: &str) -> music_artworks::Artwork {
    let (_, revision) = music_artworks::authorize(&f.state, target, entity, Some(f.alice.id))
        .await
        .unwrap();
    music_artworks::upload(
        &f.state,
        f.alice.id,
        target,
        entity,
        slot,
        &revision.to_string(),
        png(),
    )
    .await
    .unwrap()
}
async fn public(f: &Fixture) {
    let track = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.alice.id,
        f.asset,
        &music_assets::Edit {
            title: track.title.clone(),
            music: track.metadata(),
            visibility: "public".into(),
            revision: Some(track.revision.to_string()),
        },
    )
    .await
    .unwrap();
}
async fn status(f: &Fixture, path: &str) -> StatusCode {
    plamenu::build_router(f.state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn private_album_notes_do_not_revoke_a_cover_inherited_by_a_readable_track(pool: PgPool) {
    let f = fixture(pool).await;
    let cover = upload(&f, Target::Albums, f.album, "cover").await;
    public(&f).await;
    let album = music_entities::get(&f.state, Kind::Album, f.album, Some(f.alice.id))
        .await
        .unwrap();
    plamenu::music_album_access::update(
        &f.state,
        f.alice.id,
        f.album,
        &plamenu::music_album_access::Edit {
            revision: album.revision.to_string(),
            visibility: "private".into(),
            share_tracks: false,
        },
    )
    .await
    .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, None)
            .await
            .is_err()
    );
    let track = music_assets::get(&f.state, f.asset, None).await.unwrap();
    assert_eq!(track.artwork_id, Some(cover.id));
    assert_eq!(
        status(&f, &format!("/music/artwork/{}", cover.id)).await,
        StatusCode::OK
    );
    let own = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.alice.id,
        f.asset,
        &music_assets::Edit {
            title: own.title.clone(),
            music: own.metadata(),
            visibility: "private".into(),
            revision: Some(own.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        status(&f, &format!("/music/artwork/{}", cover.id)).await,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn normalized_artwork_obeys_current_source_access_and_never_uses_bearer_files(pool: PgPool) {
    let f = fixture(pool).await;
    for (target, entity, slot) in [
        (Target::Assets, f.asset, "image"),
        (Target::Artists, f.artist, "image"),
        (Target::Albums, f.album, "cover"),
    ] {
        let artwork = upload(&f, target, entity, slot).await;
        assert_eq!((artwork.width, artwork.height), (1024, 512));
        assert_eq!(artwork.content_type, "image/jpeg");
        let bytes = f.state.media.get(&artwork.file_name).await.unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), 1024);
        let url = format!("/music/artwork/{}", artwork.id);
        assert_eq!(status(&f, &url).await, StatusCode::NOT_FOUND);
        assert_eq!(
            status(&f, &format!("/media/{}", artwork.file_name)).await,
            StatusCode::NOT_FOUND
        );
        assert!(
            music_artworks::get(&f.state, artwork.id, Some(f.alice.id))
                .await
                .is_ok()
        );
        music_assets::grant(&f.state, f.alice.id, f.asset, f.bob.id, true)
            .await
            .unwrap();
        assert!(
            music_artworks::get(&f.state, artwork.id, Some(f.bob.id))
                .await
                .is_ok()
        );
        music_assets::grant(&f.state, f.alice.id, f.asset, f.bob.id, false)
            .await
            .unwrap();
        assert!(
            music_artworks::get(&f.state, artwork.id, Some(f.bob.id))
                .await
                .is_err()
        );
    }
    public(&f).await;
    let artwork = music_artworks::for_target(&f.state, Target::Albums, f.album, None)
        .await
        .unwrap()
        .remove(0);
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/music/artwork/{}", artwork.id))
                .header("Range", "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
    plamenu_db::block::create(&f.state.pool, f.alice.id, f.bob.id, None)
        .await
        .unwrap();
    assert!(
        music_artworks::get(&f.state, artwork.id, Some(f.bob.id))
            .await
            .is_err()
    );
    let track = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.alice.id,
        f.asset,
        &music_assets::Edit {
            title: track.title.clone(),
            music: track.metadata(),
            visibility: "private".into(),
            revision: Some(track.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        status(&f, &format!("/music/artwork/{}", artwork.id)).await,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn artwork_replacement_cascades_and_account_retirement_keep_storage_debt(pool: PgPool) {
    let f = fixture(pool).await;
    let old = upload(&f, Target::Assets, f.asset, "image").await;
    let current = upload(&f, Target::Assets, f.asset, "image").await;
    assert!(
        music_artworks::get(&f.state, old.id, Some(f.alice.id))
            .await
            .is_err()
    );
    let debt: i64 =
        sqlx::query_scalar("SELECT size_bytes FROM music_storage_debt WHERE file_name=$1")
            .bind(&old.file_name)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, old.size_bytes);
    // A direct FK cascade must retire the file, not merely erase its pointer.
    sqlx::query("DELETE FROM music_assets WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_artworks::get(&f.state, current.id, Some(f.alice.id))
            .await
            .is_err()
    );
    let job: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_cleanup_jobs WHERE file_name=$1)")
            .bind(&current.file_name)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert!(job);
    let artist = upload(&f, Target::Artists, f.artist, "image").await;
    let collection = upload(&f, Target::Collections, f.collection, "cover").await;
    plamenu_db::account::delete_by_id(&f.state.pool, f.alice.id)
        .await
        .unwrap();
    for artwork in [artist, collection] {
        let debt: (Option<i64>, i64) =
            sqlx::query_as("SELECT owner_id,size_bytes FROM music_storage_debt WHERE file_name=$1")
                .bind(&artwork.file_name)
                .fetch_one(&f.state.pool)
                .await
                .unwrap();
        assert_eq!(debt, (None, artwork.size_bytes));
        assert_eq!(
            status(&f, &format!("/music/artwork/{}", artwork.id)).await,
            StatusCode::NOT_FOUND
        );
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn artwork_audio_and_resource_updates_commit_together_and_failures_keep_private_cleanup(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    public(&f).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let follower = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&f.state.pool, follower.id, f.alice.id, None)
        .await
        .unwrap();
    let published = music_publications::publish(&f.state, &f.alice, f.asset)
        .await
        .unwrap();
    let artwork = upload(&f, Target::Albums, f.album, "cover").await;
    let expected = format!("https://{TEST_DOMAIN}/music/artwork/{}", artwork.id);
    let update:Value=sqlx::query_scalar("SELECT activity->'object' FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'id'=$1 ORDER BY id DESC LIMIT 1").bind(format!("https://{TEST_DOMAIN}/users/alice/statuses/{}",published.id)).fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(update["image"]["url"], expected);
    assert_eq!(update["track"]["album"]["image"]["url"], expected);
    let canonical = music_entities::document(&f.state, Kind::Album, f.album)
        .await
        .unwrap();
    assert_eq!(canonical["image"]["url"], expected);
    let prior = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_artwork_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced artwork failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_artwork_update BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_artwork_update()").execute(&f.state.pool).await.unwrap();
    let (_, revision) =
        music_artworks::authorize(&f.state, Target::Albums, f.album, Some(f.alice.id))
            .await
            .unwrap();
    assert!(
        music_artworks::upload(
            &f.state,
            f.alice.id,
            Target::Albums,
            f.album,
            "cover",
            &revision.to_string(),
            png()
        )
        .await
        .is_err()
    );
    let retained = music_artworks::for_target(&f.state, Target::Albums, f.album, Some(f.alice.id))
        .await
        .unwrap();
    assert_eq!(retained[0].id, artwork.id);
    assert_eq!(
        music_assets::get(&f.state, f.asset, Some(f.alice.id))
            .await
            .unwrap()
            .revision,
        prior.revision
    );
    let pending: Vec<String> = sqlx::query_scalar(
        "SELECT file_name FROM media_cleanup_jobs WHERE file_name LIKE 'musicart%'",
    )
    .fetch_all(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(f.state.media.get(&pending[0]).await.is_ok());
    assert_eq!(
        status(&f, &format!("/media/{}", pending[0])).await,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn invalid_artwork_foreign_edits_and_stale_revisions_do_not_store_files(pool: PgPool) {
    let f = fixture(pool).await;
    assert!(
        music_artworks::upload(
            &f.state,
            f.alice.id,
            Target::Assets,
            f.asset,
            "image",
            "1",
            b"<svg><script>alert(1)</script></svg>".to_vec()
        )
        .await
        .is_err()
    );
    assert!(
        music_artworks::upload(
            &f.state,
            f.bob.id,
            Target::Collections,
            f.collection,
            "cover",
            "1",
            png()
        )
        .await
        .is_err()
    );
    assert!(
        music_artworks::upload(
            &f.state,
            f.alice.id,
            Target::Assets,
            f.asset,
            "image",
            "999",
            png()
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_artworks")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn track_artwork_falls_back_to_album_then_artist_without_bypassing_source_access(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let artist_image = upload(&f, Target::Artists, f.artist, "image").await;
    let private = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    assert_eq!(private.artwork_id, Some(artist_image.id));
    assert_eq!(
        private.entity()["image"]["url"],
        format!("/music/artwork/{}", artist_image.id)
    );
    assert!(private.entity()["image"].get("file_name").is_none());
    assert!(music_assets::get(&f.state, f.asset, None).await.is_err());
    public(&f).await;
    let publication = music_publications::publish(&f.state, &f.alice, f.asset)
        .await
        .unwrap();
    let doc = plamenu::note::note_for_status(&f.state, &publication, &f.alice)
        .await
        .unwrap();
    assert_eq!(
        doc["image"]["url"],
        format!("https://{TEST_DOMAIN}/music/artwork/{}", artist_image.id)
    );
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/music/assets/{}", f.asset))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains(&format!("src=\"/music/artwork/{}\"", artist_image.id)));
    let album_cover = upload(&f, Target::Albums, f.album, "cover").await;
    assert_eq!(
        music_assets::get(&f.state, f.asset, None)
            .await
            .unwrap()
            .artwork_id,
        Some(album_cover.id)
    );
    let own = upload(&f, Target::Assets, f.asset, "image").await;
    assert_eq!(
        music_assets::get(&f.state, f.asset, None)
            .await
            .unwrap()
            .artwork_id,
        Some(own.id)
    );
    let revision = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap()
        .revision;
    music_artworks::remove(
        &f.state,
        f.alice.id,
        Target::Assets,
        f.asset,
        "image",
        &revision.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        music_assets::get(&f.state, f.asset, None)
            .await
            .unwrap()
            .artwork_id,
        Some(album_cover.id)
    );
    let (_, revision) =
        music_artworks::authorize(&f.state, Target::Albums, f.album, Some(f.alice.id))
            .await
            .unwrap();
    music_artworks::remove(
        &f.state,
        f.alice.id,
        Target::Albums,
        f.album,
        "cover",
        &revision.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        music_assets::get(&f.state, f.asset, None)
            .await
            .unwrap()
            .artwork_id,
        Some(artist_image.id)
    );
    let track = music_assets::get(&f.state, f.asset, Some(f.alice.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.alice.id,
        f.asset,
        &music_assets::Edit {
            title: track.title.clone(),
            visibility: "private".into(),
            music: track.metadata(),
            revision: Some(track.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        status(&f, &format!("/music/artwork/{}", artist_image.id)).await,
        StatusCode::NOT_FOUND
    );
    assert!(music_assets::get(&f.state, f.asset, None).await.is_err());
    music_assets::grant(&f.state, f.alice.id, f.asset, f.bob.id, true)
        .await
        .unwrap();
    assert_eq!(
        music_assets::get(&f.state, f.asset, Some(f.bob.id))
            .await
            .unwrap()
            .artwork_id,
        Some(artist_image.id)
    );
    assert!(
        music_artworks::get(&f.state, artist_image.id, Some(f.bob.id))
            .await
            .is_ok()
    );
    music_assets::grant(&f.state, f.alice.id, f.asset, f.bob.id, false)
        .await
        .unwrap();
    assert!(
        music_artworks::get(&f.state, artist_image.id, Some(f.bob.id))
            .await
            .is_err()
    );
}
