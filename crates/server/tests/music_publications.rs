mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    actions::{self, DeleteMode, PostParams},
    music_assets::{self, Edit},
    music_publications,
};
use plamenu_db::{PgPool, account::Account, id};
use serde_json::Value;

struct Fixture {
    state: AppState,
    owner: Account,
    asset: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let state = test_state_with(pool, StubFederation::with_actors([remote.actor.clone()]));
    let follower = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&state.pool, follower.id, owner.id, None)
        .await
        .unwrap();
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year) VALUES($1,$2,'Retained original',$3,'public',$4,$5,100,100,$6,10,'mp3',2026)")
        .bind(asset).bind(owner.id).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    state
        .media
        .put(&format!("music{asset}.orig"), vec![0; 100])
        .await
        .unwrap();
    state
        .media
        .put(&format!("music{asset}.mp3"), vec![1; 100])
        .await
        .unwrap();
    Fixture {
        state,
        owner,
        asset,
    }
}
async fn count(f: &Fixture, table: &'static str) -> i64 {
    // Test-only identifiers are compile-time literals selected by the caller.
    let query = match table {
        "statuses" => "SELECT count(*) FROM statuses",
        "media_attachments" => "SELECT count(*) FROM media_attachments",
        "music_publications" => "SELECT count(*) FROM music_publications",
        _ => panic!("unexpected test table"),
    };
    sqlx::query_scalar(query)
        .fetch_one(&f.state.pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn publish_withdraw_republish_retains_bytes_and_uses_a_fresh_audio_identity(pool: PgPool) {
    let f = fixture(pool).await;
    let first = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    assert_eq!(first.object_type.as_deref(), Some("Audio"));
    assert_eq!(
        music_publications::publish(&f.state, &f.owner, f.asset)
            .await
            .unwrap()
            .id,
        first.id
    );
    let object = plamenu::note::note_for_status(&f.state, &first, &f.owner)
        .await
        .unwrap();
    assert_eq!(
        object["attachment"][0]["url"],
        format!(
            "https://{}/music/assets/{}/stream",
            f.state.config.domain, f.asset
        )
    );
    assert_eq!(object["track"]["album"]["released"], "2026-01-01");
    let jobs: Vec<Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create'")
            .fetch_all(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["object"]["id"], object["id"]);
    music_publications::withdraw(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    assert_eq!(count(&f, "music_publications").await, 0);
    assert_eq!(
        f.state
            .media
            .get(&format!("music{}.mp3", f.asset))
            .await
            .unwrap()
            .len(),
        100
    );
    let cleanup: i64 = sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        cleanup, 0,
        "withdrawal never schedules retained music for removal"
    );
    let second = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(
        music_assets::get(&f.state, f.asset, Some(f.owner.id))
            .await
            .unwrap()
            .id,
        f.asset
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn metadata_updates_and_privacy_retraction_share_the_asset_transaction(pool: PgPool) {
    let f = fixture(pool).await;
    let first = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    let original = music_assets::get(&f.state, f.asset, Some(f.owner.id))
        .await
        .unwrap();
    let mut metadata = original.metadata();
    metadata.artist = "Updated artist".into();
    metadata.release_year = Some(2024);
    let changed = music_assets::update(
        &f.state,
        f.owner.id,
        f.asset,
        &Edit {
            title: "Updated title".into(),
            visibility: "public".into(),
            revision: Some(original.revision.to_string()),
            music: metadata,
        },
    )
    .await
    .unwrap();
    let stored = plamenu_db::status::find_by_id(&f.state.pool, first.id)
        .await
        .unwrap()
        .unwrap();
    let object = plamenu::note::note_for_status(&f.state, &stored, &f.owner)
        .await
        .unwrap();
    assert_eq!(object["name"], "Updated title");
    assert_eq!(object["track"]["artists"][0]["name"], "Updated artist");
    assert_eq!(object["track"]["album"]["released"], "2024-01-01");
    let update:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'id'=$1").bind(object["id"].as_str().unwrap()).fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(update["object"]["track"], object["track"]);
    // Keep a real reply-tree stub while retracting the linked publication.
    actions::post_status(
        &f.state,
        PostParams {
            username: &f.owner.username,
            text: "Reply",
            visibility: "public",
            in_reply_to_id: Some(first.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    music_assets::update(
        &f.state,
        f.owner.id,
        f.asset,
        &Edit {
            title: changed.title.clone(),
            visibility: "private".into(),
            music: changed.metadata(),
            revision: Some(changed.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert!(
        plamenu_db::status::is_deleted(&f.state.pool, first.id)
            .await
            .unwrap()
    );
    assert_eq!(count(&f, "music_publications").await, 0);
    assert!(music_assets::get(&f.state, f.asset, None).await.is_err());
    assert!(
        music_assets::get(&f.state, f.asset, Some(f.owner.id))
            .await
            .is_ok()
    );
    assert!(
        music_publications::publish(&f.state, &f.owner, f.asset)
            .await
            .is_err()
    );
    let deletion:i64=sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Delete' AND activity->'object'->>'id'=$1").bind(object["id"].as_str().unwrap()).fetch_one(&f.state.pool).await.unwrap();
    assert_eq!(deletion, 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn publication_and_private_transition_failures_roll_back_every_related_row(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::raw_sql("CREATE FUNCTION reject_music_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'outbox unavailable'; END; $$; CREATE TRIGGER reject_music_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_music_delivery();").execute(&f.state.pool).await.unwrap();
    assert!(
        music_publications::publish(&f.state, &f.owner, f.asset)
            .await
            .is_err()
    );
    for table in ["statuses", "media_attachments", "music_publications"] {
        assert_eq!(count(&f, table).await, 0);
    }
    sqlx::query("DROP TRIGGER reject_music_delivery ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let first = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_music_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_music_delivery()").execute(&f.state.pool).await.unwrap();
    let asset = music_assets::get(&f.state, f.asset, Some(f.owner.id))
        .await
        .unwrap();
    assert!(
        music_assets::update(
            &f.state,
            f.owner.id,
            f.asset,
            &Edit {
                title: asset.title.clone(),
                visibility: "private".into(),
                music: asset.metadata(),
                revision: Some(asset.revision.to_string())
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        music_assets::get(&f.state, f.asset, None)
            .await
            .unwrap()
            .visibility,
        "public"
    );
    assert_eq!(count(&f, "music_publications").await, 1);
    assert!(
        !plamenu_db::status::is_deleted(&f.state.pool, first.id)
            .await
            .unwrap()
    );
    assert!(
        music_assets::delete(&f.state, f.owner.id, f.asset)
            .await
            .is_err()
    );
    assert_eq!(count(&f, "music_publications").await, 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn ordinary_audio_delete_and_source_delete_use_distinct_storage_lifetimes(pool: PgPool) {
    let f = fixture(pool).await;
    let first = music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    actions::delete_status(&f.state, &f.owner, first.id, DeleteMode::Wipe)
        .await
        .unwrap();
    assert_eq!(count(&f, "music_publications").await, 0);
    assert!(
        music_assets::get(&f.state, f.asset, Some(f.owner.id))
            .await
            .is_ok()
    );
    music_publications::publish(&f.state, &f.owner, f.asset)
        .await
        .unwrap();
    music_assets::delete(&f.state, f.owner.id, f.asset)
        .await
        .unwrap();
    assert_eq!(count(&f, "music_publications").await, 0);
    assert_eq!(count(&f, "media_attachments").await, 0);
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT file_name FROM media_cleanup_jobs ORDER BY file_name")
            .fetch_all(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(
        keys,
        vec![
            format!("music{}.mp3", f.asset),
            format!("music{}.orig", f.asset)
        ]
    );
}
