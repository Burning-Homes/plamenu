mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    music_assets, music_collections,
    music_entities::{self, Kind},
    music_radios,
};
use plamenu_db::{PgPool, id};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};
struct Counter(Arc<AtomicUsize>);
impl<S: tracing::Subscriber> Layer<S> for Counter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target().starts_with("sqlx::query") {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn entity_batches_preserve_identity_current_source_access_and_flat_query_count(pool: PgPool) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let mut artists = Vec::new();
    let mut albums = Vec::new();
    let mut tracks = Vec::new();
    for index in 0..41 {
        let creator = if index == 0 {
            owner
        } else {
            create_local_account(&state.pool, &format!("creator_{index}"), "Creator")
                .await
                .id
        };
        let artist = id::next();
        let album = id::next();
        let track = id::next();
        sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Same artist name')")
            .bind(artist)
            .bind(creator)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Same album name')")
            .bind(album).bind(creator).bind(artist).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Track',$3,$4,$5,$6,100,100,$7,10,'mp3')")
            .bind(track).bind(creator).bind(artist).bind(album).bind(format!("music{track}.orig")).bind(format!("music{track}.mp3")).bind(format!("{track:064x}")).execute(&state.pool).await.unwrap();
        artists.push(artist);
        albums.push(album);
        tracks.push(track);
    }
    for (kind, ids) in [(Kind::Artist, &artists), (Kind::Album, &albums)] {
        assert!(
            music_entities::for_ids(&state, kind, ids, Some(viewer))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            music_entities::for_ids(&state, kind, ids, Some(owner))
                .await
                .unwrap()
                .len(),
            1
        );
    }
    sqlx::query(
        "INSERT INTO music_asset_grants(asset_id,account_id) SELECT unnest($1::bigint[]),$2",
    )
    .bind(&tracks)
    .bind(viewer)
    .execute(&state.pool)
    .await
    .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Counter(count.clone())),
    );
    for (kind, ids) in [(Kind::Artist, &artists), (Kind::Album, &albums)] {
        count.store(0, Ordering::Relaxed);
        let one = music_entities::for_ids(&state, kind, &ids[..1], Some(viewer))
            .await
            .unwrap();
        let single = count.load(Ordering::Relaxed);
        assert_eq!(one.len(), 1);
        assert!(single > 0);
        count.store(0, Ordering::Relaxed);
        let rows = music_entities::for_ids(&state, kind, ids, Some(viewer))
            .await
            .unwrap();
        let many = count.load(Ordering::Relaxed);
        assert_eq!(rows.len(), 41);
        assert_eq!(
            single, many,
            "Entity reads must not issue a query per resource"
        );
        assert!(rows.iter().all(|row| ids.contains(&row.id)));
        assert_eq!(
            music_entities::for_ids(&state, kind, &[ids[0], ids[0]], Some(viewer))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            music_entities::for_ids(&state, kind, &vec![ids[0]; 101], Some(viewer))
                .await
                .is_err()
        );
    }
    music_assets::grant(&state, owner, tracks[0], viewer, false)
        .await
        .unwrap();
    assert_eq!(
        music_entities::for_ids(&state, Kind::Album, &albums, Some(viewer))
            .await
            .unwrap()
            .len(),
        40
    );
    let second_owner = music_assets::get(&state, tracks[1], Some(viewer))
        .await
        .unwrap()
        .owner_id;
    plamenu_db::block::create(&state.pool, viewer, second_owner, None)
        .await
        .unwrap();
    for (kind, ids) in [(Kind::Artist, &artists), (Kind::Album, &albums)] {
        assert_eq!(
            music_entities::for_ids(&state, kind, ids, Some(viewer))
                .await
                .unwrap()
                .len(),
            39
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn container_batches_recheck_grants_blocks_retirement_and_station_config_privacy(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let mut collections = Vec::new();
    let mut radios = Vec::new();
    for visibility in ["private", "local", "public"] {
        collections.push(
            music_collections::create(
                &state,
                owner,
                &music_collections::New {
                    name: "Same collection".into(),
                    description: String::new(),
                    kind: "playlist".into(),
                    visibility: visibility.into(),
                },
            )
            .await
            .unwrap(),
        );
        radios.push(
            music_radios::create(
                &state,
                owner,
                &music_radios::New {
                    name: "Same station".into(),
                    description: String::new(),
                    config: "owner-only settings".into(),
                    visibility: visibility.into(),
                },
            )
            .await
            .unwrap(),
        );
    }
    let ids = collections.iter().map(|row| row.id).collect::<Vec<_>>();
    let stations = radios.iter().map(|row| row.id).collect::<Vec<_>>();
    assert_eq!(
        music_collections::for_ids(&state, &ids, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        music_radios::for_ids(&state, &stations, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        music_collections::for_ids(&state, &ids, Some(viewer))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        music_radios::for_ids(&state, &stations, Some(viewer))
            .await
            .unwrap()
            .len(),
        2
    );
    music_collections::grant(&state, owner, ids[0], collections[0].revision, viewer, true)
        .await
        .unwrap();
    music_radios::grant(&state, owner, stations[0], viewer, true)
        .await
        .unwrap();
    assert_eq!(
        music_collections::for_ids(&state, &ids, Some(viewer))
            .await
            .unwrap()
            .len(),
        3
    );
    let visible = music_radios::for_ids(&state, &stations, Some(viewer))
        .await
        .unwrap();
    assert_eq!(visible.len(), 3);
    assert!(
        visible
            .iter()
            .all(|row| row.entity(Some(viewer)).get("config").is_none())
    );
    assert!(
        music_collections::for_ids(&state, &vec![ids[0]; 101], Some(viewer))
            .await
            .is_err()
    );
    assert!(
        music_radios::for_ids(&state, &vec![stations[0]; 101], Some(viewer))
            .await
            .is_err()
    );
    let current = music_collections::get(&state, ids[0], Some(owner))
        .await
        .unwrap();
    music_collections::grant(&state, owner, ids[0], current.revision, viewer, false)
        .await
        .unwrap();
    music_radios::grant(&state, owner, stations[0], viewer, false)
        .await
        .unwrap();
    assert_eq!(
        music_collections::for_ids(&state, &ids, Some(viewer))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        music_radios::for_ids(&state, &stations, Some(viewer))
            .await
            .unwrap()
            .len(),
        2
    );
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_collections::for_ids(&state, &ids, Some(viewer))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        music_radios::for_ids(&state, &stations, Some(viewer))
            .await
            .unwrap()
            .is_empty()
    );
    plamenu_db::block::delete(&state.pool, viewer, owner)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_collections::for_ids(&state, &ids, Some(viewer))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        music_radios::for_ids(&state, &stations, Some(viewer))
            .await
            .unwrap()
            .is_empty()
    );
}
