mod common;
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{
    AppState,
    actions::{self, PostParams},
    entities,
    music_collections::{self, Add, New},
    music_follows, music_library_federation as libraries,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{
    PgPool,
    account::Account,
    id,
    media::{self, NewLocalMedia},
    status::Status,
};
use serde_json::{Value, json};

async fn audio(state: &AppState, owner: &Account, visibility: &str) -> Status {
    let id = id::next();
    let mut media = NewLocalMedia::new(owner.id, id, "tone.mp3", "audio/mpeg");
    media.kind = "audio";
    media.duration = Some(3.0);
    media::create_local(&state.pool, media).await.unwrap();
    actions::post_status(
        state,
        PostParams {
            username: &owner.username,
            title: Some("A tune"),
            visibility,
            kind: PostKind::Audio,
            media_ids: &[id],
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0
}
async fn collection(
    state: &AppState,
    owner: i64,
    visibility: &str,
) -> music_collections::Collection {
    music_collections::create(
        state,
        owner,
        &New {
            name: "Public listening room".into(),
            description: "References retain their publisher".into(),
            kind: "library".into(),
            visibility: visibility.into(),
        },
    )
    .await
    .unwrap()
}
async fn add(state: &AppState, owner: i64, collection: i64, status: i64) {
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
            asset_id: None,
            status_id: Some(status.to_string()),
            revision: revision.to_string(),
        },
    )
    .await
    .unwrap();
}
async fn setup(pool: PgPool) -> (AppState, Account, Account) {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let state = test_state_with(pool, StubFederation::with_actors([remote.actor.clone()]));
    let subscriber = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    (state, owner, subscriber)
}
async fn follow(state: &AppState, subscriber: &Account, collection: i64) -> Value {
    let follow = json!({"id":format!("https://songhive.test/follows/{collection}"),"type":"Follow","actor":subscriber.uri,"object":libraries::uri(TEST_DOMAIN,collection)});
    assert!(
        music_follows::incoming(state, subscriber, &follow)
            .await
            .unwrap()
    );
    follow
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn contents_sharing_changes_and_library_delivery_commit_or_rollback_together(pool: PgPool) {
    let (state, owner, subscriber) = setup(pool).await;
    let library = collection(&state, owner.id, "public").await;
    follow(&state, &subscriber, library.id).await;
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_contents_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced contents delivery failure'; END $$").execute(&state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_contents_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_contents_delivery()")
        .execute(&state.pool).await.unwrap();
    assert!(
        music_collections::set_contents_sharing(
            &state,
            owner.id,
            library.id,
            library.revision,
            true
        )
        .await
        .is_err()
    );
    let unchanged = music_collections::get(&state, library.id, Some(owner.id))
        .await
        .unwrap();
    assert_eq!(unchanged.revision, library.revision);
    assert!(!unchanged.share_tracks);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        jobs
    );
    sqlx::query("DROP TRIGGER reject_contents_delivery ON delivery_jobs")
        .execute(&state.pool)
        .await
        .unwrap();
    let updated = music_collections::set_contents_sharing(
        &state,
        owner.id,
        library.id,
        library.revision,
        true,
    )
    .await
    .unwrap();
    assert!(updated.share_tracks);
    assert_eq!(updated.revision, library.revision + 1);
    let updates:Vec<Value>=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'type'='Library'")
        .fetch_all(&state.pool).await.unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0]["object"]["id"],
        libraries::uri(TEST_DOMAIN, library.id)
    );
    assert_eq!(
        libraries::document(&state, library.id, None).await.unwrap()["totalItems"],
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn named_library_follow_delivers_scoped_audio_edits_retractions_and_exact_undo(pool: PgPool) {
    let (state, owner, subscriber) = setup(pool).await;
    let library = collection(&state, owner.id, "public").await;
    let follow = follow(&state, &subscriber, library.id).await;
    assert!(
        plamenu_db::follow::find(&state.pool, subscriber.id, owner.id)
            .await
            .unwrap()
            .is_none()
    );
    let track = audio(&state, &owner, "public").await;
    add(&state, owner.id, library.id, track.id).await;
    let creates: Vec<Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create'")
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(
        creates[0]["object"]["library"],
        libraries::uri(TEST_DOMAIN, library.id)
    );
    assert_eq!(
        creates[0]["object"]["id"],
        entities::status_uri_for_account(TEST_DOMAIN, &track, &owner)
    );
    actions::edit_status(
        &state,
        &owner,
        track.id,
        actions::EditParams {
            title: Some("Revised tune"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let update:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'type'='Audio' LIMIT 1").fetch_one(&state.pool).await.unwrap();
    assert_eq!(update["object"]["name"], "Revised tune");
    assert_eq!(
        update["object"]["library"],
        libraries::uri(TEST_DOMAIN, library.id)
    );
    let wrong = json!({"type":"Undo","actor":subscriber.uri,"object":{"id":"https://songhive.test/follows/stale","type":"Follow","actor":subscriber.uri,"object":libraries::uri(TEST_DOMAIN,library.id)}});
    assert!(
        music_follows::incoming(&state, &subscriber, &wrong)
            .await
            .unwrap()
    );
    assert_eq!(
        music_follows::list(&state, subscriber.id)
            .await
            .unwrap()
            .len(),
        1
    );
    actions::delete_status(&state, &owner, track.id, actions::DeleteMode::Stub)
        .await
        .unwrap();
    let delete:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete' AND activity->'object'->>'library' IS NOT NULL LIMIT 1").fetch_one(&state.pool).await.unwrap();
    assert_eq!(delete["object"]["id"], creates[0]["object"]["id"]);
    assert_eq!(
        libraries::document(&state, library.id, None).await.unwrap()["totalItems"],
        0
    );
    assert!(
        music_follows::incoming(
            &state,
            &subscriber,
            &json!({"type":"Undo","actor":subscriber.uri,"object":follow})
        )
        .await
        .unwrap()
    );
    assert!(
        music_follows::list(&state, subscriber.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn named_library_withdrawal_and_deletion_are_atomic_and_hide_private_metadata(pool: PgPool) {
    let (state, owner, subscriber) = setup(pool).await;
    let library = collection(&state, owner.id, "public").await;
    follow(&state, &subscriber, library.id).await;
    let private = New {
        name: "Secret collection title".into(),
        description: "Secret description".into(),
        kind: "library".into(),
        visibility: "private".into(),
    };
    sqlx::query("CREATE FUNCTION reject_library_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced outbox failure'; END $$").execute(&state.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_library_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_library_delivery()").execute(&state.pool).await.unwrap();
    assert!(
        music_collections::update(&state, owner.id, library.id, library.revision, &private)
            .await
            .is_err()
    );
    assert!(
        music_collections::delete(&state, owner.id, library.id, library.revision)
            .await
            .is_err()
    );
    assert_eq!(
        music_collections::get(&state, library.id, None)
            .await
            .unwrap()
            .name,
        library.name
    );
    assert_eq!(
        music_follows::list(&state, subscriber.id)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DROP TRIGGER reject_library_delivery ON delivery_jobs")
        .execute(&state.pool)
        .await
        .unwrap();
    music_collections::update(&state, owner.id, library.id, library.revision, &private)
        .await
        .unwrap();
    assert!(libraries::document(&state, library.id, None).await.is_err());
    assert!(
        music_follows::list(&state, subscriber.id)
            .await
            .unwrap()
            .is_empty()
    );
    let delete: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete' LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        delete["object"],
        json!({"id":libraries::uri(TEST_DOMAIN,library.id),"type":"Tombstone"})
    );
    assert!(!delete.to_string().contains("Secret"));
    assert!(!music_follows::incoming(&state,&subscriber,&json!({"id":"https://songhive.test/follows/private","type":"Follow","actor":subscriber.uri,"object":libraries::uri(TEST_DOMAIN,library.id)})).await.unwrap());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn named_library_pages_are_bounded_and_foreign_audio_keeps_origin_ownership(pool: PgPool) {
    let (state, owner, subscriber) = setup(pool).await;
    let library = collection(&state, owner.id, "public").await;
    follow(&state, &subscriber, library.id).await;
    let other = create_local_account(&state.pool, "bob", "Bob").await;
    for _ in 0..51 {
        let track = audio(&state, &other, "public").await;
        add(&state, owner.id, library.id, track.id).await;
    }
    let root = libraries::document(&state, library.id, None).await.unwrap();
    assert_eq!(root["totalItems"], 51);
    assert_eq!(
        root["last"],
        format!("{}?page=2", libraries::uri(TEST_DOMAIN, library.id))
    );
    let first = libraries::document(&state, library.id, Some(1))
        .await
        .unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert_eq!(
        first["items"][0]["attributedTo"],
        entities::account_uri(TEST_DOMAIN, &other)
    );
    assert_eq!(
        libraries::document(&state, library.id, Some(2))
            .await
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        libraries::document(&state, library.id, Some(3))
            .await
            .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Create'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "collecting another publisher's Audio must not send forged Creates"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn retained_original_enters_federated_library_only_after_explicit_publication(pool: PgPool) {
    let (state, owner, subscriber) = setup(pool).await;
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Retained original',$3,'public',$4,$5,100,100,$6,10,'mp3')").bind(asset).bind(owner.id).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    let library = collection(&state, owner.id, "public").await;
    music_collections::add(
        &state,
        owner.id,
        library.id,
        &Add {
            remote_id: None,
            episode_id: None,
            asset_id: Some(asset.to_string()),
            status_id: None,
            revision: library.revision.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        libraries::document(&state, library.id, None).await.unwrap()["totalItems"],
        0
    );
    follow(&state, &subscriber, library.id).await;
    let published = plamenu::music_publications::publish(&state, &owner, asset)
        .await
        .unwrap();
    let page = libraries::document(&state, library.id, Some(1))
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["items"][0]["id"],
        entities::status_uri_for_account(TEST_DOMAIN, &published, &owner)
    );
    let create: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create' LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        create["object"]["library"],
        libraries::uri(TEST_DOMAIN, library.id)
    );
    let source = plamenu::music_assets::get(&state, asset, Some(owner.id))
        .await
        .unwrap();
    plamenu::music_assets::update(
        &state,
        owner.id,
        asset,
        &plamenu::music_assets::Edit {
            title: "Secret retained title".into(),
            music: source.metadata(),
            visibility: "private".into(),
            revision: Some(source.revision.to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        libraries::document(&state, library.id, None).await.unwrap()["totalItems"],
        0
    );
    let delete:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete' AND activity->'object'->>'library' IS NOT NULL LIMIT 1").fetch_one(&state.pool).await.unwrap();
    assert_eq!(delete["object"]["id"], create["object"]["id"]);
    assert!(!delete.to_string().contains("Secret"));
}
