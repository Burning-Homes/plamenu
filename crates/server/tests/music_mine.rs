mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState, music_assets,
    music_entities::{self, Kind as EntityKind},
    music_favourites,
    music_mine::{self, Filter, Request},
    music_publications,
    music_queues::Source,
    music_saved::{self, Kind, Reference},
};
use plamenu_db::{PgPool, account::Account, id};
struct Fixture {
    state: AppState,
    owner: Account,
    viewer: Account,
    artist: i64,
    album: i64,
    asset: i64,
}
async fn asset(
    state: &AppState,
    owner: i64,
    artist: i64,
    album: Option<i64>,
    visibility: &str,
) -> i64 {
    let asset = id::next();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Searchable title',$3,$4,$5,$6,$7,100,100,$8,10,'mp3')")
        .bind(asset).bind(owner).bind(artist).bind(album).bind(visibility).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    asset
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let viewer = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
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
    let asset = asset(&state, owner.id, artist, Some(album), "public").await;
    Fixture {
        state,
        owner,
        viewer,
        artist,
        album,
        asset,
    }
}
fn reference(kind: Kind, id: i64) -> Reference {
    Reference {
        kind,
        id: id.to_string(),
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn owned_saved_and_favourite_membership_fold_only_current_publication_aliases(pool: PgPool) {
    let f = fixture(pool).await;
    let publication = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let saved = music_saved::save(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
        .await
        .unwrap();
    let favourite = Source {
        remote_id: None,
        episode_id: None,
        asset_id: None,
        status_id: Some(publication.id.to_string()),
    };
    music_favourites::save(&f.state, f.viewer.id, &favourite)
        .await
        .unwrap();
    let input = Request {
        kind: Filter::Tracks,
        ..Default::default()
    };
    let viewer = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(viewer.items.len(), 1);
    let track = &viewer.items[0];
    assert_eq!(track.reference.kind, Kind::Track);
    assert_eq!(track.reference.id, f.asset.to_string());
    assert!(!track.membership.owned);
    assert!(track.membership.saved);
    assert!(track.membership.favourite);
    let owner = music_mine::page(&f.state, f.owner.id, &input)
        .await
        .unwrap();
    assert_eq!(owner.items.len(), 1);
    assert!(owner.items[0].membership.owned);
    music_saved::remove(&f.state, f.viewer.id, saved.id.parse().unwrap())
        .await
        .unwrap();
    let viewer = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(viewer.items.len(), 1);
    assert!(!viewer.items[0].membership.saved);
    assert!(viewer.items[0].membership.favourite);
    music_publications::withdraw(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    assert!(
        music_mine::page(&f.state, f.viewer.id, &input)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        music_mine::page(&f.state, f.owner.id, &input)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn unavailable_saved_track_does_not_collect_artist_via_an_unrelated_public_track(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    music_saved::save(&f.state, f.viewer.id, &reference(Kind::Track, f.asset))
        .await
        .unwrap();
    asset(&f.state, f.owner.id, f.artist, None, "public").await;
    let source = music_assets::get(&f.state, f.asset, Some(f.owner.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.owner.id,
        f.asset,
        &music_assets::Edit {
            title: source.title.clone(),
            visibility: "private".into(),
            music: source.metadata(),
            revision: Some(source.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert!(
        music_entities::get(&f.state, EntityKind::Artist, f.artist, Some(f.viewer.id))
            .await
            .is_ok()
    );
    assert!(
        music_mine::page(
            &f.state,
            f.viewer.id,
            &Request {
                kind: Filter::Artists,
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    assert!(
        music_mine::page(
            &f.state,
            f.viewer.id,
            &Request {
                kind: Filter::Tracks,
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    assert!(
        music_saved::page(&f.state, f.viewer.id, None)
            .await
            .unwrap()
            .items[0]
            .source
            .is_none()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collected_album_includes_its_artist_and_own_empty_entities_remain_owned(pool: PgPool) {
    let f = fixture(pool).await;
    music_saved::save(&f.state, f.viewer.id, &reference(Kind::Album, f.album))
        .await
        .unwrap();
    let input = Request {
        kind: Filter::Artists,
        ..Default::default()
    };
    let viewer = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(viewer.items.len(), 1);
    assert_eq!(viewer.items[0].reference.id, f.artist.to_string());
    assert!(viewer.items[0].membership.implicit);
    assert!(!viewer.items[0].membership.saved);
    let empty = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Empty owned artist')")
        .bind(empty)
        .bind(f.viewer.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let viewer = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(viewer.items.len(), 2);
    assert!(
        viewer
            .items
            .iter()
            .any(|item| item.reference.id == empty.to_string() && item.membership.owned)
    );
    plamenu_db::block::create(&f.state.pool, f.viewer.id, f.owner.id, None)
        .await
        .unwrap();
    let viewer = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(viewer.items.len(), 1);
    assert_eq!(viewer.items[0].reference.id, empty.to_string());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn paging_and_search_apply_current_source_access_before_visible_cursors(pool: PgPool) {
    let f = fixture(pool).await;
    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    for _ in 0..51 {
        visible.push(asset(&f.state, f.owner.id, f.artist, Some(f.album), "public").await);
    }
    for _ in 0..105 {
        hidden.push(asset(&f.state, f.owner.id, f.artist, Some(f.album), "private").await);
    }
    let mut all = visible.clone();
    all.extend(&hidden);
    let rows = all.iter().map(|_| id::next()).collect::<Vec<_>>();
    // These references were readable when saved; a later revoke hides the first
    // hundred candidates. Current visibility, not stored metadata, determines pages.
    sqlx::query("INSERT INTO music_saved_resources(id,account_id,kind,asset_id) SELECT saved_id,$3,'track',asset_id FROM unnest($1::bigint[],$2::bigint[]) s(saved_id,asset_id)")
        .bind(rows).bind(&all).bind(f.viewer.id).execute(&f.state.pool).await.unwrap();
    let input = Request {
        kind: Filter::Tracks,
        q: "searchable".into(),
        ..Default::default()
    };
    let first = music_mine::page(&f.state, f.viewer.id, &input)
        .await
        .unwrap();
    assert_eq!(first.items.len(), 50);
    assert!(
        first
            .items
            .iter()
            .all(|item| visible.contains(&item.reference.id.parse::<i64>().unwrap()))
    );
    let second = music_mine::page(
        &f.state,
        f.viewer.id,
        &Request {
            before: first.before.clone(),
            ..input
        },
    )
    .await
    .unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.before.is_none());
    assert!(
        !first
            .items
            .iter()
            .any(|item| item.reference.id == second.items[0].reference.id)
    );
    assert!(
        music_mine::page(
            &f.state,
            f.viewer.id,
            &Request {
                before: Some("invalid:track".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
}
