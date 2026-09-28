mod common;

#[sqlx::test(migrations = "../db/migrations")]
async fn delivered_audience_matches_public_and_unlisted_playlist_object(pool: PgPool) {
    let (state, alice, _, channel) = setup(&pool).await;
    let viewer = RemoteUser::new("remote.test", "viewer");
    let viewer = plamenu::remote::store_remote_actor(&pool, &viewer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, channel, None)
        .await
        .unwrap();
    for visibility in ["public", "unlisted"] {
        let p = create(&state, &alice, channel, visibility).await;
        let activity: serde_json::Value = sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->'object'->>'id'=$1 AND activity->>'type'='Create' LIMIT 1")
            .bind(service::uri(&state, &p)).fetch_one(&pool).await.unwrap();
        assert_eq!(activity["to"], activity["object"]["to"]);
        assert_eq!(
            activity["to"]
                .as_array()
                .unwrap()
                .contains(&json!(plamenu_ap::activity::PUBLIC)),
            visibility == "public"
        );
        assert!(
            activity["cc"]
                .as_array()
                .unwrap()
                .contains(&json!(plamenu_ap::activity::PUBLIC))
        );
        assert!(activity["object"]["content"].is_null());
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_delete_uses_the_current_channel_owner_not_a_stale_creator(pool: PgPool) {
    let (stub, owner, channel, url) = remote_fixture();
    let state = test_state_with(pool.clone(), stub.clone());
    let p = remote::import(&state, &url, None).await.unwrap();
    let original = plamenu::remote::store_remote_actor(&pool, &owner.actor)
        .await
        .unwrap();
    let new_owner = RemoteUser::new("remote.test", "newowner");
    stub.actors
        .lock()
        .unwrap()
        .insert(new_owner.actor.id.clone(), new_owner.actor.clone());
    stub.actors
        .lock()
        .unwrap()
        .get_mut(&channel.actor.id)
        .unwrap()
        .attributed_to = Some(json!([new_owner.actor.id]));
    let recipient = plamenu::remote::store_remote_actor(&pool, &new_owner.actor)
        .await
        .unwrap();
    let fresh_channel = stub
        .actors
        .lock()
        .unwrap()
        .remove(&channel.actor.id)
        .unwrap();
    assert!(
        remote::delete(&state, &original, &url).await.is_err(),
        "an unavailable origin cannot restore stale creator authority"
    );
    assert!(
        db::find(&pool, p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
    stub.actors
        .lock()
        .unwrap()
        .insert(channel.actor.id.clone(), fresh_channel);
    assert!(remote::delete(&state, &original, &url).await.unwrap());
    assert!(
        db::find(&pool, p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none(),
        "an old cached creator is no longer authorized after channel handoff"
    );
    let transferred = remote::import(&state, &url, Some(&recipient))
        .await
        .unwrap();
    assert_eq!(transferred.creator_id, recipient.id);
    assert_eq!(transferred.id, p.id);
    assert_eq!(transferred.uuid, p.uuid);
    assert!(remote::delete(&state, &recipient, &url).await.unwrap());
    assert!(
        db::find(&pool, p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accepted_publishers_only_manage_their_own_playlists_and_revocation_is_immediate(
    pool: PgPool,
) {
    let (state, alice, bob, channel) = setup(&pool).await;
    let alice_playlist = create(&state, &alice, channel, "private").await;
    let invite = plamenu::video_channels::Change {
        operation: "invite".into(),
        username: Some("bob".into()),
        role: Some("publisher".into()),
        ..Default::default()
    };
    plamenu::video_channels::change(&state, &alice, channel, &invite)
        .await
        .unwrap();
    assert!(
        service::create(
            &state,
            &bob,
            &Change {
                channel_id: Some(channel.to_string()),
                name: Some("Invited only".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    plamenu::video_channels::change(
        &state,
        &bob,
        channel,
        &plamenu::video_channels::Change {
            operation: "accept".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let bob_playlist = create(&state, &bob, channel, "private").await;
    assert!(
        !service::can_read(&state, &alice_playlist, Some(bob.id))
            .await
            .unwrap()
    );
    assert!(
        !service::can_edit(&state, &alice_playlist, bob.id)
            .await
            .unwrap()
    );
    assert!(
        service::can_edit(&state, &bob_playlist, bob.id)
            .await
            .unwrap()
    );
    plamenu::video_channels::change(
        &state,
        &alice,
        channel,
        &plamenu::video_channels::Change {
            operation: "revoke".into(),
            username: Some("bob".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        !service::can_read(&state, &bob_playlist, Some(bob.id))
            .await
            .unwrap()
    );
    assert!(
        change(
            &state,
            &bob,
            &bob_playlist,
            Change {
                operation: "profile".into(),
                name: Some("Revoked edit".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        service::can_edit(&state, &bob_playlist, alice.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn first_party_app_bootstrap_is_safe_for_simultaneous_creators(pool: PgPool) {
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let states = (0..16).map(|_| state.clone()).collect::<Vec<_>>();
    let results = futures_util::future::join_all(
        states
            .into_iter()
            .map(|state| async move { plamenu::web::session::ensure_web_app(&state).await }),
    )
    .await;
    let ids = results
        .into_iter()
        .map(|r| r.unwrap().id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 1);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_apps WHERE client_id='plamenu-web-ui'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collection_limits_and_batched_large_entry_rendering(pool: PgPool) {
    let (state, alice, _, channel) = setup(&pool).await;
    let video = video(&state, &alice, channel, "public").await;
    let p = create(&state, &alice, channel, "public").await;
    let author = plamenu_db::account::find_by_id(&pool, video.account_id)
        .await
        .unwrap()
        .unwrap();
    let uri = plamenu::entities::status_uri_for_account(&state.config.domain, &video, &author);
    sqlx::query("INSERT INTO video_playlist_entries(id,playlist_id,status_id,video_uri,position) SELECT n,$1,$2,$3,n FROM generate_series(1,1000) n").bind(p.id).bind(video.id).bind(uri).execute(&pool).await.unwrap();
    assert!(matches!(
        change(
            &state,
            &alice,
            &p,
            Change {
                operation: "add".into(),
                status_id: Some(video.id.to_string()),
                ..Default::default()
            }
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
    let before = std::time::Instant::now();
    let app = plamenu::build_router(state.clone());
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::builder()
            .uri(format!("/api/v1/video_playlists/{}", p.id))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let entity: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["entries"].as_array().unwrap().len(), 1000);
    assert!(
        entity["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["available"] == true)
    );
    assert!(
        before.elapsed() < std::time::Duration::from_secs(3),
        "1000 duplicate entries must not cause a per-entry permission query loop"
    );
    sqlx::query("INSERT INTO video_playlists(id,creator_id,channel_id,name,visibility) SELECT n,$1,$2,'Quota playlist','private' FROM generate_series(1,99) n").bind(alice.id).bind(channel).execute(&pool).await.unwrap();
    assert!(matches!(
        service::create(
            &state,
            &alice,
            &Change {
                channel_id: Some(channel.to_string()),
                name: Some("One too many".into()),
                ..Default::default()
            }
        )
        .await,
        Err(ApiError::Unprocessable(_))
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn copied_first_party_permalink_with_anchor_is_a_valid_video_reference(pool: PgPool) {
    let (state, alice, _, channel) = setup(&pool).await;
    let video = video(&state, &alice, channel, "public").await;
    let p = create(&state, &alice, channel, "public").await;
    let url = format!(
        "https://{}/@alice/{}#post-{}",
        state.config.domain, video.id, video.id
    );
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "add".into(),
            video_url: Some(url),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db::entries(&pool, p.id).await.unwrap()[0].status_id,
        Some(video.id)
    );
}
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{
    actions::{self, PostParams},
    error::ApiError,
    video_playlists::{self as service, Change},
    video_playlists_remote as remote,
};
use plamenu_db::{PgPool, account::Account, media, video_playlists as db};
use serde_json::json;

async fn setup(pool: &PgPool) -> (plamenu::AppState, Account, Account, i64) {
    let alice = create_local_account(pool, "alice", "Alice").await;
    let bob = create_local_account(pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    (state, alice, bob, channel.id)
}
async fn video(
    state: &plamenu::AppState,
    actor: &Account,
    channel: i64,
    visibility: &str,
) -> plamenu_db::status::Status {
    let attachment = media::create_local(
        &state.pool,
        media::NewLocalMedia {
            kind: "video",
            width: Some(320),
            height: Some(240),
            duration: Some(10.0),
            small: Some(media::SmallStyle {
                file_name: "video.jpg",
                width: 320,
                height: 180,
            }),
            ..media::NewLocalMedia::new(actor.id, plamenu_db::id::next(), "video.mp4", "video/mp4")
        },
    )
    .await
    .unwrap();
    // Native publication accepts public/unlisted; restricted-entry regressions
    // subsequently tighten the shared status row as an independent audience edit.
    let (stored, _) = actions::post_video(
        state,
        PostParams {
            username: &actor.username,
            title: Some("A Video"),
            visibility: "public",
            media_ids: &[attachment.id],
            kind: plamenu_ap::activity::PostKind::Video,
            ..Default::default()
        },
        channel,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE statuses SET visibility=$2 WHERE id=$1")
        .bind(stored.id)
        .bind(visibility)
        .execute(&state.pool)
        .await
        .unwrap();
    plamenu_db::status::find_by_id(&state.pool, stored.id)
        .await
        .unwrap()
        .unwrap()
}
async fn create(
    state: &plamenu::AppState,
    actor: &Account,
    channel: i64,
    visibility: &str,
) -> db::Playlist {
    service::create(
        state,
        actor,
        &Change {
            channel_id: Some(channel.to_string()),
            name: Some("My films".into()),
            visibility: Some(visibility.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}
async fn change(
    state: &plamenu::AppState,
    actor: &Account,
    p: &db::Playlist,
    mut input: Change,
) -> Result<db::Playlist, ApiError> {
    input.revision = Some(p.revision);
    service::change(state, actor, p.id, &input).await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stable_duplicates_reorder_clips_removal_and_missing_video(pool: PgPool) {
    let (state, alice, _, channel) = setup(&pool).await;
    let film = video(&state, &alice, channel, "public").await;
    let p = create(&state, &alice, channel, "public").await;
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "add".into(),
            status_id: Some(film.id.to_string()),
            start_seconds: Some(1),
            stop_seconds: Some(3),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "add".into(),
            status_id: Some(film.id.to_string()),
            start_seconds: Some(5),
            stop_seconds: Some(8),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let entries = db::entries(&pool, p.id).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].id, entries[1].id);
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "move".into(),
            entry_id: Some(entries[1].id.to_string()),
            position: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let moved = db::entries(&pool, p.id).await.unwrap();
    assert_eq!(
        moved.iter().map(|e| e.id).collect::<Vec<_>>(),
        [entries[1].id, entries[0].id]
    );
    assert_eq!(moved[0].start_seconds, 5);
    for (start, stop) in [(-1, Some(2)), (2, Some(2)), (11, None), (1, Some(11))] {
        assert!(
            change(
                &state,
                &alice,
                &p,
                Change {
                    operation: "clip".into(),
                    entry_id: Some(moved[0].id.to_string()),
                    start_seconds: Some(start),
                    stop_seconds: stop,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
    }
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "clip".into(),
            entry_id: Some(moved[0].id.to_string()),
            start_seconds: Some(0),
            stop_seconds: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "remove".into(),
            entry_id: Some(moved[1].id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let entry = db::entries(&pool, p.id).await.unwrap().remove(0);
    assert_eq!(entry.position, 1);
    assert_eq!(entry.id, moved[0].id);
    actions::delete_status(&state, &alice, film.id, actions::DeleteMode::Stub)
        .await
        .unwrap();
    assert!(
        remote::resolve_entry(&state, &p, &entry, Some(alice.id))
            .await
            .unwrap()
            .is_none()
    );
    let missing = db::entries(&pool, p.id).await.unwrap().remove(0);
    assert_eq!(missing.id, entry.id);
    assert_eq!(missing.position, entry.position);
    assert_eq!(missing.start_seconds, entry.start_seconds);
    assert!(missing.video_uri.is_empty() && missing.status_id.is_none());
    assert!(
        change(
            &state,
            &alice,
            &p,
            Change {
                operation: "delete".into(),
                confirm: Some("wrong title".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let deleted = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "delete".into(),
            confirm: Some(p.name.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(deleted.deleted_at.is_some());
    assert!(
        !service::can_read(&state, &deleted, Some(alice.id))
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_and_unlisted_never_widen_a_video_audience(pool: PgPool) {
    let (state, alice, bob, channel) = setup(&pool).await;
    let film = video(&state, &alice, channel, "private").await;
    let p = create(&state, &alice, channel, "private").await;
    assert!(!service::can_read(&state, &p, None).await.unwrap());
    assert!(!service::can_read(&state, &p, Some(bob.id)).await.unwrap());
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "add".into(),
            status_id: Some(film.id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        change(
            &state,
            &alice,
            &p,
            Change {
                operation: "profile".into(),
                visibility: Some("public".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let e = db::entries(&pool, p.id).await.unwrap().remove(0);
    assert!(
        remote::resolve_entry(&state, &p, &e, Some(alice.id))
            .await
            .unwrap()
            .is_some()
    );
    plamenu::video_channels::change(
        &state,
        &alice,
        channel,
        &plamenu::video_channels::Change {
            operation: "invite".into(),
            username: Some("bob".into()),
            role: Some("editor".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    plamenu::video_channels::change(
        &state,
        &bob,
        channel,
        &plamenu::video_channels::Change {
            operation: "accept".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(service::can_edit(&state, &p, bob.id).await.unwrap());
    assert!(
        matches!(
            change(
                &state,
                &bob,
                &p,
                Change {
                    operation: "clip".into(),
                    entry_id: Some(e.id.to_string()),
                    start_seconds: Some(1),
                    stop_seconds: Some(2),
                    ..Default::default()
                }
            )
            .await,
            Err(ApiError::NotFound)
        ),
        "collection edit authority must not expose a restricted Video's duration"
    );
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "remove".into(),
            entry_id: Some(e.id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "profile".into(),
            visibility: Some("unlisted".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(service::can_read(&state, &p, None).await.unwrap());
    assert!(
        service::list(&state, None, false, None)
            .await
            .unwrap()
            .is_empty()
    );
    let public = video(&state, &alice, channel, "public").await;
    let p = change(
        &state,
        &alice,
        &p,
        Change {
            operation: "add".into(),
            status_id: Some(public.id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(public.id)
        .execute(&pool)
        .await
        .unwrap();
    let e = db::entries(&pool, p.id).await.unwrap().remove(0);
    assert!(
        remote::resolve_entry(&state, &p, &e, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        remote::resolve_entry(&state, &p, &e, Some(alice.id))
            .await
            .unwrap()
            .is_none(),
        "a shared playlist does not show a now-restricted Video even to its creator"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn collaboration_transfer_revision_and_deletion_authority(pool: PgPool) {
    let (state, alice, bob, channel) = setup(&pool).await;
    let p = create(&state, &alice, channel, "public").await;
    assert!(
        change(
            &state,
            &bob,
            &p,
            Change {
                operation: "profile".into(),
                name: Some("Foreign edit".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    plamenu::video_channels::change(
        &state,
        &alice,
        channel,
        &plamenu::video_channels::Change {
            operation: "invite".into(),
            username: Some("bob".into()),
            role: Some("editor".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!service::can_edit(&state, &p, bob.id).await.unwrap());
    plamenu::video_channels::change(
        &state,
        &bob,
        channel,
        &plamenu::video_channels::Change {
            operation: "accept".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let changed = change(
        &state,
        &bob,
        &p,
        Change {
            operation: "profile".into(),
            name: Some("Editor title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        change(
            &state,
            &alice,
            &p,
            Change {
                operation: "profile".into(),
                name: Some("Stale edit".into()),
                ..Default::default()
            }
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert_eq!(
        db::find(&pool, p.id).await.unwrap().unwrap().name,
        "Editor title"
    );
    plamenu::video_channels::change(
        &state,
        &alice,
        channel,
        &plamenu::video_channels::Change {
            operation: "transfer".into(),
            username: Some("bob".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    plamenu::video_channels::change(
        &state,
        &bob,
        channel,
        &plamenu::video_channels::Change {
            operation: "accept_transfer".into(),
            confirm: Some("films".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!service::can_edit(&state, &changed, alice.id).await.unwrap());
    assert!(service::can_edit(&state, &changed, bob.id).await.unwrap());
    let after = change(
        &state,
        &bob,
        &changed,
        Change {
            operation: "profile".into(),
            description: Some("After handoff".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(service::uri(&state, &p), service::uri(&state, &after));
    assert_eq!(p.uuid, after.uuid);
    plamenu::video_channels::change(
        &state,
        &bob,
        channel,
        &plamenu::video_channels::Change {
            operation: "delete".into(),
            confirm: Some("films".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db::find(&pool, p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn playlist_outbox_failure_rolls_back_create_edit_and_delete(pool: PgPool) {
    let (state, alice, _, channel) = setup(&pool).await;
    let viewer = RemoteUser::new("remote.test", "viewer");
    let viewer = plamenu::remote::store_remote_actor(&pool, &viewer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, channel, None)
        .await
        .unwrap();
    let p = create(&state, &alice, channel, "public").await;
    sqlx::query("CREATE FUNCTION reject_playlist_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected playlist outbox failure'; END; $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_playlist_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_playlist_delivery()").execute(&pool).await.unwrap();
    for input in [
        Change {
            operation: "profile".into(),
            name: Some("Failed edit".into()),
            ..Default::default()
        },
        Change {
            operation: "delete".into(),
            confirm: Some(p.name.clone()),
            ..Default::default()
        },
    ] {
        assert!(change(&state, &alice, &p, input).await.is_err());
    }
    let current = db::find(&pool, p.id).await.unwrap().unwrap();
    assert_eq!(current.name, p.name);
    assert_eq!(current.revision, p.revision);
    assert!(current.deleted_at.is_none());
    assert!(
        service::create(
            &state,
            &alice,
            &Change {
                channel_id: Some(channel.to_string()),
                name: Some("Failed create".into()),
                visibility: Some("public".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_playlists")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

fn remote_fixture() -> (
    std::sync::Arc<StubFederation>,
    RemoteUser,
    RemoteUser,
    String,
) {
    let owner = RemoteUser::new("remote.test", "author");
    let mut channel = RemoteUser::new("remote.test", "films");
    channel.actor.kind = "Group".into();
    channel.actor.attributed_to = Some(json!([owner.actor.id]));
    let stub = StubFederation::with_users(&[&owner, &channel]);
    let url = "https://remote.test/video-playlists/4".to_owned();
    stub.objects.lock().unwrap().insert(url.clone(),json!({"id":url,"type":"Playlist","uuid":"550e8400-e29b-41d4-a716-446655440000","name":"Remote films","content":"Description","published":"2026-09-28T00:00:00Z","updated":"2026-09-28T00:00:00Z","audience":channel.actor.id,"to":[plamenu_ap::activity::PUBLIC],"totalItems":2,"first":format!("{url}?page=1")}));
    stub.objects.lock().unwrap().insert(format!("{url}?page=1"),json!({"id":format!("{url}?page=1"),"type":"OrderedCollectionPage","partOf":url,"orderedItems":[format!("{url}/entries/1"),format!("{url}/entries/2")]}));
    for (n, start, stop) in [(1, 1, 3), (2, 5, 8)] {
        stub.objects.lock().unwrap().insert(format!("{url}/entries/{n}"),json!({"id":format!("{url}/entries/{n}"),"type":"PlaylistElement","url":"https://third.test/videos/unknown","position":n,"startTimestamp":start,"stopTimestamp":stop}));
    }
    (stub, owner, channel, url)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_metadata_only_update_stable_identity_and_delete_retention(pool: PgPool) {
    let (stub, owner, _, url) = remote_fixture();
    let state = test_state_with(pool.clone(), stub.clone());
    let p = remote::import(&state, &url, None).await.unwrap();
    let original = db::entries(&pool, p.id).await.unwrap();
    assert!(original.iter().all(|e| e.status_id.is_none()));
    assert!(stub.media_fetches.lock().unwrap().is_empty());
    assert!(!stub.fetches().iter().any(|u| u.contains("third.test")));
    let mut root = stub.objects.lock().unwrap().get(&url).unwrap().clone();
    root["updated"] = json!("2026-09-28T01:00:00Z");
    root["name"] = json!("Remote correction");
    stub.objects
        .lock()
        .unwrap()
        .insert(url.clone(), root.clone());
    stub.objects
        .lock()
        .unwrap()
        .get_mut(&format!("{url}/entries/1"))
        .unwrap()["position"] = json!(2);
    stub.objects
        .lock()
        .unwrap()
        .get_mut(&format!("{url}/entries/2"))
        .unwrap()["position"] = json!(1);
    let updated = remote::import(&state, &url, None).await.unwrap();
    assert_eq!(updated.id, p.id);
    assert_eq!(updated.name, "Remote correction");
    let reordered = db::entries(&pool, p.id).await.unwrap();
    assert_eq!(reordered[0].id, original[1].id);
    root["updated"] = json!("2026-09-28T00:00:00Z");
    root["name"] = json!("Stale title");
    stub.objects
        .lock()
        .unwrap()
        .insert(url.clone(), root.clone());
    assert_eq!(
        remote::import(&state, &url, None).await.unwrap().name,
        "Remote correction"
    );
    let actor = plamenu::remote::store_remote_actor(&pool, &owner.actor)
        .await
        .unwrap();
    assert!(remote::delete(&state, &actor, &url).await.unwrap());
    assert!(
        remote::import(&state, &url, None).await.is_err(),
        "a delayed Create with old canonical metadata cannot resurrect deletion"
    );
    assert!(db::entries(&pool, p.id).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_hostile_origins_loops_counts_clips_and_foreign_signers(pool: PgPool) {
    let (stub, _, _, url) = remote_fixture();
    let state = test_state_with(pool.clone(), stub.clone());
    let root = stub.objects.lock().unwrap().get(&url).unwrap().clone();
    for (key, value) in [
        ("uuid", json!("00000000-0000-0000-0000-00000000000Z")),
        ("totalItems", json!(1001)),
        ("first", json!("https://other.test/page")),
        ("audience", json!("https://other.test/group")),
        ("totalItems", json!(3)),
    ] {
        let mut bad = root.clone();
        bad[key] = value;
        stub.objects.lock().unwrap().insert(url.clone(), bad);
        assert!(remote::import(&state, &url, None).await.is_err());
    }
    stub.objects.lock().unwrap().insert(url.clone(), root);
    let page_url = format!("{url}?page=1");
    let page = stub.objects.lock().unwrap().get(&page_url).unwrap().clone();
    stub.objects.lock().unwrap().get_mut(&page_url).unwrap()["next"] = json!(page_url);
    assert!(remote::import(&state, &url, None).await.is_err());
    stub.objects.lock().unwrap().insert(page_url, page);
    let element = format!("{url}/entries/1");
    stub.objects.lock().unwrap().get_mut(&element).unwrap()["stopTimestamp"] = json!(1);
    assert!(remote::import(&state, &url, None).await.is_err());
    stub.objects.lock().unwrap().get_mut(&element).unwrap()["stopTimestamp"] = json!(3);
    let stranger = RemoteUser::new("remote.test", "stranger");
    let stranger = plamenu::remote::store_remote_actor(&pool, &stranger.actor)
        .await
        .unwrap();
    assert!(remote::import(&state, &url, Some(&stranger)).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_playlists")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let p = remote::import(&state, &url, None).await.unwrap();
    assert!(remote::delete(&state, &stranger, &url).await.unwrap());
    assert!(
        db::find(&pool, p.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
}
