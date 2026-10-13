mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{
    actions::{self, EditParams, PostParams},
    note,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{
    PgPool, follow,
    media::{self, NewLocalMedia},
    music,
};
use serde_json::{Value, json};

async fn upload(pool: &PgPool, account: i64, mime: &str) -> i64 {
    let id = plamenu_db::id::next();
    let mut input = NewLocalMedia::new(account, id, "song.mp3", mime);
    input.kind = if mime.starts_with("audio/") {
        "audio"
    } else {
        "image"
    };
    input.duration = Some(12.0);
    media::create_local(pool, input).await.unwrap().id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn native_audio_create_served_object_edit_and_delete_keep_one_identity(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    let follower = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    follow::create(&pool, follower.id, alice.id, None)
        .await
        .unwrap();
    let media_id = upload(&pool, alice.id, "audio/mpeg").await;
    let metadata = music::Metadata {
        artist: "Artist & friends".into(),
        album: "An album".into(),
        ..Default::default()
    };
    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "A recording",
            title: Some("Track title"),
            visibility: "public",
            media_ids: &[media_id],
            kind: PostKind::Audio,
            music: Some(&metadata),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stored.object_type.as_deref(), Some("Audio"));
    let served = note::note_for_status(&state, &stored, &alice)
        .await
        .unwrap();
    let create:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->'object'->>'id'=$1 AND activity->>'type'='Create' LIMIT 1").bind(served["id"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(served["track"], create["object"]["track"]);
    assert_eq!(served["type"], "Audio");
    assert_eq!(served["track"]["name"], "Track title");
    assert_eq!(served["track"]["artists"][0]["name"], metadata.artist);
    let edited_music = music::Metadata {
        artist: "Updated artist".into(),
        album: "Updated album".into(),
        release_year: Some(2026),
        track_number: Some(2),
        disc_number: Some(1),
        ..Default::default()
    };
    let updated = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            music: Some(edited_music.clone()),
            text: Some("Updated recording"),
            title: Some("Changed title"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let changed = note::note_for_status(&state, &updated, &alice)
        .await
        .unwrap();
    assert_eq!(changed["id"], served["id"]);
    assert_eq!(changed["type"], "Audio");
    assert_eq!(changed["track"]["name"], "Changed title");
    assert_eq!(changed["track"]["artists"][0]["name"], "Updated artist");
    assert_eq!(changed["track"]["album"]["released"], "2026-01-01");
    let update: Value = sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' AND activity->'object'->>'id'=$1 LIMIT 1").bind(changed["id"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(changed["track"], update["object"]["track"]);
    assert!(
        actions::edit_status(
            &state,
            &alice,
            stored.id,
            EditParams {
                music: Some(music::Metadata {
                    track_number: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        music::for_statuses(&pool, &[stored.id]).await.unwrap()[&stored.id],
        edited_music
    );
    let refused = actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            media_ids: Some(vec![]),
            ..Default::default()
        },
    )
    .await;
    assert!(refused.is_err());
    assert_eq!(
        media::for_statuses(&pool, &[stored.id])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        actions::edit_status(
            &state,
            &alice,
            stored.id,
            EditParams {
                music: Some(music::Metadata {
                    artist: "bad\0artist".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let public = plamenu::audio::catalogue(&state, None, false, "Updated artist", None)
        .await
        .unwrap();
    assert_eq!(public.items.len(), 1);
    actions::delete_status(&state, &alice, stored.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    assert!(
        plamenu::audio::catalogue(&state, None, false, "", None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn invalid_audio_never_publishes_or_claims_an_attachment(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let own = upload(&pool, alice.id, "audio/mpeg").await;
    let foreign = upload(&pool, bob.id, "audio/mpeg").await;
    let image = upload(&pool, alice.id, "image/png").await;
    for (title, ids) in [
        (None, vec![own]),
        (Some("Audio"), vec![]),
        (Some("Audio"), vec![foreign]),
        (Some("Audio"), vec![image]),
        (Some("Audio"), vec![own, foreign]),
    ] {
        assert!(
            actions::post_status(
                &state,
                PostParams {
                    username: "alice",
                    title,
                    visibility: "public",
                    media_ids: &ids,
                    kind: PostKind::Audio,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id=$1")
        .bind(alice.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let attached: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id=$1")
            .bind(own)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attached, None);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn catalogue_respects_private_visibility_blocks_and_literal_search(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    for visibility in ["public", "direct", "local"] {
        let id = upload(&pool, alice.id, "audio/mpeg").await;
        actions::post_status(
            &state,
            PostParams {
                username: "alice",
                title: Some("Audio"),
                visibility,
                media_ids: &[id],
                kind: PostKind::Audio,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        plamenu::audio::catalogue(&state, None, false, "", None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        plamenu::audio::catalogue(&state, Some(alice.id), true, "", None)
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    assert!(
        plamenu::audio::catalogue(&state, None, false, "%", None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    plamenu_db::block::create(&pool, bob.id, alice.id, None)
        .await
        .unwrap();
    assert!(
        plamenu::audio::catalogue(&state, Some(bob.id), false, "", None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn incoming_songhive_music_metadata_is_escaped_and_retained(pool: PgPool) {
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([remote.actor.clone()]),
    );
    let actor = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let object = json!({"id":"https://songhive.test/tracks/123","type":"Audio","attributedTo":[actor.uri,"https://songhive.test/artists/123"],"name":"A song","content":"A recording","to":[plamenu_ap::activity::PUBLIC],"published":"2026-10-08T00:00:00Z","attachment":[{"type":"Audio","mediaType":"audio/mpeg","url":"https://songhive.test/music.mp3"}],"track":{"name":"A song","artists":[{"name":"<script>Artist</script>"}],"album":{"name":"Album","released":"2020-01-01"},"position":2}});
    let stored = plamenu::ingest::ingest_remote_note(&state, &actor, &object)
        .await
        .unwrap();
    let rendered = plamenu::entities::render_statuses(
        &pool,
        "plamenu.test",
        std::slice::from_ref(&stored),
        None,
    )
    .await
    .unwrap();
    assert_eq!(rendered[0]["music"]["artist"], "<script>Artist</script>");
    assert_eq!(rendered[0]["music"]["release_year"], 2020);
    assert_eq!(rendered[0]["music"]["track_number"], 2);
    let listener = create_local_account(&pool, "listener", "Listener").await;
    let boosted = actions::reblog_status(&state, &listener, stored.id)
        .await
        .unwrap();
    let wrapped =
        plamenu::entities::render_status(&pool, "plamenu.test", &boosted, Some(listener.id))
            .await
            .unwrap();
    assert_eq!(wrapped["music"], serde_json::Value::Null);
    assert!(
        wrapped.get("music").is_some(),
        "A boost wrapper retains the complete Status shape"
    );
    assert_eq!(
        wrapped["reblog"]["music"]["artist"],
        "<script>Artist</script>"
    );
    assert_eq!(wrapped["reblog"]["music"]["release_year"], 2020);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn audio_outbox_failure_rolls_back_metadata_status_and_attachment(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("songhive.test", "follower");
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([remote.actor.clone()]),
    );
    let follower = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    follow::create(&pool, follower.id, alice.id, None)
        .await
        .unwrap();
    let id = upload(&pool, alice.id, "audio/mpeg").await;
    sqlx::raw_sql("CREATE FUNCTION reject_audio_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'outbox unavailable'; END; $$; CREATE TRIGGER reject_audio_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_audio_delivery();").execute(&pool).await.unwrap();
    assert!(
        actions::post_status(
            &state,
            PostParams {
                username: "alice",
                title: Some("A song"),
                visibility: "public",
                media_ids: &[id],
                kind: PostKind::Audio,
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&pool)
        .await
        .unwrap();
    let metadata: i64 = sqlx::query_scalar("SELECT count(*) FROM status_music")
        .fetch_one(&pool)
        .await
        .unwrap();
    let attachment: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((statuses, metadata, attachment), (0, 0, None));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn music_documents_use_parent_actor_visibility_and_deletion_gates(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let mut public = None;
    for visibility in ["public", "unlisted", "private", "direct", "local"] {
        let attachment = upload(&pool, alice.id, "audio/mpeg").await;
        let (stored, _) = actions::post_status(
            &state,
            PostParams {
                username: "alice",
                text: "A recording",
                title: Some("Title"),
                visibility,
                kind: PostKind::Audio,
                media_ids: &[attachment],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for entity in ["track", "artist", "album"] {
            let uri = format!("/users/alice/statuses/{}/{entity}", stored.id);
            let response = common::test_app(pool.clone())
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header("accept", "application/activity+json")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if ["public", "unlisted"].contains(&visibility) {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                },
                "{visibility} {entity}"
            );
            let wrong = format!("/users/bob/statuses/{}/{entity}", stored.id);
            let response = common::test_app(pool.clone())
                .oneshot(
                    Request::builder()
                        .uri(wrong)
                        .header("accept", "application/activity+json")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        if visibility == "public" {
            public = Some(stored);
        }
    }
    let public = public.unwrap();
    actions::delete_status(&state, &alice, public.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    for entity in ["track", "artist", "album"] {
        let response = common::test_app(pool.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/users/alice/statuses/{}/{entity}", public.id))
                    .header("accept", "application/activity+json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
