mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState, music_assets, music_collections, music_favourites, music_publications,
    music_queues::Source,
    music_radios,
    music_saved::{self, Kind, Reference},
};
use plamenu_db::{PgPool, account::Account, id};
struct Fixture {
    state: AppState,
    owner: Account,
    viewer: Account,
    asset: i64,
    artist: i64,
    album: i64,
    collection: i64,
    radio: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let viewer = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Album')")
        .bind(album)
        .bind(owner.id)
        .bind(artist)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Original title',$3,$4,'public',$5,$6,100,100,$7,10,'mp3')")
        .bind(asset).bind(owner.id).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    let collection = music_collections::create(
        &state,
        owner.id,
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
    let radio = music_radios::create(
        &state,
        owner.id,
        &music_radios::New {
            name: "Radio".into(),
            description: String::new(),
            config: "private config".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap()
    .id;
    Fixture {
        state,
        owner,
        viewer,
        asset,
        artist,
        album,
        collection,
        radio,
    }
}
fn reference(kind: Kind, id: i64) -> Reference {
    Reference {
        kind,
        id: id.to_string(),
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn typed_playlists_are_saveable_but_notes_and_boost_wrappers_are_not_music_sources(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let playlist = music_collections::create(
        &f.state,
        f.owner.id,
        &music_collections::New {
            name: "Playlist".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let saved = music_saved::save(
        &f.state,
        f.viewer.id,
        &reference(Kind::Playlist, playlist.id),
    )
    .await
    .unwrap();
    assert_eq!(saved.reference.kind, Kind::Playlist);
    let note = plamenu::actions::post_status(
        &f.state,
        plamenu::actions::PostParams {
            username: &f.owner.username,
            text: "Ordinary note",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0;
    assert!(
        music_saved::save(&f.state, f.viewer.id, &reference(Kind::Audio, note.id))
            .await
            .is_err()
    );
    let publication = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let boost = plamenu_db::status::create_local_reblog(&f.state.pool, f.viewer.id, publication.id)
        .await
        .unwrap();
    assert!(
        music_saved::save(&f.state, f.viewer.id, &reference(Kind::Audio, boost.id))
            .await
            .is_err()
    );
    music_collections::delete(&f.state, f.owner.id, playlist.id, playlist.revision)
        .await
        .unwrap();
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn entity_previews_and_legacy_publication_references_follow_current_access_and_identity(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let artwork = id::next();
    sqlx::query("INSERT INTO music_artworks(id,owner_id,artist_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'image',$4,'image/png',100,128,128,$5)")
        .bind(artwork).bind(f.owner.id).bind(f.artist).bind(format!("musicart{artwork}.png"))
        .bind(id::next()).execute(&f.state.pool).await.unwrap();
    music_saved::save(&f.state, f.viewer.id, &reference(Kind::Artist, f.artist))
        .await
        .unwrap();
    let page = music_saved::page(&f.state, f.viewer.id, None)
        .await
        .unwrap();
    assert_eq!(
        page.items[0].source.as_ref().unwrap()["image"]["id"],
        artwork.to_string()
    );
    plamenu_db::block::create(&f.state.pool, f.viewer.id, f.owner.id, None)
        .await
        .unwrap();
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items[0]
            .source
            .is_none()
    );
    plamenu_db::block::delete(&f.state.pool, f.viewer.id, f.owner.id)
        .await
        .unwrap();
    let publication = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let old = id::next();
    sqlx::query(
        "INSERT INTO music_saved_resources(id,account_id,kind,status_id) VALUES($1,$2,'audio',$3)",
    )
    .bind(old)
    .bind(f.viewer.id)
    .bind(publication.id)
    .execute(&f.state.pool)
    .await
    .unwrap();
    assert!(
        music_saved::contains(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
            .await
            .unwrap()
    );
    music_saved::remove_reference(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_saved_resources WHERE id=$1")
            .bind(old)
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        music_assets::get(&f.state, f.asset, Some(f.owner.id))
            .await
            .is_ok()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn exact_saves_are_concurrently_idempotent_and_do_not_create_social_activity(pool: PgPool) {
    let f = fixture(pool).await;
    let target = reference(Kind::Track, f.asset);
    let (first, second) = tokio::join!(
        music_saved::save(&f.state, f.viewer.id, &target),
        music_saved::save(&f.state, f.viewer.id, &target)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.id, second.id);
    for (kind, id) in [
        (Kind::Artist, f.artist),
        (Kind::Album, f.album),
        (Kind::Library, f.collection),
        (Kind::Radio, f.radio),
    ] {
        let target = reference(kind, id);
        let saved = music_saved::save(&f.state, f.viewer.id, &target)
            .await
            .unwrap();
        assert_eq!(
            music_saved::save(&f.state, f.viewer.id, &target)
                .await
                .unwrap()
                .id,
            saved.id
        );
        if kind == Kind::Radio {
            assert!(saved.source.unwrap().get("config").is_none());
        }
    }
    assert!(
        music_saved::save(
            &f.state,
            f.viewer.id,
            &reference(Kind::Playlist, f.collection)
        )
        .await
        .is_err()
    );
    assert_eq!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .len(),
        5
    );
    assert!(
        music_saved::page(&f.state, f.owner.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for query in [
        "SELECT count(*) FROM delivery_jobs",
        "SELECT count(*) FROM music_follows",
        "SELECT count(*) FROM music_favourites",
        "SELECT count(*) FROM favourites",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&f.state.pool)
                .await
                .unwrap(),
            0
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn withdrawn_local_audio_keeps_normalized_saved_source_and_independent_favourite(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let publication = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let saved = music_saved::save(
        &f.state,
        f.viewer.id,
        &reference(Kind::Audio, publication.id),
    )
    .await
    .unwrap();
    assert_eq!(saved.reference.kind, Kind::Track);
    assert_eq!(saved.reference.id, f.asset.to_string());
    assert_eq!(
        music_saved::save(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
            .await
            .unwrap()
            .id,
        saved.id
    );
    music_publications::withdraw(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items[0]
            .source
            .is_some()
    );
    let source = Source {
        remote_id: None,
        episode_id: None,
        asset_id: Some(f.asset.to_string()),
        status_id: None,
    };
    music_favourites::save(&f.state, f.viewer.id, &source)
        .await
        .unwrap();
    music_favourites::remove(&f.state, f.viewer.id, &source)
        .await
        .unwrap();
    assert_eq!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    music_favourites::save(&f.state, f.viewer.id, &source)
        .await
        .unwrap();
    music_saved::remove(&f.state, f.viewer.id, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        music_favourites::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn manager_reads_live_metadata_and_can_remove_unavailable_references_without_access(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let saved = music_saved::save(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
        .await
        .unwrap();
    let source = music_assets::get(&f.state, f.asset, Some(f.owner.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.owner.id,
        f.asset,
        &music_assets::Edit {
            title: "Revised title".into(),
            visibility: "private".into(),
            music: source.metadata(),
            revision: Some(source.revision.to_string()),
        },
    )
    .await
    .unwrap();
    let hidden = music_saved::page(&f.state, f.viewer.id, None)
        .await
        .unwrap();
    assert_eq!(hidden.items.len(), 1);
    assert!(hidden.items[0].source.is_none());
    music_assets::grant(&f.state, f.owner.id, f.asset, f.viewer.id, true)
        .await
        .unwrap();
    assert_eq!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items[0]
            .source
            .as_ref()
            .unwrap()["title"],
        "Revised title"
    );
    music_assets::grant(&f.state, f.owner.id, f.asset, f.viewer.id, false)
        .await
        .unwrap();
    music_saved::remove(&f.state, f.owner.id, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    music_saved::remove(&f.state, f.viewer.id, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_saved_insert_rolls_back_and_purge_without_deleted_marker_erases_personal_refs(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    sqlx::query("CREATE FUNCTION reject_saved_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced saved failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_saved_insert BEFORE INSERT ON music_saved_resources FOR EACH ROW EXECUTE FUNCTION reject_saved_insert()")
        .execute(&f.state.pool).await.unwrap();
    let target = reference(Kind::Track, f.asset);
    assert!(
        music_saved::save(&f.state, f.viewer.id, &target)
            .await
            .is_err()
    );
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    sqlx::query("DROP TRIGGER reject_saved_insert ON music_saved_resources")
        .execute(&f.state.pool)
        .await
        .unwrap();
    music_saved::save(&f.state, f.viewer.id, &target)
        .await
        .unwrap();
    let mut tx = f.state.pool.begin().await.unwrap();
    plamenu_db::account::purge_local_data_conn(&mut tx, f.viewer.id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !plamenu_db::account::is_deleted(&f.state.pool, f.viewer.id)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_saved_resources WHERE account_id=$1"
        )
        .bind(f.viewer.id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap(),
        0
    );
    assert!(
        music_assets::get(&f.state, f.asset, Some(f.owner.id))
            .await
            .is_ok()
    );
}
