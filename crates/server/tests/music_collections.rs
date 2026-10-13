mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState, music_assets,
    music_collections::{self, Add, New, Reorder},
};
use plamenu_db::{PgPool, id};

async fn setup(pool: PgPool) -> (AppState, i64, i64) {
    let alice = create_local_account(&pool, "alice", "Alice").await.id;
    let bob = create_local_account(&pool, "bob", "Bob").await.id;
    (
        test_state_with(pool, StubFederation::with_actors([])),
        alice,
        bob,
    )
}
async fn asset(state: &AppState, owner: i64, title: &str, visibility: &str) -> i64 {
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,$3)")
        .bind(artist)
        .bind(owner)
        .bind(format!("Artist {artist}"))
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,$3,$4,$5,$6,$7,100,100,$8,10,'mp3')")
        .bind(asset).bind(owner).bind(title).bind(artist).bind(visibility).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    asset
}
async fn collection(
    state: &AppState,
    owner: i64,
    kind: &str,
    visibility: &str,
) -> music_collections::Collection {
    music_collections::create(
        state,
        owner,
        &New {
            name: "Listening room".into(),
            description: "Music references".into(),
            kind: kind.into(),
            visibility: visibility.into(),
        },
    )
    .await
    .unwrap()
}
async fn add(state: &AppState, owner: i64, collection: i64, asset: i64) {
    let revision = music_collections::get(state, collection, Some(owner))
        .await
        .unwrap()
        .revision;
    music_collections::add(
        state,
        owner,
        collection,
        &Add {
            remote_id: None,
            episode_id: None,
            asset_id: Some(asset.to_string()),
            status_id: None,
            revision: revision.to_string(),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_contents_share_grants_only_owned_tracks_and_revokes_with_membership(
    pool: PgPool,
) {
    let (state, alice, bob) = setup(pool).await;
    let carol = create_local_account(&state.pool, "carol", "Carol").await.id;
    let own = asset(&state, alice, "Shared own track", "private").await;
    let foreign = asset(&state, carol, "Foreign private track", "private").await;
    music_assets::grant(&state, carol, foreign, alice, true)
        .await
        .unwrap();
    let list = collection(&state, alice, "playlist", "private").await;
    add(&state, alice, list.id, own).await;
    add(&state, alice, list.id, foreign).await;
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::grant(&state, alice, list.id, current.revision, bob, true)
        .await
        .unwrap();
    assert!(music_assets::get(&state, own, Some(bob)).await.is_err());
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::set_contents_sharing(&state, alice, list.id, current.revision, true)
        .await
        .unwrap();
    assert_eq!(
        music_assets::get(&state, own, Some(bob)).await.unwrap().id,
        own
    );
    assert!(music_assets::get(&state, foreign, Some(bob)).await.is_err());
    assert!(music_assets::get(&state, own, None).await.is_err());
    assert_eq!(
        music_collections::statistics(&state, list.id, Some(bob))
            .await
            .unwrap()
            .track_count,
        1
    );
    let entries = music_collections::entries(&state, list.id).await.unwrap();
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::remove(
        &state,
        alice,
        list.id,
        current.revision,
        entries
            .iter()
            .find(|entry| entry.asset_id == Some(own))
            .unwrap()
            .id,
    )
    .await
    .unwrap();
    assert!(music_assets::get(&state, own, Some(bob)).await.is_err());
    assert!(
        music_collections::playables(&state, list.id, Some(bob))
            .await
            .unwrap()
            .is_empty()
    );
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_asset_grants WHERE asset_id=$1 AND account_id=$2",
    )
    .bind(own)
    .bind(bob)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        grants, 0,
        "derived access must never become a permanent direct grant"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn independent_parent_and_direct_grants_survive_other_parent_revocation(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let source = asset(&state, alice, "Two parent track", "private").await;
    let first = collection(&state, alice, "playlist", "private").await;
    let second = collection(&state, alice, "library", "private").await;
    for list in [&first, &second] {
        add(&state, alice, list.id, source).await;
        let current = music_collections::get(&state, list.id, Some(alice))
            .await
            .unwrap();
        music_collections::grant(&state, alice, list.id, current.revision, bob, true)
            .await
            .unwrap();
        let current = music_collections::get(&state, list.id, Some(alice))
            .await
            .unwrap();
        music_collections::set_contents_sharing(&state, alice, list.id, current.revision, true)
            .await
            .unwrap();
    }
    assert!(music_assets::get(&state, source, Some(bob)).await.is_ok());
    let current = music_collections::get(&state, first.id, Some(alice))
        .await
        .unwrap();
    music_collections::grant(&state, alice, first.id, current.revision, bob, false)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, Some(bob)).await.is_ok());
    music_assets::grant(&state, alice, source, bob, true)
        .await
        .unwrap();
    let current = music_collections::get(&state, second.id, Some(alice))
        .await
        .unwrap();
    music_collections::delete(&state, alice, second.id, current.revision)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, Some(bob)).await.is_ok());
    music_assets::grant(&state, alice, source, bob, false)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, Some(bob)).await.is_err());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_public_contents_access_is_current_and_never_publishes_audio(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let source = asset(
        &state,
        alice,
        "Deliberately shared private track",
        "private",
    )
    .await;
    let list = collection(&state, alice, "library", "public").await;
    assert!(!list.share_tracks);
    let enabled =
        music_collections::set_contents_sharing(&state, alice, list.id, list.revision, true)
            .await
            .unwrap();
    assert!(
        music_collections::set_contents_sharing(&state, bob, list.id, enabled.revision, false)
            .await
            .is_err()
    );
    assert!(
        music_collections::set_contents_sharing(&state, alice, list.id, list.revision, false)
            .await
            .is_err()
    );
    add(&state, alice, list.id, source).await;
    assert!(music_assets::get(&state, source, None).await.is_ok());
    assert!(
        music_collections::m3u(&state, list.id, None)
            .await
            .unwrap()
            .contains(&format!("/music/assets/{source}/stream"))
    );
    let document = plamenu::music_library_federation::document(&state, list.id, None)
        .await
        .unwrap();
    assert_eq!(
        document["totalItems"], 0,
        "sharing bytes does not manufacture a publication"
    );
    let publications: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_publications WHERE asset_id=$1")
            .bind(source)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(publications, 0);
    plamenu_db::block::create(&state.pool, bob, alice, None)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, Some(bob)).await.is_err());
    assert!(music_assets::get(&state, source, None).await.is_ok());
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::set_contents_sharing(&state, alice, list.id, current.revision, false)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, None).await.is_err());
    assert_eq!(
        music_collections::statistics(&state, list.id, None)
            .await
            .unwrap()
            .track_count,
        0
    );
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::set_contents_sharing(&state, alice, list.id, current.revision, true)
        .await
        .unwrap();
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::update(
        &state,
        alice,
        list.id,
        current.revision,
        &New {
            name: current.name.clone(),
            description: current.description.clone(),
            kind: current.kind.clone(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    assert!(music_assets::get(&state, source, None).await.is_err());
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(alice)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_assets::get(&state, source, Some(alice))
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_contents_sharing_does_not_admit_anonymous_or_foreign_viewers(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let actor = common::RemoteUser::new("remote.test", "foreign");
    let remote = plamenu::remote::store_remote_actor(&state.pool, &actor.actor)
        .await
        .unwrap()
        .id;
    let source = asset(&state, alice, "Local members only", "private").await;
    let list = collection(&state, alice, "playlist", "local").await;
    add(&state, alice, list.id, source).await;
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::set_contents_sharing(&state, alice, list.id, current.revision, true)
        .await
        .unwrap();
    assert!(music_assets::get(&state, source, Some(bob)).await.is_ok());
    assert!(music_assets::get(&state, source, None).await.is_err());
    assert!(
        music_assets::get(&state, source, Some(remote))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn contents_sharing_api_requires_owner_write_scope_and_current_revision(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    use tower::ServiceExt;
    let (state, alice, bob) = setup(pool).await;
    let list = collection(&state, alice, "playlist", "private").await;
    let mut tokens = Vec::new();
    for (owner, email) in [(alice, "alice@plamenu.test"), (bob, "bob@plamenu.test")] {
        let user = user::create(
            &state.pool,
            owner,
            Some(email),
            &hash_password("fixture-password").unwrap(),
        )
        .await
        .unwrap();
        let app = oauth::create_app(
            &state.pool,
            oauth::NewApp {
                name: "contents-tests",
                website: None,
                client_id: &generate_secret(),
                client_secret_hash: &hash_secret(&generate_secret()),
                redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
                scopes: "read write",
            },
        )
        .await
        .unwrap();
        for scope in ["read:statuses", "write:statuses"] {
            let token = generate_secret();
            oauth::create_token(
                &state.pool,
                &hash_secret(&token),
                app.id,
                Some(user.id),
                scope,
            )
            .await
            .unwrap();
            tokens.push(token);
        }
    }
    let app = plamenu::build_router(state.clone());
    let path = format!("/api/v1/music/collections/{}/contents-sharing", list.id);
    let body =
        serde_json::json!({"revision":list.revision.to_string(),"share_tracks":true}).to_string();
    for (token, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(&tokens[0]), StatusCode::FORBIDDEN),
        (Some(&tokens[3]), StatusCode::NOT_FOUND),
        (Some(&tokens[1]), StatusCode::OK),
        (Some(&tokens[1]), StatusCode::CONFLICT),
    ] {
        let mut request = Request::builder()
            .method("PUT")
            .uri(&path)
            .header("Content-Type", "application/json");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.clone())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            let value: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(value["share_tracks"], true);
        }
    }
    assert!(
        music_collections::get(&state, list.id, Some(alice))
            .await
            .unwrap()
            .share_tracks
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn full_statistics_count_repeats_beyond_displayed_pages_and_hide_revoked_sources(
    pool: PgPool,
) {
    let (state, alice, bob) = setup(pool).await;
    let private = asset(&state, alice, "Private repeated source", "private").await;
    let public = asset(&state, bob, "Public repeated source", "public").await;
    let list = collection(&state, alice, "playlist", "private").await;
    let ids = (0..105).map(|_| id::next()).collect::<Vec<_>>();
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) SELECT id,$2,$3,position::integer FROM unnest($1::bigint[]) WITH ORDINALITY e(id,position)")
        .bind(ids).bind(list.id).bind(private).execute(&state.pool).await.unwrap();
    for _ in 0..3 {
        add(&state, alice, list.id, public).await;
    }
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    music_collections::grant(&state, alice, list.id, current.revision, bob, true)
        .await
        .unwrap();
    let first_page = music_collections::entry_page(&state, list.id, None)
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 50);
    let owner = music_collections::statistics(&state, list.id, Some(alice))
        .await
        .unwrap();
    assert_eq!(owner.track_count, 108);
    assert!((owner.total_duration - 1080.0).abs() < f64::EPSILON);
    let viewer = music_collections::statistics(&state, list.id, Some(bob))
        .await
        .unwrap();
    assert_eq!(viewer.track_count, 3);
    assert!((viewer.total_duration - 30.0).abs() < f64::EPSILON);
    music_assets::delete(&state, bob, public).await.unwrap();
    let viewer = music_collections::statistics(&state, list.id, Some(bob))
        .await
        .unwrap();
    assert_eq!(viewer.track_count, 0);
    assert!(viewer.total_duration.abs() < f64::EPSILON);
    music_assets::grant(&state, alice, private, bob, true)
        .await
        .unwrap();
    let viewer = music_collections::statistics(&state, list.id, Some(bob))
        .await
        .unwrap();
    assert_eq!(viewer.track_count, 105);
    assert!((viewer.total_duration - 1050.0).abs() < f64::EPSILON);
    music_assets::grant(&state, alice, private, bob, false)
        .await
        .unwrap();
    assert_eq!(
        music_collections::statistics(&state, list.id, Some(bob))
            .await
            .unwrap()
            .track_count,
        0
    );
    plamenu_db::block::create(&state.pool, bob, alice, None)
        .await
        .unwrap();
    assert!(
        music_collections::statistics(&state, list.id, Some(bob))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn collection_grants_never_bypass_asset_permissions_or_block_revocation(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let private = asset(&state, alice, "Creator-private title", "private").await;
    let public = collection(&state, alice, "library", "public").await;
    assert!(
        music_collections::add(
            &state,
            alice,
            public.id,
            &Add {
                remote_id: None,
                episode_id: None,
                asset_id: Some(private.to_string()),
                status_id: None,
                revision: public.revision.to_string()
            }
        )
        .await
        .is_err()
    );
    let list = collection(&state, alice, "playlist", "private").await;
    add(&state, alice, list.id, private).await;
    let revision = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap()
        .revision;
    music_collections::grant(&state, alice, list.id, revision, bob, true)
        .await
        .unwrap();
    assert!(
        music_collections::playables(&state, list.id, Some(bob))
            .await
            .unwrap()
            .is_empty()
    );
    music_assets::grant(&state, alice, private, bob, true)
        .await
        .unwrap();
    let items = music_collections::playables(&state, list.id, Some(bob))
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Creator-private title");
    music_assets::grant(&state, alice, private, bob, false)
        .await
        .unwrap();
    assert!(
        music_collections::playables(&state, list.id, Some(bob))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        music_collections::m3u(&state, list.id, Some(bob))
            .await
            .unwrap(),
        "#EXTM3U\n"
    );
    assert!(music_collections::m3u(&state, list.id, None).await.is_err());
    assert_eq!(
        music_collections::entries(&state, list.id)
            .await
            .unwrap()
            .len(),
        1,
        "revocation hides the reference without erasing the owner's playlist"
    );
    plamenu_db::block::create(&state.pool, alice, bob, None)
        .await
        .unwrap();
    assert!(
        music_collections::get(&state, list.id, Some(bob))
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn ordering_is_revision_checked_and_m3u_hides_revoked_sources(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let a = asset(&state, alice, "Title\n#EXTINF:999,Injected", "public").await;
    let b = asset(&state, alice, "Second", "public").await;
    let list = collection(&state, alice, "playlist", "public").await;
    add(&state, alice, list.id, a).await;
    add(&state, alice, list.id, b).await;
    let current = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap();
    let entries = music_collections::entries(&state, list.id).await.unwrap();
    let order = Reorder {
        revision: current.revision.to_string(),
        entry_ids: vec![entries[1].id.to_string(), entries[0].id.to_string()],
    };
    assert!(
        music_collections::reorder(&state, bob, list.id, &order)
            .await
            .is_err()
    );
    music_collections::reorder(&state, alice, list.id, &order)
        .await
        .unwrap();
    assert!(matches!(
        music_collections::reorder(&state, alice, list.id, &order).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    let visible = music_collections::playables(&state, list.id, None)
        .await
        .unwrap();
    assert_eq!(visible[0].title, "Second");
    assert_eq!(visible[1].entry_id, entries[0].id.to_string());
    let m3u = music_collections::m3u(&state, list.id, None).await.unwrap();
    assert_eq!(m3u.lines().filter(|v| v.starts_with("#EXTINF:")).count(), 2);
    assert!(!m3u.lines().any(|v| v.starts_with("#EXTINF:999")));
    let original = music_assets::get(&state, a, Some(alice)).await.unwrap();
    music_assets::update(
        &state,
        alice,
        a,
        &music_assets::Edit {
            title: original.title.clone(),
            visibility: "private".into(),
            music: original.metadata(),
            revision: Some(original.revision.to_string()),
        },
    )
    .await
    .unwrap();
    let m3u = music_collections::m3u(&state, list.id, None).await.unwrap();
    assert!(!m3u.contains("Injected"));
    assert!(!m3u.contains(&format!("/music/assets/{a}/stream")));
    let revision = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap()
        .revision;
    let duplicate = Reorder {
        revision: revision.to_string(),
        entry_ids: vec![entries[0].id.to_string(), entries[0].id.to_string()],
    };
    assert!(
        music_collections::reorder(&state, alice, list.id, &duplicate)
            .await
            .is_err()
    );
    music_collections::remove(&state, alice, list.id, revision, entries[1].id)
        .await
        .unwrap();
    assert!(
        music_assets::get(&state, b, Some(alice)).await.is_ok(),
        "removing a reference never deletes the original"
    );
    assert_eq!(
        music_collections::entries(&state, list.id).await.unwrap()[0].position,
        1
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn libraries_deduplicate_references_playlists_allow_repeats_and_urls_are_canonical(
    pool: PgPool,
) {
    let (state, alice, _) = setup(pool).await;
    let a = asset(&state, alice, "Track", "public").await;
    let library = collection(&state, alice, "library", "public").await;
    add(&state, alice, library.id, a).await;
    let revision = music_collections::get(&state, library.id, Some(alice))
        .await
        .unwrap()
        .revision;
    add(&state, alice, library.id, a).await;
    assert_eq!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        music_collections::get(&state, library.id, Some(alice))
            .await
            .unwrap()
            .revision,
        revision
    );
    let playlist = collection(&state, alice, "playlist", "public").await;
    add(&state, alice, playlist.id, a).await;
    add(&state, alice, playlist.id, a).await;
    assert_eq!(
        music_collections::entries(&state, playlist.id)
            .await
            .unwrap()
            .len(),
        2
    );
    let url = format!("https://{}/music/assets/{a}", state.config.domain);
    assert_eq!(
        music_collections::resolve_reference(&state, &url)
            .await
            .unwrap(),
        (Some(a.to_string()), None)
    );
    for url in [
        format!("{url}?token=secret"),
        format!("{url}#player"),
        format!("https://other.test/music/assets/{a}"),
        "file:///etc/passwd".into(),
    ] {
        assert!(
            music_collections::resolve_reference(&state, &url)
                .await
                .is_err()
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn verified_remote_audio_remains_origin_owned_and_source_tombstones_hide_it(pool: PgPool) {
    let (state, alice, _) = setup(pool).await;
    let remote = common::RemoteUser::new("songhive.test", "sylvia");
    let owner = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    let uri = "https://songhive.test/tracks/collection-original";
    let object = serde_json::json!({"id":uri,"type":"Audio","attributedTo":owner.uri,"name":"Origin title","content":"Original recording","to":[plamenu_ap::activity::PUBLIC],"attachment":[{"type":"Audio","mediaType":"audio/mpeg","url":"https://songhive.test/audio.mp3"}],"track":{"artists":[{"name":"Origin artist"}]}});
    let source = plamenu::ingest::ingest_remote_note(&state, &owner, &object)
        .await
        .unwrap();
    let list = collection(&state, alice, "playlist", "public").await;
    music_collections::add(
        &state,
        alice,
        list.id,
        &Add {
            remote_id: None,
            episode_id: None,
            asset_id: None,
            status_id: Some(source.id.to_string()),
            revision: list.revision.to_string(),
        },
    )
    .await
    .unwrap();
    let tracks = music_collections::playables(&state, list.id, None)
        .await
        .unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Origin title");
    assert_eq!(tracks[0].artist, "Origin artist");
    assert_eq!(
        music_collections::resolve_reference(&state, uri)
            .await
            .unwrap(),
        (None, Some(source.id.to_string()))
    );
    let copies: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id=$1")
        .bind(alice)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(copies, 0);
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(outbox, 0);
    assert!(
        plamenu_db::status::stub_by_uri(&state.pool, uri, owner.id)
            .await
            .unwrap()
    );
    assert!(
        music_collections::playables(&state, list.id, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_cascade_advances_revision_and_paging_exposes_only_readable_sources(pool: PgPool) {
    let (state, alice, bob) = setup(pool).await;
    let list = collection(&state, alice, "playlist", "public").await;
    let readable = asset(&state, alice, "Readable", "public").await;
    let hidden = asset(&state, alice, "Hidden creator title", "private").await;
    // Publicizing a private playlist retains references but never source ACLs.
    let private = collection(&state, alice, "playlist", "private").await;
    add(&state, alice, private.id, hidden).await;
    let revision = music_collections::get(&state, private.id, Some(alice))
        .await
        .unwrap()
        .revision;
    music_collections::update(
        &state,
        alice,
        private.id,
        revision,
        &New {
            name: "Mixed sources".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let (items, next) = music_collections::playable_page(&state, private.id, Some(bob), None)
        .await
        .unwrap();
    assert!(items.is_empty());
    assert!(next.is_none());
    for _ in 0..51 {
        add(&state, alice, list.id, readable).await;
    }
    let (first, next) = music_collections::playable_page(&state, list.id, None, None)
        .await
        .unwrap();
    assert_eq!(first.len(), 50);
    let (second, last) = music_collections::playable_page(&state, list.id, None, next)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert!(last.is_none());
    assert_ne!(first[49].entry_id, second[0].entry_id);
    let prior = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap()
        .revision;
    music_assets::delete(&state, alice, readable).await.unwrap();
    let after = music_collections::get(&state, list.id, Some(alice))
        .await
        .unwrap()
        .revision;
    assert!(after > prior);
    assert!(
        music_collections::entries(&state, list.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        music_collections::reorder(
            &state,
            alice,
            list.id,
            &Reorder {
                revision: prior.to_string(),
                entry_ids: Vec::new()
            }
        )
        .await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_search_filters_kind_before_paging_and_preserves_creator_visibility(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "search_owner", "Owner")
        .await
        .id;
    let viewer = create_local_account(&pool, "search_viewer", "Viewer")
        .await
        .id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    for (kind, visibility) in [
        ("library", "public"),
        ("playlist", "public"),
        ("playlist", "private"),
    ] {
        music_collections::create(
            &state,
            owner,
            &music_collections::New {
                name: "Evening collection".into(),
                description: "Ambient selection".into(),
                kind: kind.into(),
                visibility: visibility.into(),
            },
        )
        .await
        .unwrap();
    }
    for _ in 0..62 {
        music_collections::create(
            &state,
            owner,
            &music_collections::New {
                name: "Other collection".into(),
                description: String::new(),
                kind: "library".into(),
                visibility: "public".into(),
            },
        )
        .await
        .unwrap();
    }
    let filtered =
        music_collections::search(&state, None, false, None, "EVENING", Some("playlist"))
            .await
            .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].visibility, "public");
    assert_eq!(
        music_collections::search(&state, Some(owner), true, None, "ambient", Some("playlist"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        music_collections::search(&state, None, false, None, "", Some("unknown"))
            .await
            .is_err()
    );
    assert!(
        music_collections::search(&state, None, false, Some(-1), "", None)
            .await
            .is_err()
    );
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_collections::search(&state, Some(viewer), false, None, "evening", None)
            .await
            .unwrap()
            .is_empty()
    );
}
