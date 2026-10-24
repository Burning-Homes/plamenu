mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{AppState, error::ApiError, music_assets, music_favourites, music_queues::Source};
use plamenu_db::{
    PgPool, id, notification,
    notification_policy::{self, Disposition, Policy},
};
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Saved performer')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Saved recording',$3,'private',$4,$5,100,100,$6,10,'mp3')").bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    (state, owner, viewer, asset)
}
fn source(asset: i64) -> Source {
    Source {
        remote_id: None,
        episode_id: None,
        asset_id: Some(asset.to_string()),
        status_id: None,
    }
}
async fn notes(state: &AppState, recipient: i64) -> Vec<notification::Notification> {
    sqlx::query_as(
        "SELECT * FROM notifications WHERE account_id=$1 AND kind='music.favourite' ORDER BY id",
    )
    .bind(recipient)
    .fetch_all(&state.pool)
    .await
    .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn concurrent_saves_are_idempotent_and_current_notifications_retract_on_undo(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    let source = source(asset);
    let (a, b) = tokio::join!(
        music_favourites::save(&state, viewer, &source),
        music_favourites::save(&state, viewer, &source)
    );
    assert_eq!(a.unwrap().id, b.unwrap().id);
    let notifications = notes(&state, owner).await;
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].status_id.is_none());
    let rendered = plamenu::entities::render_notifications(
        &state.pool,
        &state.config.domain,
        &notifications,
        owner,
    )
    .await
    .unwrap();
    assert_eq!(rendered[0]["music_track"]["title"], "Saved recording");
    assert!(rendered[0].get("status").is_none());
    sqlx::query("UPDATE music_assets SET title='Changed recording' WHERE id=$1")
        .bind(asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let rendered = plamenu::entities::render_notifications(
        &state.pool,
        &state.config.domain,
        &notifications,
        owner,
    )
    .await
    .unwrap();
    assert_eq!(rendered[0]["music_track"]["title"], "Changed recording");
    assert_eq!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items[0]
            .track
            .title,
        "Changed recording"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    assert!(notes(&state, owner).await.is_empty());
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    music_assets::delete(&state, owner, asset).await.unwrap();
    assert!(
        notes(&state, owner).await.is_empty(),
        "Source deletion retracts the event"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn saves_respect_revocation_blocks_and_source_or_account_deletion(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    let source = source(asset);
    assert!(matches!(
        music_favourites::save(&state, viewer, &source).await,
        Err(ApiError::NotFound)
    ));
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    music_assets::grant(&state, owner, asset, viewer, false)
        .await
        .unwrap();
    let page = music_favourites::page(&state, viewer, None).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.before.is_none());
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("Saved recording")
    );
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    assert!(notes(&state, owner).await.is_empty());
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    sqlx::query("DELETE FROM blocks")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(viewer)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(notes(&state, owner).await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        music_favourites::page(&state, viewer, None).await,
        Err(ApiError::NotFound)
    ));
    music_favourites::save(&state, owner, &source)
        .await
        .unwrap();
    assert!(
        notes(&state, owner).await.is_empty(),
        "No self notification"
    );
    music_assets::delete(&state, owner, asset).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn music_events_obey_the_existing_sender_policy(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    let source = source(asset);
    notification_policy::upsert(
        &state.pool,
        owner,
        Policy {
            for_not_following: Disposition::Drop,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    assert!(notes(&state, owner).await.is_empty());
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    notification_policy::upsert(
        &state.pool,
        owner,
        Policy {
            for_not_following: Disposition::Filter,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    let notifications = notes(&state, owner).await;
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].filtered);
    assert_eq!(
        notification_policy::evaluate(&state.pool, owner, viewer, "music.favourite", None)
            .await
            .unwrap(),
        Disposition::Filter
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM push_delivery_jobs")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    assert!(notes(&state, owner).await.is_empty());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_saves_preserve_origin_without_likes_and_hide_tombstones(pool: PgPool) {
    let (state, _, viewer, _) = fixture(pool).await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let author = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    let document = serde_json::json!({"id":"https://songhive.test/ap/audio/saved-track","type":"Audio","attributedTo":remote.actor.id,"name":"Remote saved recording","to":["https://www.w3.org/ns/activitystreams#Public"],"url":[{"type":"Link","mediaType":"audio/mpeg","href":"https://songhive.test/track.mp3"}],"track":{"name":"Remote saved recording","artists":[{"name":"Remote performer"}]}});
    let audio = plamenu::ingest::ingest_remote_note(&state, &author, &document)
        .await
        .unwrap();
    let source = Source {
        remote_id: None,
        episode_id: None,
        asset_id: None,
        status_id: Some(audio.id.to_string()),
    };
    let saved = music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    assert_eq!(saved.track.title, "Remote saved recording");
    assert_eq!(
        saved.track.url,
        "https://songhive.test/ap/audio/saved-track"
    );
    assert_eq!(saved.status_id, Some(audio.id.to_string()));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(notes(&state, viewer).await.is_empty());
    plamenu_db::status::delete_by_uri(&state.pool, audio.uri.as_deref().unwrap(), author.id)
        .await
        .unwrap();
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn failed_save_insertion_rolls_back_the_event(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_music_save() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture save failure'; END; $$; CREATE TRIGGER reject_music_save BEFORE INSERT ON music_favourites FOR EACH ROW EXECUTE FUNCTION reject_music_save();").execute(&state.pool).await.unwrap();
    assert!(
        music_favourites::save(&state, viewer, &source(asset))
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(notes(&state, owner).await.is_empty());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn favourites_api_requires_scopes_and_private_cache_policy(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    use tower::ServiceExt;
    let (state, owner, _, asset) = fixture(pool).await;
    let u = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let client = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "favourites-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let read = generate_secret();
    let write = generate_secret();
    for (token, scopes) in [(&read, "read:statuses"), (&write, "write:statuses")] {
        oauth::create_token(
            &state.pool,
            &hash_secret(token),
            client.id,
            Some(u.id),
            scopes,
        )
        .await
        .unwrap();
    }
    let app = plamenu::build_router(state.clone());
    let path = "/api/v1/music/favourites";
    for (token, expected) in [(&read, StatusCode::OK), (&write, StatusCode::FORBIDDEN)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
        }
    }
    for method in ["POST", "DELETE"] {
        for (token, expected) in [(&read, StatusCode::FORBIDDEN), (&write, StatusCode::OK)] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("Authorization", format!("Bearer {token}"))
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"asset_id":asset.to_string()}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn paging_counts_only_currently_readable_saves(pool: PgPool) {
    let (state, _, viewer, template) = fixture(pool).await;
    for index in 0..157 {
        let asset = id::next();
        let title = if index < 52 {
            format!("Readable {index}")
        } else {
            format!("Hidden {index}")
        };
        let visibility = if index < 52 { "public" } else { "private" };
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) SELECT $1,owner_id,$2,artist_id,$3,$4,$5,100,100,$6,10,'mp3' FROM music_assets WHERE id=$7").bind(asset).bind(title).bind(visibility).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).bind(template).execute(&state.pool).await.unwrap();
        // A save does not turn a revoked source into an access grant.
        sqlx::query("INSERT INTO music_favourites(id,account_id,asset_id) VALUES($1,$2,$3)")
            .bind(id::next())
            .bind(viewer)
            .bind(asset)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    let first = music_favourites::page(&state, viewer, None).await.unwrap();
    assert_eq!(first.items.len(), 50);
    assert!(!serde_json::to_string(&first).unwrap().contains("Hidden"));
    let cursor = first.before.as_deref().unwrap().parse().unwrap();
    let second = music_favourites::page(&state, viewer, Some(cursor))
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(second.before.is_none());
    let unique = first
        .items
        .iter()
        .chain(&second.items)
        .map(|f| &f.id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), 52);
}
