mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    error::ApiError,
    music_assets,
    music_queues::{self, Action, Change, Queue, Source},
};
use plamenu_db::{PgPool, id};

async fn fixture(pool: PgPool) -> (AppState, i64, i64) {
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
    let source = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,$3)")
        .bind(artist)
        .bind(owner)
        .bind(format!("Performer {artist}"))
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,$3,$4,$5,$6,$7,100,100,$8,10,'mp3')").bind(source).bind(owner).bind(title).bind(artist).bind(visibility).bind(format!("music{source}.orig")).bind(format!("music{source}.mp3")).bind(format!("{source:064x}")).execute(&state.pool).await.unwrap();
    source
}
fn source(id: i64) -> Source {
    Source {
        remote_id: None,
        episode_id: None,
        asset_id: Some(id.to_string()),
        status_id: None,
    }
}
async fn change(state: &AppState, owner: i64, queue: &Queue, action: Action) -> Queue {
    music_queues::change(
        state,
        owner,
        &queue.device,
        &Change {
            revision: queue.revision.clone(),
            action,
        },
    )
    .await
    .unwrap()
}
fn ids(queue: &Queue) -> Vec<String> {
    queue.entries.iter().map(|e| e.entry_id.clone()).collect()
}

#[allow(clippy::too_many_lines)] // One lifecycle journey checks the transitions together.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn queues_restore_original_order_repeat_positions_and_device_isolation(pool: PgPool) {
    let (state, alice, bob) = fixture(pool).await;
    let a = asset(&state, alice, "First track", "private").await;
    let b = asset(&state, alice, "Second track", "private").await;
    let q = music_queues::get(&state, alice, "laptop").await.unwrap();
    assert_eq!(q.revision, "0");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_queues")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0,
        "Reading an empty player must not create a device"
    );
    let q = change(
        &state,
        alice,
        &q,
        Action::Add {
            sources: vec![source(a), source(b), source(a)],
        },
    )
    .await;
    assert_eq!(q.revision, "1");
    assert_eq!(q.entries.len(), 3);
    let original = ids(&q);
    assert_eq!(
        original
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3,
        "Duplicate tracks need distinct queue entry identities"
    );
    let q = change(
        &state,
        alice,
        &q,
        Action::Select {
            entry_id: original[1].clone(),
        },
    )
    .await;
    let q = change(
        &state,
        alice,
        &q,
        Action::Seek {
            seconds: 999.0,
            entry_id: original[1].clone(),
        },
    )
    .await;
    assert!((q.position_seconds - 10.0).abs() < f64::EPSILON);
    let q = change(
        &state,
        alice,
        &q,
        Action::Settings {
            repeat_mode: "one".into(),
            shuffle: true,
        },
    )
    .await;
    let loaded = music_queues::get(&state, alice, "laptop").await.unwrap();
    assert_eq!(ids(&q), ids(&loaded));
    assert_eq!(loaded.current_entry_id, Some(original[1].clone()));
    assert!((loaded.position_seconds - 10.0).abs() < f64::EPSILON);
    let q = change(&state, alice, &q, Action::Next { ended: true }).await;
    assert_eq!(q.current_entry_id, Some(original[1].clone()));
    assert!((q.position_seconds - 0.0).abs() < f64::EPSILON);
    let q = change(
        &state,
        alice,
        &q,
        Action::Settings {
            repeat_mode: "off".into(),
            shuffle: false,
        },
    )
    .await;
    assert_eq!(ids(&q), original);
    let q = change(&state, alice, &q, Action::Next { ended: false }).await;
    assert_eq!(q.current_entry_id, Some(original[2].clone()));
    let q = change(&state, alice, &q, Action::Next { ended: true }).await;
    assert!(q.current_entry_id.is_none());
    let q = change(
        &state,
        alice,
        &q,
        Action::Settings {
            repeat_mode: "all".into(),
            shuffle: false,
        },
    )
    .await;
    let q = change(
        &state,
        alice,
        &q,
        Action::Select {
            entry_id: original[0].clone(),
        },
    )
    .await;
    let q = change(&state, alice, &q, Action::Previous).await;
    assert_eq!(q.current_entry_id, Some(original[2].clone()));
    let q = change(&state, alice, &q, Action::Next { ended: true }).await;
    assert_eq!(q.current_entry_id, Some(original[0].clone()));
    assert!(
        music_queues::get(&state, bob, "laptop")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        music_queues::get(&state, alice, "phone")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0,
        "Playing retained music must not publish it"
    );
}
#[allow(clippy::too_many_lines)] // One lifecycle journey checks the transitions together.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn queues_never_retain_private_metadata_after_grants_blocks_or_source_retirement(
    pool: PgPool,
) {
    let (state, alice, bob) = fixture(pool).await;
    let a = asset(&state, alice, "Secret shared title", "private").await;
    let b = asset(&state, alice, "Open recording", "public").await;
    let q = music_queues::get(&state, bob, "phone").await.unwrap();
    assert!(matches!(
        music_queues::change(
            &state,
            bob,
            "phone",
            &Change {
                revision: q.revision.clone(),
                action: Action::Add {
                    sources: vec![source(a), source(b)]
                }
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_queue_entries")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0,
        "An invalid mixed batch must roll back every source and device"
    );
    music_assets::grant(&state, alice, a, bob, true)
        .await
        .unwrap();
    let q = change(
        &state,
        bob,
        &q,
        Action::Add {
            sources: vec![source(a), source(b)],
        },
    )
    .await;
    let original = ids(&q);
    let q = change(
        &state,
        bob,
        &q,
        Action::Select {
            entry_id: original[0].clone(),
        },
    )
    .await;
    let q = change(
        &state,
        bob,
        &q,
        Action::Seek {
            seconds: 6.0,
            entry_id: original[0].clone(),
        },
    )
    .await;
    music_assets::grant(&state, alice, a, bob, false)
        .await
        .unwrap();
    let hidden = music_queues::get(&state, bob, "phone").await.unwrap();
    assert_eq!(hidden.entries.len(), 1);
    assert!(hidden.current_entry_id.is_none());
    assert!((hidden.position_seconds - 0.0).abs() < f64::EPSILON);
    let json = serde_json::to_string(&hidden).unwrap();
    assert!(!json.contains("Secret shared title"));
    assert!(!json.contains(&a.to_string()));
    assert!(matches!(
        music_queues::change(
            &state,
            bob,
            "phone",
            &Change {
                revision: q.revision.clone(),
                action: Action::Select {
                    entry_id: original[0].clone()
                }
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    plamenu_db::block::create(&state.pool, bob, alice, None)
        .await
        .unwrap();
    assert!(
        music_queues::get(&state, bob, "phone")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    sqlx::query("DELETE FROM blocks")
        .execute(&state.pool)
        .await
        .unwrap();
    let q = music_queues::get(&state, bob, "phone").await.unwrap();
    let q = change(
        &state,
        bob,
        &q,
        Action::Select {
            entry_id: original[1].clone(),
        },
    )
    .await;
    music_assets::delete(&state, alice, b).await.unwrap();
    let q = music_queues::get(&state, bob, &q.device).await.unwrap();
    assert!(q.current_entry_id.is_none());
    assert!(q.entries.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_queue_entries WHERE asset_id=$1")
            .bind(b)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[allow(clippy::too_many_lines)] // One lifecycle journey checks the transitions together.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn concurrent_first_writes_and_stale_seeks_cannot_replace_another_selection(pool: PgPool) {
    let (state, alice, _) = fixture(pool).await;
    let a = asset(&state, alice, "Concurrent recording", "private").await;
    let first = Change {
        revision: "0".into(),
        action: Action::Add {
            sources: vec![source(a)],
        },
    };
    let second = Change {
        revision: "0".into(),
        action: Action::Add {
            sources: vec![source(a)],
        },
    };
    let (a, b) = tokio::join!(
        music_queues::change(&state, alice, "desktop", &first),
        music_queues::change(&state, alice, "desktop", &second)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert!(matches!(
        a.as_ref().err().or_else(|| b.as_ref().err()),
        Some(ApiError::Conflict(_))
    ));
    let q = music_queues::get(&state, alice, "desktop").await.unwrap();
    assert_eq!(q.entries.len(), 1);
    assert_eq!(q.revision, "1");
    let entry = q.entries[0].entry_id.clone();
    let q = change(
        &state,
        alice,
        &q,
        Action::Select {
            entry_id: entry.clone(),
        },
    )
    .await;
    let q = change(&state, alice, &q, Action::Clear).await;
    assert!(matches!(
        music_queues::change(
            &state,
            alice,
            "desktop",
            &Change {
                revision: q.revision.clone(),
                action: Action::Seek {
                    seconds: 3.0,
                    entry_id: entry
                }
            }
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert_eq!(
        music_queues::get(&state, alice, "desktop")
            .await
            .unwrap()
            .revision,
        q.revision
    );
}
#[allow(clippy::too_many_lines)] // One lifecycle journey checks the transitions together.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn collection_queue_import_filters_every_source_and_queue_order_is_atomic(pool: PgPool) {
    use plamenu::music_collections::{self, Add, New};
    let (state, alice, bob) = fixture(pool).await;
    let private = asset(&state, alice, "Playlist secret", "private").await;
    let public = asset(&state, alice, "Playlist public", "public").await;
    let collection = music_collections::create(
        &state,
        alice,
        &New {
            name: "Shared playlist".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    for id in [private, public, public] {
        let c = music_collections::get(&state, collection.id, Some(alice))
            .await
            .unwrap();
        music_collections::add(
            &state,
            alice,
            c.id,
            &Add {
                remote_id: None,
                episode_id: None,
                asset_id: Some(id.to_string()),
                status_id: None,
                revision: c.revision.to_string(),
            },
        )
        .await
        .unwrap();
    }
    let c = music_collections::get(&state, collection.id, Some(alice))
        .await
        .unwrap();
    music_collections::grant(&state, alice, c.id, c.revision, bob, true)
        .await
        .unwrap();
    let q = music_queues::get(&state, bob, "web").await.unwrap();
    let q = change(
        &state,
        bob,
        &q,
        Action::Collection {
            collection_id: c.id.to_string(),
            replace: true,
        },
    )
    .await;
    assert_eq!(q.entries.len(), 2);
    assert!(q.entries.iter().all(|p| p.title == "Playlist public"));
    let original = ids(&q);
    let q = change(
        &state,
        bob,
        &q,
        Action::Move {
            entry_id: original[1].clone(),
            direction: "up".into(),
        },
    )
    .await;
    assert_eq!(ids(&q), vec![original[1].clone(), original[0].clone()]);
    let invalid = Change {
        revision: q.revision.clone(),
        action: Action::Order {
            entry_ids: vec![original[0].clone(), original[0].clone()],
        },
    };
    assert!(matches!(
        music_queues::change(&state, bob, "web", &invalid).await,
        Err(ApiError::Unprocessable(_))
    ));
    assert_eq!(
        ids(&music_queues::get(&state, bob, "web").await.unwrap()),
        ids(&q)
    );
    let q = change(
        &state,
        bob,
        &q,
        Action::Order {
            entry_ids: original.clone(),
        },
    )
    .await;
    assert_eq!(ids(&q), original);
    music_collections::delete(
        &state,
        alice,
        c.id,
        music_collections::get(&state, c.id, Some(alice))
            .await
            .unwrap()
            .revision,
    )
    .await
    .unwrap();
    let denied = Change {
        revision: q.revision.clone(),
        action: Action::Collection {
            collection_id: c.id.to_string(),
            replace: true,
        },
    };
    assert!(matches!(
        music_queues::change(&state, bob, "web", &denied).await,
        Err(ApiError::NotFound)
    ));
    assert_eq!(
        ids(&music_queues::get(&state, bob, "web").await.unwrap()),
        original,
        "Failed replacement must keep the existing queue"
    );
}
#[allow(clippy::too_many_lines)] // One lifecycle journey checks the transitions together.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn queue_inputs_are_bounded_and_account_lifecycle_remains_authoritative(pool: PgPool) {
    let (state, alice, _) = fixture(pool).await;
    let a = asset(&state, alice, "Bounded source", "private").await;
    for device in ["", "../phone", "space device"] {
        assert!(music_queues::get(&state, alice, device).await.is_err());
    }
    for sources in [
        vec![],
        vec![source(a); 1001],
        vec![Source {
            remote_id: None,
            episode_id: None,
            asset_id: Some(a.to_string()),
            status_id: Some(a.to_string()),
        }],
    ] {
        assert!(matches!(
            music_queues::change(
                &state,
                alice,
                "web",
                &Change {
                    revision: "0".into(),
                    action: Action::Add { sources }
                }
            )
            .await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    let q = music_queues::get(&state, alice, "web").await.unwrap();
    let q = change(
        &state,
        alice,
        &q,
        Action::Add {
            sources: vec![source(a)],
        },
    )
    .await;
    let entry = q.entries[0].entry_id.clone();
    let q = change(
        &state,
        alice,
        &q,
        Action::Select {
            entry_id: entry.clone(),
        },
    )
    .await;
    for seconds in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            music_queues::change(
                &state,
                alice,
                "web",
                &Change {
                    revision: q.revision.clone(),
                    action: Action::Seek {
                        seconds,
                        entry_id: entry.clone()
                    }
                }
            )
            .await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    assert_eq!(music_queues::devices(&state, alice).await.unwrap().len(), 1);
    let forgotten = change(&state, alice, &q, Action::Forget).await;
    assert_eq!(forgotten.revision, "0");
    assert!(forgotten.entries.is_empty());
    assert!(
        music_queues::devices(&state, alice)
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(alice)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_queues::get(&state, alice, "web").await,
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        music_queues::change(
            &state,
            alice,
            "web",
            &Change {
                revision: q.revision,
                action: Action::Clear
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
}

async fn tokens(state: &AppState, owner: i64) -> (String, String) {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let u = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("test-password").unwrap(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "music-queue-tests",
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
    for (t, scope) in [(&read, "read:statuses"), (&write, "write:statuses")] {
        oauth::create_token(&state.pool, &hash_secret(t), app.id, Some(u.id), scope)
            .await
            .unwrap();
    }
    (read, write)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn queue_api_keeps_scopes_revisions_and_cache_private(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (state, alice, bob) = fixture(pool).await;
    let a = asset(&state, alice, "API queue title", "private").await;
    let (read, write) = tokens(&state, alice).await;
    let q = music_queues::get(&state, alice, "web").await.unwrap();
    let q = change(
        &state,
        alice,
        &q,
        Action::Add {
            sources: vec![source(a)],
        },
    )
    .await;
    let app = plamenu::build_router(state.clone());
    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(&write), StatusCode::FORBIDDEN),
        (Some(&read), StatusCode::OK),
    ] {
        let mut req = Request::builder().uri("/api/v1/music/queues/web");
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            let v: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["revision"], "1");
            assert_eq!(v["entries"][0]["title"], "API queue title");
        }
    }
    let body=serde_json::json!({"revision":q.revision,"action":"select","entry_id":q.entries[0].entry_id}).to_string();
    for (token, expected) in [
        (&read, StatusCode::FORBIDDEN),
        (&write, StatusCode::OK),
        (&write, StatusCode::CONFLICT),
    ] {
        let req = Request::builder()
            .uri("/api/v1/music/queues/web")
            .method("POST")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), expected);
    }
    assert!(
        music_queues::get(&state, bob, "web")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn audio_status_queues_preserve_origin_and_hide_retracted_publications(pool: PgPool) {
    use common::RemoteUser;
    use plamenu::{
        actions::{self, PostParams},
        ingest,
    };
    use plamenu_ap::activity::PostKind;
    use plamenu_db::media::{self, NewLocalMedia};
    let (state, alice, bob) = fixture(pool).await;
    let alice_account = plamenu_db::account::find_by_id(&state.pool, alice)
        .await
        .unwrap()
        .unwrap();
    let (note, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "An ordinary Note",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let q = music_queues::get(&state, bob, "web").await.unwrap();
    assert!(matches!(
        music_queues::change(
            &state,
            bob,
            "web",
            &Change {
                revision: q.revision.clone(),
                action: Action::Add {
                    sources: vec![Source {
                        remote_id: None,
                        episode_id: None,
                        asset_id: None,
                        status_id: Some(note.id.to_string())
                    }]
                }
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    let mut attachment = NewLocalMedia::new(alice, id::next(), "queue.mp3", "audio/mpeg");
    attachment.kind = "audio";
    attachment.duration = Some(9.0);
    let media_id = media::create_local(&state.pool, attachment)
        .await
        .unwrap()
        .id;
    let (audio, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "Origin audio",
            title: Some("Published track"),
            visibility: "public",
            kind: PostKind::Audio,
            media_ids: &[media_id],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let remote_account = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    let document = serde_json::json!({"id":"https://songhive.test/ap/audio/queue-track","type":"Audio","attributedTo":remote.actor.id,"name":"Remote queue recording","to":["https://www.w3.org/ns/activitystreams#Public"],"url":[{"type":"Link","mediaType":"audio/mpeg","href":"https://songhive.test/track.mp3"}],"track":{"name":"Remote queue recording","artists":[{"name":"Remote performer"}]}});
    let remote_audio = ingest::ingest_remote_note(&state, &remote_account, &document)
        .await
        .unwrap();
    let q = change(
        &state,
        bob,
        &q,
        Action::Add {
            sources: vec![
                Source {
                    remote_id: None,
                    episode_id: None,
                    asset_id: None,
                    status_id: Some(audio.id.to_string()),
                },
                Source {
                    remote_id: None,
                    episode_id: None,
                    asset_id: None,
                    status_id: Some(remote_audio.id.to_string()),
                },
            ],
        },
    )
    .await;
    assert_eq!(q.entries.len(), 2);
    assert!(q.entries[1].url.starts_with("https://songhive.test/"));
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE account_id=$1")
        .bind(bob)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "Queuing remote Audio must never manufacture a local publication"
    );
    actions::delete_status(&state, &alice_account, audio.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    let q = music_queues::get(&state, bob, "web").await.unwrap();
    assert_eq!(q.entries.len(), 1);
    assert_eq!(q.entries[0].title, "Remote queue recording");
    plamenu_db::block::create(&state.pool, bob, remote_account.id, None)
        .await
        .unwrap();
    assert!(
        music_queues::get(&state, bob, "web")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
