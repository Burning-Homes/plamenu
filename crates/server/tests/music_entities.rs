mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{
    AppState,
    actions::{self, PostParams},
    entities, music_assets,
    music_entities::{self, Kind},
    music_publications,
};
use plamenu_db::{PgPool, account::Account, id};
use serde_json::Value;
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    owner: Account,
    bob: Account,
    artist: i64,
    album: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name,description) VALUES($1,$2,'Shared artist','Artist biography')").bind(artist).bind(owner.id).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,release_year,description) VALUES($1,$2,$3,'Shared album',2026,'Album description')").bind(album).bind(owner.id).bind(artist).execute(&state.pool).await.unwrap();
    Fixture {
        state,
        owner,
        bob,
        artist,
        album,
    }
}
async fn asset(f: &Fixture, artist: i64, album: Option<i64>, visibility: &str) -> i64 {
    let asset = id::next();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year) VALUES($1,$2,'A retained track',$3,$4,$5,$6,$7,100,100,$8,10,'mp3',2026)")
        .bind(asset).bind(f.owner.id).bind(artist).bind(album).bind(visibility).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&f.state.pool).await.unwrap();
    asset
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn aggregate_duration_and_empty_owned_albums_follow_current_source_access(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::query(
        "INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Empty owned album')",
    )
    .bind(id::next())
    .bind(f.owner.id)
    .bind(f.artist)
    .execute(&f.state.pool)
    .await
    .unwrap();
    let public = asset(&f, f.artist, Some(f.album), "public").await;
    let private = asset(&f, f.artist, Some(f.album), "private").await;
    for (viewer, tracks, albums, duration) in [
        (Some(f.owner.id), 2, 2, 20.0),
        (None, 1, 1, 10.0),
        (Some(f.bob.id), 1, 1, 10.0),
    ] {
        let stats = music_entities::statistics(&f.state, Kind::Artist, f.artist, viewer)
            .await
            .unwrap();
        assert_eq!(stats.track_count, tracks);
        assert_eq!(stats.album_count, albums);
        assert!((stats.total_duration - duration).abs() < f64::EPSILON);
    }
    music_assets::grant(&f.state, f.owner.id, private, f.bob.id, true)
        .await
        .unwrap();
    let stats = music_entities::statistics(&f.state, Kind::Album, f.album, Some(f.bob.id))
        .await
        .unwrap();
    assert_eq!(stats.track_count, 2);
    assert!((stats.total_duration - 20.0).abs() < f64::EPSILON);
    plamenu_db::block::create(&f.state.pool, f.bob.id, f.owner.id, None)
        .await
        .unwrap();
    assert!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, Some(f.bob.id))
            .await
            .is_err()
    );
    plamenu_db::block::delete(&f.state.pool, f.bob.id, f.owner.id)
        .await
        .unwrap();
    music_assets::grant(&f.state, f.owner.id, private, f.bob.id, false)
        .await
        .unwrap();
    let stats = music_entities::statistics(&f.state, Kind::Album, f.album, Some(f.bob.id))
        .await
        .unwrap();
    assert_eq!(stats.track_count, 1);
    assert!((stats.total_duration - 10.0).abs() < f64::EPSILON);
    music_assets::delete(&f.state, f.owner.id, public)
        .await
        .unwrap();
    assert!(
        music_entities::statistics(&f.state, Kind::Album, f.album, Some(f.bob.id))
            .await
            .is_err()
    );
    music_assets::delete(&f.state, f.owner.id, private)
        .await
        .unwrap();
    let stats = music_entities::statistics(&f.state, Kind::Artist, f.artist, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(stats.track_count, 0);
    assert_eq!(stats.album_count, 2);
    assert!(stats.total_duration.abs() < f64::EPSILON);
    assert!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, None)
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn two_publications_share_reusable_entities_and_withdrawing_one_preserves_them(pool: PgPool) {
    let f = fixture(pool).await;
    let first_asset = asset(&f, f.artist, Some(f.album), "public").await;
    let second_asset = asset(&f, f.artist, Some(f.album), "public").await;
    let first = music_publications::publish(&f.state, &f.owner, first_asset)
        .await
        .unwrap();
    let second = music_publications::publish(&f.state, &f.owner, second_asset)
        .await
        .unwrap();
    let first_doc = plamenu::note::note_for_status(&f.state, &first, &f.owner)
        .await
        .unwrap();
    let second_doc = plamenu::note::note_for_status(&f.state, &second, &f.owner)
        .await
        .unwrap();
    let artist = music_entities::document(&f.state, Kind::Artist, f.artist)
        .await
        .unwrap();
    let album = music_entities::document(&f.state, Kind::Album, f.album)
        .await
        .unwrap();
    assert_eq!(first_doc["track"]["artists"][0]["id"], artist["id"]);
    assert_eq!(second_doc["track"]["artists"][0]["id"], artist["id"]);
    assert_eq!(first_doc["track"]["album"]["id"], album["id"]);
    assert_eq!(second_doc["track"]["album"]["id"], album["id"]);
    assert_eq!(
        artist["attributedTo"],
        entities::account_uri(TEST_DOMAIN, &f.owner)
    );
    assert_eq!(album["released"], "2026-01-01");
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, None)
            .await
            .unwrap()
            .track_count,
        2
    );
    music_publications::withdraw(&f.state, &f.owner, first_asset)
        .await
        .unwrap();
    assert_eq!(
        music_entities::document(&f.state, Kind::Artist, f.artist)
            .await
            .unwrap()["id"],
        artist["id"]
    );
    assert_eq!(
        music_entities::document(&f.state, Kind::Album, f.album)
            .await
            .unwrap()["id"],
        album["id"]
    );
    let again = music_publications::publish(&f.state, &f.owner, first_asset)
        .await
        .unwrap();
    assert_ne!(again.id, first.id);
    assert_eq!(
        plamenu::note::note_for_status(&f.state, &again, &f.owner)
            .await
            .unwrap()["track"]["artists"][0]["id"],
        artist["id"]
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn private_only_entity_metadata_and_statistics_follow_source_grants_and_blocks(pool: PgPool) {
    let f = fixture(pool).await;
    let private = asset(&f, f.artist, Some(f.album), "private").await;
    for kind in [Kind::Artist, Kind::Album] {
        let id = if kind == Kind::Artist {
            f.artist
        } else {
            f.album
        };
        assert!(music_entities::get(&f.state, kind, id, None).await.is_err());
        assert!(music_entities::document(&f.state, kind, id).await.is_err());
        assert!(
            music_entities::get(&f.state, kind, id, Some(f.bob.id))
                .await
                .is_err()
        );
        assert!(
            music_entities::get(&f.state, kind, id, Some(f.owner.id))
                .await
                .is_ok()
        );
    }
    music_assets::grant(&f.state, f.owner.id, private, f.bob.id, true)
        .await
        .unwrap();
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, Some(f.bob.id))
            .await
            .unwrap()
            .track_count,
        1
    );
    assert_eq!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(f.bob.id))
            .await
            .unwrap()
            .description,
        "Album description"
    );
    plamenu_db::block::create(&f.state.pool, f.owner.id, f.bob.id, None)
        .await
        .unwrap();
    assert!(
        music_entities::get(&f.state, Kind::Artist, f.artist, Some(f.bob.id))
            .await
            .is_err()
    );
    assert!(
        music_entities::statistics(&f.state, Kind::Album, f.album, Some(f.bob.id))
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn reusable_entities_cannot_cross_owner_boundaries_or_leak_music_from_reply_stubs(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let foreign = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Shared artist')")
        .bind(foreign)
        .bind(f.bob.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let result = sqlx::query(
        "INSERT INTO music_albums(id,owner_id,artist_id,name) VALUES($1,$2,$3,'Wrong owner')",
    )
    .bind(id::next())
    .bind(f.owner.id)
    .bind(foreign)
    .execute(&f.state.pool)
    .await;
    assert!(
        result.is_err(),
        "ownership is enforced in storage as well as services"
    );
    let source = asset(&f, f.artist, Some(f.album), "public").await;
    let published = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    actions::post_status(
        &f.state,
        PostParams {
            username: &f.owner.username,
            text: "Keep the reply tree",
            visibility: "public",
            in_reply_to_id: Some(published.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    actions::delete_status(&f.state, &f.owner, published.id, actions::DeleteMode::Stub)
        .await
        .unwrap();
    assert!(
        plamenu_db::status::is_deleted(&f.state.pool, published.id)
            .await
            .unwrap()
    );
    assert!(
        plamenu_db::music::for_statuses(&f.state.pool, &[published.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        music_assets::get(&f.state, source, Some(f.owner.id))
            .await
            .is_ok()
    );
    let rendered =
        plamenu::entities::render_status(&f.state.pool, TEST_DOMAIN, &published, Some(f.owner.id))
            .await
            .unwrap();
    assert_eq!(rendered["music"], Value::Null);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn all_music_activitypub_documents_use_the_existing_secure_fetch_gate(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let source = asset(&f, f.artist, Some(f.album), "public").await;
    let post = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let app = common::test_app_secure(pool, stub);
    let paths = [
        format!("/ap/music/artists/{}", f.artist),
        format!("/ap/music/albums/{}", f.album),
        "/users/alice/library".into(),
    ];
    for path in paths {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("accept", "application/activity+json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        let signed = remote.signer().sign_get(
            TEST_DOMAIN,
            &path,
            "application/activity+json",
            std::time::SystemTime::now(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("host", TEST_DOMAIN)
                    .header("accept", "application/activity+json")
                    .header("date", signed.date)
                    .header("signature", signed.signature)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let alias = format!("/users/alice/statuses/{}/artist", post.id);
    let signed = remote.signer().sign_get(
        TEST_DOMAIN,
        &alias,
        "application/activity+json",
        std::time::SystemTime::now(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(&alias)
                .header("host", TEST_DOMAIN)
                .header("accept", "application/activity+json")
                .header("date", signed.date)
                .header("signature", signed.signature)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()["location"],
        music_entities::uri(TEST_DOMAIN, Kind::Artist, f.artist)
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn entity_subscription_lifetime_tracks_public_sources_and_withdrawal_is_atomic(pool: PgPool) {
    let f = fixture(pool).await;
    let first = asset(&f, f.artist, Some(f.album), "public").await;
    let second = asset(&f, f.artist, Some(f.album), "public").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let subscriber = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    let target = music_entities::uri(TEST_DOMAIN, Kind::Artist, f.artist);
    assert!(plamenu::music_follows::incoming(&f.state,&subscriber,&serde_json::json!({"id":"https://songhive.test/follows/shared-artist","type":"Follow","actor":remote.actor.id,"object":target})).await.unwrap());
    let source = music_assets::get(&f.state, first, Some(f.owner.id))
        .await
        .unwrap();
    let edit = music_assets::Edit {
        title: "Secret retained title".into(),
        music: source.metadata(),
        visibility: "private".into(),
        revision: Some(source.revision.to_string()),
    };
    sqlx::query("CREATE FUNCTION reject_entity_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced entity outbox failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_entity_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_entity_delivery()").execute(&f.state.pool).await.unwrap();
    assert!(
        music_assets::update(&f.state, f.owner.id, first, &edit)
            .await
            .is_err()
    );
    assert_eq!(
        music_assets::get(&f.state, first, None)
            .await
            .unwrap()
            .visibility,
        "public"
    );
    assert_eq!(
        plamenu::music_follows::list(&f.state, subscriber.id)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DROP TRIGGER reject_entity_delivery ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    music_assets::update(&f.state, f.owner.id, first, &edit)
        .await
        .unwrap();
    assert!(
        music_entities::document(&f.state, Kind::Artist, f.artist)
            .await
            .is_ok()
    );
    assert_eq!(
        plamenu::music_follows::list(&f.state, subscriber.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let update:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'type'='Artist' LIMIT 1").fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(update["object"]["id"], target);
    let source = music_assets::get(&f.state, second, Some(f.owner.id))
        .await
        .unwrap();
    music_assets::update(
        &f.state,
        f.owner.id,
        second,
        &music_assets::Edit {
            title: "Another secret title".into(),
            music: source.metadata(),
            visibility: "private".into(),
            revision: Some(source.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert!(
        music_entities::document(&f.state, Kind::Artist, f.artist)
            .await
            .is_err()
    );
    assert!(
        plamenu::music_follows::list(&f.state, subscriber.id)
            .await
            .unwrap()
            .is_empty()
    );
    let deleted: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete' LIMIT 1",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(
        deleted["object"],
        serde_json::json!({"id":target,"type":"Tombstone"})
    );
    assert!(!deleted.to_string().contains("secret"));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn ordinary_status_edits_keep_retained_source_and_canonical_music_documents_in_sync(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let follower = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&f.state.pool, follower.id, f.owner.id, None)
        .await
        .unwrap();
    let source = asset(&f, f.artist, Some(f.album), "public").await;
    let published = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    let before = music_assets::get(&f.state, source, Some(f.owner.id))
        .await
        .unwrap();
    let metadata = plamenu_db::music::Metadata {
        artist: "Edited artist".into(),
        album: "Edited album".into(),
        genre: "Ambient".into(),
        release_year: Some(2025),
        ..before.metadata()
    };
    let edited = actions::edit_status(
        &f.state,
        &f.owner,
        published.id,
        actions::EditParams {
            title: Some("Edited retained title"),
            music: Some(metadata.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let current = music_assets::get(&f.state, source, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(current.title, edited.title.as_deref().unwrap());
    assert_eq!(current.metadata(), metadata);
    assert_eq!(current.revision, before.revision + 1);
    assert_eq!(current.original_file, before.original_file);
    assert_eq!(
        plamenu_db::music::for_statuses(&f.state.pool, &[published.id])
            .await
            .unwrap()[&published.id],
        metadata
    );
    let object = plamenu::note::note_for_status(&f.state, &edited, &f.owner)
        .await
        .unwrap();
    assert_eq!(
        object["track"]["artists"][0]["id"],
        music_entities::uri(TEST_DOMAIN, Kind::Artist, current.artist_id)
    );
    assert_eq!(object["track"]["artists"][0]["name"], current.artist);
    assert_eq!(object["track"]["album"]["name"], current.album);
    sqlx::query("CREATE FUNCTION reject_reverse_bridge() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced reverse bridge failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_reverse_bridge BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_reverse_bridge()").execute(&f.state.pool).await.unwrap();
    assert!(
        actions::edit_status(
            &f.state,
            &f.owner,
            published.id,
            actions::EditParams {
                title: Some("Must roll back"),
                music: Some(plamenu_db::music::Metadata {
                    artist: "Rejected artist".into(),
                    ..metadata.clone()
                }),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let rolled_back = music_assets::get(&f.state, source, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(rolled_back.title, current.title);
    assert_eq!(rolled_back.revision, current.revision);
    assert_eq!(rolled_back.artist_id, current.artist_id);
    assert_eq!(
        plamenu_db::music::for_statuses(&f.state.pool, &[published.id])
            .await
            .unwrap()[&published.id],
        metadata
    );
}

fn edit(entity: &music_entities::Entity) -> music_entities::Edit {
    music_entities::Edit {
        name: entity.name.clone(),
        description: entity.description.clone(),
        musicbrainz_id: entity.musicbrainz_id.clone(),
        tags: entity.tags.clone(),
        genres: entity.genres.clone(),
        release_year: entity.release_year,
        revision: entity.revision.to_string(),
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn artist_and_album_edits_propagate_atomically_and_preserve_track_year_overrides(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let first = asset(&f, f.artist, Some(f.album), "public").await;
    let second = asset(&f, f.artist, Some(f.album), "private").await;
    sqlx::query("UPDATE music_assets SET release_year=1999 WHERE id=$1")
        .bind(second)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let published = music_publications::publish(&f.state, &f.owner, first)
        .await
        .unwrap();
    let artist = music_entities::get(&f.state, Kind::Artist, f.artist, Some(f.owner.id))
        .await
        .unwrap();
    let mut input = edit(&artist);
    input.name = "Renamed artist".into();
    input.description = "Updated biography".into();
    input.tags = vec![" ELECTRONIC ".into(), "electronic".into()];
    let current = music_entities::update(&f.state, f.owner.id, Kind::Artist, f.artist, &input)
        .await
        .unwrap();
    assert_eq!(current.tags, vec!["electronic"]);
    assert!(matches!(
        music_entities::update(&f.state, f.owner.id, Kind::Artist, f.artist, &input).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert!(matches!(
        music_entities::update(&f.state, f.bob.id, Kind::Artist, f.artist, &edit(&current)).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    let album = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(
        album.revision, 2,
        "Embedded artist changes invalidate stale album forms"
    );
    let mut input = edit(&album);
    input.name = "New album title".into();
    input.release_year = Some(2025);
    input.genres = vec!["Ambient".into()];
    music_entities::update(&f.state, f.owner.id, Kind::Album, f.album, &input)
        .await
        .unwrap();
    let source = music_assets::get(&f.state, first, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(source.artist, "Renamed artist");
    assert_eq!(source.album, "New album title");
    assert_eq!(source.release_year, Some(2025));
    assert_eq!(source.genre, "ambient");
    assert_eq!(source.revision, 3);
    assert_eq!(
        music_assets::get(&f.state, second, Some(f.owner.id))
            .await
            .unwrap()
            .release_year,
        Some(1999)
    );
    assert_eq!(
        plamenu_db::music::for_statuses(&f.state.pool, &[published.id])
            .await
            .unwrap()[&published.id],
        source.metadata()
    );
    let override_source = music_assets::get(&f.state, second, Some(f.owner.id))
        .await
        .unwrap();
    let override_update = music_assets::update(
        &f.state,
        f.owner.id,
        second,
        &music_assets::Edit {
            title: override_source.title.clone(),
            visibility: override_source.visibility.clone(),
            revision: Some(override_source.revision.to_string()),
            music: plamenu_db::music::Metadata {
                genre: "Updated track genre".into(),
                ..override_source.metadata()
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(override_update.album_id, Some(f.album));
    assert_eq!(override_update.release_year, Some(1999));
    let canonical = music_entities::document(&f.state, Kind::Album, f.album)
        .await
        .unwrap();
    assert_eq!(canonical["name"], source.album);
    assert_eq!(canonical["artists"][0]["name"], source.artist);
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let follower = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&f.state.pool, follower.id, f.owner.id, None)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_metadata_edit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced metadata edit failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_metadata_edit BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_metadata_edit()").execute(&f.state.pool).await.unwrap();
    let current = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap();
    let mut rejected = edit(&current);
    rejected.description = "Description-only change must roll back".into();
    assert!(
        music_entities::update(&f.state, f.owner.id, Kind::Album, f.album, &rejected)
            .await
            .is_err()
    );
    assert_eq!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
            .await
            .unwrap()
            .revision,
        current.revision
    );
    let rolled_back = music_assets::get(&f.state, first, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(rolled_back.metadata(), source.metadata());
    assert_eq!(rolled_back.revision, source.revision);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_entity_pages_and_track_paging_hide_unreadable_sources(pool: PgPool) {
    let f = fixture(pool).await;
    let hidden = asset(&f, f.artist, Some(f.album), "private").await;
    let visible = asset(&f, f.artist, Some(f.album), "public").await;
    sqlx::query("UPDATE music_assets SET title='SECRET TITLE',track_number=1 WHERE id=$1")
        .bind(hidden)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE music_assets SET track_number=2 WHERE id=$1")
        .bind(visible)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let tracks = music_entities::tracks(&f.state, Kind::Album, f.album, None, None)
        .await
        .unwrap();
    assert_eq!(tracks.items.len(), 1);
    assert_eq!(tracks.items[0].id, visible);
    assert!(
        music_entities::tracks(&f.state, Kind::Album, f.album, None, Some(hidden))
            .await
            .is_err()
    );
    let settings = plamenu_db::instance_settings::get(&f.state.pool)
        .await
        .unwrap();
    plamenu_db::instance_settings::save(
        &f.state.pool,
        plamenu_db::instance_settings::SettingsUpdate {
            timeline_preview_local: true,
            ..settings.as_update()
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/music/albums/{}", f.album))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("Shared album"));
    assert!(!body.contains("SECRET TITLE"));
    assert!(!body.contains("entity-name"));
    let items = music_entities::catalogue(&f.state, Kind::Album, None, false, "Shared", None)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    plamenu_db::block::create(&f.state.pool, f.bob.id, f.owner.id, None)
        .await
        .unwrap();
    assert!(
        music_entities::catalogue(&f.state, Kind::Album, Some(f.bob.id), false, "", None)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn api_tokens(f: &Fixture) -> (String, String) {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let password = hash_password("test-password").unwrap();
    let user = user::create(
        &f.state.pool,
        f.owner.id,
        Some("alice@plamenu.test"),
        &password,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        &f.state.pool,
        oauth::NewApp {
            name: "music-entity-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let read = generate_secret();
    let write = generate_secret();
    for (token, scopes) in [(&read, "read:statuses"), (&write, "write:media")] {
        oauth::create_token(
            &f.state.pool,
            &hash_secret(token),
            app.id,
            Some(user.id),
            scopes,
        )
        .await
        .unwrap();
    }
    (read, write)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_entity_api_preserves_scope_boundaries_and_string_revisions(pool: PgPool) {
    let f = fixture(pool).await;
    let source = asset(&f, f.artist, Some(f.album), "private").await;
    let (read, write) = api_tokens(&f).await;
    let app = plamenu::build_router(f.state.clone());
    let target = format!("/api/v1/music/artists/{}", f.artist);
    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(&write), StatusCode::FORBIDDEN),
        (Some(&read), StatusCode::OK),
    ] {
        let mut request = Request::builder().uri(&target);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(
            response.headers().get("Cache-Control").unwrap(),
            "private, no-store"
        );
        if expected == StatusCode::OK {
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let document: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(document["id"], f.artist.to_string());
            assert_eq!(document["revision"], "1");
        }
    }
    let body=serde_json::json!({"name":"API artist","description":"Native metadata edit","tags":["ambient"],"revision":"1"}).to_string();
    for (token, expected) in [(&read, StatusCode::FORBIDDEN), (&write, StatusCode::OK)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(&target)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(
        music_assets::get(&f.state, source, Some(f.owner.id))
            .await
            .unwrap()
            .artist,
        "API artist"
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("{target}/tracks"))
                .header("Authorization", format!("Bearer {read}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["items"][0]["id"], source.to_string());
}

fn new_entity(name: &str, artist: Option<i64>) -> music_entities::New {
    music_entities::New {
        name: name.into(),
        description: "Created before importing tracks".into(),
        musicbrainz_id: None,
        tags: vec![" Ambient ".into()],
        genres: Vec::new(),
        release_year: None,
        artist_id: artist.map(|id| id.to_string()),
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn standalone_creation_is_private_owner_scoped_and_does_not_implicitly_merge(pool: PgPool) {
    let f = fixture(pool).await;
    let artist = music_entities::create(
        &f.state,
        f.owner.id,
        Kind::Artist,
        &new_entity("New artist", None),
    )
    .await
    .unwrap();
    assert_eq!(artist.tags, vec!["ambient"]);
    assert!(
        music_entities::get(&f.state, Kind::Artist, artist.id, None)
            .await
            .is_err()
    );
    assert!(
        music_entities::document(&f.state, Kind::Artist, artist.id)
            .await
            .is_err()
    );
    assert!(matches!(
        music_entities::create(
            &f.state,
            f.owner.id,
            Kind::Artist,
            &new_entity("New artist", None)
        )
        .await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    let other = music_entities::create(
        &f.state,
        f.bob.id,
        Kind::Artist,
        &new_entity("New artist", None),
    )
    .await
    .unwrap();
    assert_ne!(artist.id, other.id);
    assert!(matches!(
        music_entities::create(
            &f.state,
            f.owner.id,
            Kind::Album,
            &new_entity("Foreign artist album", Some(other.id))
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    let album = music_entities::create(
        &f.state,
        f.owner.id,
        Kind::Album,
        &new_entity("New album", Some(artist.id)),
    )
    .await
    .unwrap();
    assert_eq!(album.artist_id, Some(artist.id));
    assert!(
        music_entities::get(&f.state, Kind::Album, album.id, Some(f.bob.id))
            .await
            .is_err()
    );
    let source = asset(&f, artist.id, Some(album.id), "public").await;
    let stored = music_assets::get(&f.state, source, Some(f.owner.id))
        .await
        .unwrap();
    assert_eq!(stored.artist, "New artist");
    assert_eq!(stored.album, "New album");
    assert_eq!(
        music_entities::document(&f.state, Kind::Artist, artist.id)
            .await
            .unwrap()["name"],
        artist.name
    );
    let mut invalid = new_entity("Invalid artist", None);
    invalid.genres = vec!["ambient".into()];
    assert!(
        music_entities::create(&f.state, f.owner.id, Kind::Artist, &invalid)
            .await
            .is_err()
    );
}

fn deletion(revision: i64, recursive: bool) -> music_entities::Delete {
    music_entities::Delete {
        revision: revision.to_string(),
        recursive,
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn album_detach_preserves_track_identity_bytes_and_compilation_artist_authorization(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let performer = music_entities::create(
        &f.state,
        f.owner.id,
        Kind::Artist,
        &new_entity("Compilation performer", None),
    )
    .await
    .unwrap();
    let source = asset(&f, performer.id, Some(f.album), "public").await;
    let publication = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    // The album artist is public through its compilation track even though it
    // is not the primary performer. Every public discovery path agrees.
    assert_eq!(
        music_entities::statistics(&f.state, Kind::Artist, f.artist, None)
            .await
            .unwrap()
            .track_count,
        1
    );
    assert_eq!(
        music_entities::tracks(&f.state, Kind::Artist, f.artist, None, None)
            .await
            .unwrap()
            .items[0]
            .id,
        source
    );
    assert!(
        music_entities::catalogue(&f.state, Kind::Artist, None, false, "Shared artist", None)
            .await
            .unwrap()
            .iter()
            .any(|e| e.id == f.artist)
    );
    let before = music_assets::get(&f.state, source, None).await.unwrap();
    let mut artist_edit = edit(
        &music_entities::get(&f.state, Kind::Artist, f.artist, Some(f.owner.id))
            .await
            .unwrap(),
    );
    artist_edit.description = "Compilation artist biography".into();
    music_entities::update(&f.state, f.owner.id, Kind::Artist, f.artist, &artist_edit)
        .await
        .unwrap();
    let refreshed = plamenu_db::status::find_by_id(&f.state.pool, publication.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu::note::note_for_status(&f.state, &refreshed, &f.owner)
            .await
            .unwrap()["track"]["album"]["artists"][0]["summary"],
        "Compilation artist biography"
    );
    let revision = music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
        .await
        .unwrap()
        .revision;
    music_entities::delete(
        &f.state,
        f.owner.id,
        Kind::Album,
        f.album,
        &deletion(revision, false),
    )
    .await
    .unwrap();
    let after = music_assets::get(&f.state, source, None).await.unwrap();
    assert_eq!(after.album_id, None);
    assert_eq!(after.artist_id, performer.id);
    assert_eq!(after.original_file, before.original_file);
    assert_eq!(after.stream_file, before.stream_file);
    assert_eq!(
        music_publications::published(&f.state, f.owner.id, source)
            .await
            .unwrap()
            .unwrap()
            .id,
        publication.id
    );
    assert!(
        music_entities::document(&f.state, Kind::Album, f.album)
            .await
            .is_err()
    );
    assert!(
        music_entities::document(&f.state, Kind::Artist, f.artist)
            .await
            .is_err()
    );
    let stored = plamenu_db::status::find_by_id(&f.state.pool, publication.id)
        .await
        .unwrap()
        .unwrap();
    let document = plamenu::note::note_for_status(&f.state, &stored, &f.owner)
        .await
        .unwrap();
    assert_ne!(
        document["track"]["album"]["id"],
        music_entities::uri(TEST_DOMAIN, Kind::Album, f.album)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_cleanup_jobs")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn recursive_artist_deletion_retires_all_sources_artwork_and_collection_references_atomically(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let source = asset(&f, f.artist, Some(f.album), "public").await;
    let private = asset(&f, f.artist, None, "private").await;
    let publication = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    let unaffected_artist = music_entities::create(
        &f.state,
        f.owner.id,
        Kind::Artist,
        &new_entity("Unrelated performer", None),
    )
    .await
    .unwrap();
    let unaffected = asset(&f, unaffected_artist.id, None, "public").await;
    let foreign = music_entities::create(
        &f.state,
        f.bob.id,
        Kind::Artist,
        &new_entity("Shared artist", None),
    )
    .await
    .unwrap();
    let collection = id::next();
    sqlx::query("INSERT INTO music_collections(id,owner_id,name,kind,visibility) VALUES($1,$2,'A saved playlist','playlist','public')").bind(collection).bind(f.bob.id).execute(&f.state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,1)").bind(id::next()).bind(collection).bind(source).execute(&f.state.pool).await.unwrap();
    for (asset, artist, album) in [
        (Some(source), None, None),
        (None, Some(f.artist), None),
        (None, None, Some(f.album)),
    ] {
        let artwork = id::next();
        sqlx::query("INSERT INTO music_artworks(id,owner_id,asset_id,artist_id,album_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,$4,$5,'image',$6,'image/png',50,1,1,$7)").bind(artwork).bind(f.owner.id).bind(asset).bind(artist).bind(album).bind(format!("musicart{artwork}.png")).bind(id::next()).execute(&f.state.pool).await.unwrap();
    }
    assert!(matches!(
        music_entities::delete(
            &f.state,
            f.owner.id,
            Kind::Artist,
            f.artist,
            &deletion(1, false)
        )
        .await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert!(matches!(
        music_entities::delete(
            &f.state,
            f.owner.id,
            Kind::Artist,
            f.artist,
            &deletion(99, true)
        )
        .await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert!(matches!(
        music_entities::delete(
            &f.state,
            f.bob.id,
            Kind::Artist,
            f.artist,
            &deletion(1, true)
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    music_entities::delete(
        &f.state,
        f.owner.id,
        Kind::Artist,
        f.artist,
        &deletion(1, true),
    )
    .await
    .unwrap();
    for track in [source, private] {
        assert!(
            music_assets::get(&f.state, track, Some(f.owner.id))
                .await
                .is_err()
        );
    }
    assert!(
        plamenu_db::status::find_by_id(&f.state.pool, publication.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(music_assets::get(&f.state, unaffected, None).await.is_ok());
    assert!(
        music_entities::get(&f.state, Kind::Artist, foreign.id, Some(f.bob.id))
            .await
            .is_ok()
    );
    assert!(
        music_entities::get(&f.state, Kind::Artist, f.artist, Some(f.owner.id))
            .await
            .is_err()
    );
    assert!(
        music_entities::get(&f.state, Kind::Album, f.album, Some(f.owner.id))
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_storage_debt WHERE owner_id=$1")
            .bind(f.owner.id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        7
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_collection_entries WHERE collection_id=$1"
        )
        .bind(collection)
        .fetch_one(&f.state.pool)
        .await
        .unwrap(),
        0
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM music_collections WHERE id=$1")
            .bind(collection)
            .fetch_one(&f.state.pool)
            .await
            .unwrap()
            > 1
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn entity_deletion_rolls_back_entire_batch_on_delivery_failure(pool: PgPool) {
    let f = fixture(pool).await;
    let source = asset(&f, f.artist, Some(f.album), "public").await;
    let remote = RemoteUser::new("listener.test", "listener");
    let listener = plamenu::remote::store_remote_actor(&f.state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&f.state.pool, listener.id, f.owner.id, None)
        .await
        .unwrap();
    let publication = music_publications::publish(&f.state, &f.owner, source)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_entity_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced bulk deletion outbox failure'; END $$").execute(&f.state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_entity_delete BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_entity_delete()").execute(&f.state.pool).await.unwrap();
    assert!(
        music_entities::delete(
            &f.state,
            f.owner.id,
            Kind::Artist,
            f.artist,
            &deletion(1, true)
        )
        .await
        .is_err()
    );
    assert!(music_assets::get(&f.state, source, None).await.is_ok());
    assert!(
        music_entities::document(&f.state, Kind::Album, f.album)
            .await
            .is_ok()
    );
    assert!(
        !plamenu_db::status::is_deleted(&f.state.pool, publication.id)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_cleanup_jobs")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_creation_and_deletion_api_enforces_write_scope_and_current_revision(pool: PgPool) {
    let f = fixture(pool).await;
    let (read, write) = api_tokens(&f).await;
    let app = plamenu::build_router(f.state.clone());
    let body =
        serde_json::json!({"name":"Created through native API","tags":["Ambient"]}).to_string();
    let mut created = None;
    for (token, expected) in [(&read, StatusCode::FORBIDDEN), (&write, StatusCode::OK)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/music/artists")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["revision"], "1");
            assert_eq!(value["tags"], serde_json::json!(["ambient"]));
            created = value["id"].as_str().map(str::to_owned);
        }
    }
    let created = created.unwrap();
    for (token, revision, expected) in [
        (&read, "1", StatusCode::FORBIDDEN),
        (&write, "2", StatusCode::CONFLICT),
        (&write, "1", StatusCode::NO_CONTENT),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/music/artists/{created}"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"revision":revision}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(
        music_entities::get(
            &f.state,
            Kind::Artist,
            created.parse().unwrap(),
            Some(f.owner.id)
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn totals_endpoints_require_read_scope_and_preserve_full_private_source_counts(pool: PgPool) {
    let f = fixture(pool).await;
    let source = asset(&f, f.artist, Some(f.album), "private").await;
    let list = plamenu::music_collections::create(
        &f.state,
        f.owner.id,
        &plamenu::music_collections::New {
            name: "Private totals".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    plamenu::music_collections::add(
        &f.state,
        f.owner.id,
        list.id,
        &plamenu::music_collections::Add {
            remote_id: None,
            episode_id: None,
            asset_id: Some(source.to_string()),
            status_id: None,
            revision: list.revision.to_string(),
        },
    )
    .await
    .unwrap();
    let (read, write) = api_tokens(&f).await;
    let app = plamenu::build_router(f.state.clone());
    for path in [
        format!("/api/v1/music/artists/{}/stats", f.artist),
        format!("/api/v1/music/albums/{}/stats", f.album),
        format!("/api/v1/music/collections/{}/stats", list.id),
    ] {
        for (token, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(&write), StatusCode::FORBIDDEN),
            (Some(&read), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri(&path);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            if status == StatusCode::OK {
                let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                let data: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(data["track_count"], 1);
                assert_eq!(data["total_duration"], 10.0);
            }
        }
    }
    music_assets::delete(&f.state, f.owner.id, source)
        .await
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/music/collections/{}/stats", list.id))
                .header("Authorization", format!("Bearer {read}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let data: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(data["track_count"], 0);
    assert_eq!(data["total_duration"], 0.0);
}
