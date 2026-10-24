mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::{
    actions::{self, PostParams},
    entities, music_follows as music,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{
    PgPool,
    media::{self, NewLocalMedia},
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn audio(
    state: &plamenu::AppState,
    username: &str,
    owner: i64,
    visibility: &str,
) -> plamenu_db::status::Status {
    let id = plamenu_db::id::next();
    let mut input = NewLocalMedia::new(owner, id, "tone.mp3", "audio/mpeg");
    input.kind = "audio";
    input.duration = Some(3.0);
    media::create_local(&state.pool, input).await.unwrap();
    actions::post_status(
        state,
        PostParams {
            username,
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn signed_library_follow_delivers_audio_without_social_follow_or_private_posts(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let library = music::library_uri(TEST_DOMAIN, &alice);
    let follow = json!({"id":"https://songhive.test/follow/music-1","type":"Follow","actor":remote.actor.id,"object":library});
    let bytes = serde_json::to_vec(&follow).unwrap();
    let headers =
        remote
            .signer()
            .sign_post(TEST_DOMAIN, "/inbox", &bytes, std::time::SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", headers.host)
        .header("date", headers.date)
        .header("digest", headers.digest)
        .header("signature", headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    assert_eq!(
        common::test_app_with(pool.clone(), stub)
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );
    let subscriber = plamenu_db::account::find_by_uri(&pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(music::list(&state, subscriber.id).await.unwrap().len(), 1);
    assert!(
        plamenu_db::follow::find(&pool, subscriber.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
    let accept: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Accept' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(accept["object"], follow);
    assert_eq!(accept["actor"], entities::account_uri(TEST_DOMAIN, &alice));
    let public = audio(&state, "alice", alice.id, "public").await;
    let private = audio(&state, "alice", alice.id, "private").await;
    let unlisted = audio(&state, "alice", alice.id, "unlisted").await;
    let creates: Vec<Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(
        creates[0]["object"]["id"],
        entities::status_uri_for_account(TEST_DOMAIN, &public, &alice)
    );
    assert_eq!(creates[0]["object"]["library"], library);
    let index = music::library(&state, &alice, None).await.unwrap();
    assert_eq!(index["totalItems"], 1);
    let page = music::library(&state, &alice, Some(1)).await.unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], creates[0]["object"]["id"]);
    assert!(music::library(&state, &alice, Some(2)).await.is_err());
    for stored in [&private, &unlisted] {
        assert!(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["id"] != entities::status_uri_for_account(TEST_DOMAIN, stored, &alice))
        );
    }
    actions::edit_status(
        &state,
        &alice,
        public.id,
        actions::EditParams {
            title: Some("Changed tune"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updates: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Update'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(updates, 1);
    actions::delete_status(&state, &alice, public.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let deletes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Delete'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deletes, 1);
    assert_eq!(
        music::library(&state, &alice, None).await.unwrap()["totalItems"],
        0
    );
    music::incoming(
        &state,
        &subscriber,
        &json!({"type":"Undo","actor":remote.actor.id,"object":follow}),
    )
    .await
    .unwrap();
    assert!(music::list(&state, subscriber.id).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn outbound_resource_follow_checks_owner_exact_follow_and_undo(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let attacker = RemoteUser::new("other.test", "mallory");
    let target = "https://songhive.test/users/sylvia/library";
    let stub = StubFederation::with_actors([remote.actor.clone(), attacker.actor.clone()]);
    stub.objects.lock().unwrap().insert(
        target.into(),
        json!({"id":target,"type":"Library","actor":remote.actor.id,"name":"Music <safe>"}),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    music::follow(&state, &alice, target).await.unwrap();
    music::follow(&state, &alice, target).await.unwrap();
    let rows = music::list(&state, alice.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].pending);
    let entity = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(entity["follower_id"], alice.id.to_string());
    assert!(entity["owner_id"].is_string());
    let owner = plamenu_db::account::find_by_uri(&pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    let impostor = plamenu::remote::store_remote_actor(&pool, &attacker.actor)
        .await
        .unwrap();
    let follow: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Follow' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    music::incoming(
        &state,
        &impostor,
        &json!({"type":"Accept","actor":attacker.actor.id,"object":follow}),
    )
    .await
    .unwrap();
    assert!(music::list(&state, alice.id).await.unwrap()[0].pending);
    let mut altered = follow.clone();
    altered["object"] = json!("https://songhive.test/another-library");
    music::incoming(
        &state,
        &owner,
        &json!({"type":"Accept","actor":remote.actor.id,"object":altered}),
    )
    .await
    .unwrap();
    assert!(music::list(&state, alice.id).await.unwrap()[0].pending);
    music::incoming(
        &state,
        &owner,
        &json!({"type":"Accept","actor":remote.actor.id,"object":follow}),
    )
    .await
    .unwrap();
    assert!(!music::list(&state, alice.id).await.unwrap()[0].pending);
    assert!(
        plamenu_db::follow::find(&pool, alice.id, owner.id)
            .await
            .unwrap()
            .is_none()
    );
    music::unfollow(&state, &alice, target).await.unwrap();
    assert!(music::list(&state, alice.id).await.unwrap().is_empty());
    let undo: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->>'type'='Undo' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(undo["object"], follow);
    stub.objects.lock().unwrap().insert(
        target.into(),
        json!({"id":target,"type":"Library","actor":attacker.actor.id,"name":"Wrong owner"}),
    );
    assert!(music::follow(&state, &alice, target).await.is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn track_follow_repeat_undo_blocks_and_atomic_response(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    let subscriber = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let stored = audio(&state, "alice", alice.id, "unlisted").await;
    let target = entities::status_uri_for_account(TEST_DOMAIN, &stored, &alice);
    let first = json!({"id":"https://songhive.test/follow/first","type":"Follow","actor":remote.actor.id,"object":target});
    assert!(music::incoming(&state, &subscriber, &first).await.unwrap());
    let mut repeat = first.clone();
    repeat["id"] = json!("https://songhive.test/follow/repeat");
    music::incoming(&state, &subscriber, &repeat).await.unwrap();
    music::incoming(
        &state,
        &subscriber,
        &json!({"type":"Undo","actor":remote.actor.id,"object":first}),
    )
    .await
    .unwrap();
    assert_eq!(
        music::list(&state, subscriber.id).await.unwrap()[0].follow_uri,
        repeat["id"]
    );
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        actions::EditParams {
            title: Some("Track update"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updates: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Update'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(updates, 1);
    plamenu_db::block::create(&pool, alice.id, subscriber.id, None)
        .await
        .unwrap();
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        actions::EditParams {
            title: Some("Blocked update"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let unchanged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE activity->>'type'='Update'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unchanged, updates);
    music::incoming(&state, &subscriber, &repeat).await.unwrap();
    assert!(music::list(&state, subscriber.id).await.unwrap().is_empty());
    plamenu_db::block::delete(&pool, alice.id, subscriber.id)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_music_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test delivery failure'; END $$; CREATE TRIGGER reject_music_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_music_delivery();").execute(&pool).await.unwrap();
    assert!(music::incoming(&state, &subscriber, &repeat).await.is_err());
    assert!(music::list(&state, subscriber.id).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_library_pages_hide_deleted_private_and_wrong_actor_resources(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    for _ in 0..51 {
        audio(&state, "alice", alice.id, "public").await;
    }
    let page1 = music::library(&state, &alice, Some(1)).await.unwrap();
    let page2 = music::library(&state, &alice, Some(2)).await.unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 50);
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page1["next"], page2["id"]);
    assert_eq!(page2["prev"], page1["id"]);
    assert!(
        page1["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["id"] != page2["items"][0]["id"])
    );
    let private = audio(&state, "alice", alice.id, "private").await;
    assert!(
        music::follow(
            &state,
            &bob,
            &entities::status_uri_for_account(TEST_DOMAIN, &private, &alice)
        )
        .await
        .is_err()
    );
    let public = audio(&state, "alice", alice.id, "public").await;
    let wrong = format!(
        "{}/statuses/{}",
        entities::account_uri(TEST_DOMAIN, &bob),
        public.id
    );
    assert!(music::follow(&state, &bob, &wrong).await.is_err());
    music::follow(&state, &bob, &music::library_uri(TEST_DOMAIN, &alice))
        .await
        .unwrap();
    assert!(!music::list(&state, bob.id).await.unwrap()[0].pending);
    assert!(
        plamenu_db::follow::find(&pool, bob.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
    let url = music::library_uri(TEST_DOMAIN, &alice);
    let path = url.strip_prefix(&format!("https://{TEST_DOMAIN}")).unwrap();
    let response = common::test_app(pool.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    plamenu_db::account::suspend(&pool, alice.id, "local")
        .await
        .unwrap();
    assert!(music::library(&state, &alice, None).await.is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn metadata_resource_subscribers_receive_their_own_updates_and_tombstones(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    let subscriber = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let stored = audio(&state, "alice", alice.id, "public").await;
    let root = entities::status_uri_for_account(TEST_DOMAIN, &stored, &alice);
    for entity in ["artist", "album", "track"] {
        let follow = json!({"id":format!("https://songhive.test/follow/{entity}"),"type":"Follow","actor":remote.actor.id,"object":format!("{root}/{entity}")});
        assert!(music::incoming(&state, &subscriber, &follow).await.unwrap());
    }
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        actions::EditParams {
            music: Some(plamenu_db::music::Metadata {
                artist: "Changed artist".into(),
                album: "Changed album".into(),
                track_number: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let updates: Vec<Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (entity, kind, name) in [
        ("artist", "Artist", "Changed artist"),
        ("album", "Album", "Changed album"),
        ("track", "Track", "A tune"),
    ] {
        let update = updates
            .iter()
            .find(|v| v["object"]["id"] == format!("{root}/{entity}"))
            .expect("metadata resource Update");
        assert_eq!(update["object"]["type"], kind);
        assert_eq!(update["object"]["name"], name);
        assert_eq!(update["actor"], entities::account_uri(TEST_DOMAIN, &alice));
    }
    actions::delete_status(&state, &alice, stored.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let deletes: Vec<Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Delete'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for entity in ["artist", "album", "track"] {
        assert!(
            deletes
                .iter()
                .any(|v| v["object"]["id"] == format!("{root}/{entity}")
                    && v["object"]["type"] == "Tombstone")
        );
    }
    assert!(
        plamenu_db::follow::find(&pool, subscriber.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
}
