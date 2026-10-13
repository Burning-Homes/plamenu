mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    error::ApiError,
    music_assets,
    music_listening::{self, PreferenceChange, Progress, Session, Start},
};
use plamenu_db::{PgPool, id};
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Listen performer')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Listen recording',$3,'private',$4,$5,100,100,$6,10,'mp3')").bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    (state, owner, viewer, asset)
}
fn start(asset: i64, key: &str) -> Start {
    Start {
        remote_id: None,
        episode_id: None,
        asset_id: Some(asset.to_string()),
        status_id: None,
        queue_device: None,
        queue_entry_id: None,
        request_key: key.into(),
    }
}
async fn elapsed(state: &AppState, session: &Session, seconds: i32) {
    sqlx::query("UPDATE music_listen_sessions SET last_report_at=clock_timestamp()-$2::integer*interval '1 second' WHERE token=$1").bind(session.token.as_deref().unwrap()).bind(seconds).execute(&state.pool).await.unwrap();
}
async fn progress(
    state: &AppState,
    owner: i64,
    session: &Session,
    seconds: f64,
    finish: bool,
) -> Session {
    music_listening::progress(
        state,
        owner,
        session.token.as_deref().unwrap(),
        &Progress {
            played_seconds: seconds,
            finish,
        },
    )
    .await
    .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn real_time_threshold_replays_and_finished_sessions_count_once(pool: PgPool) {
    let (state, owner, _, asset) = fixture(pool).await;
    let first = music_listening::start(&state, owner, &start(asset, "first-play"))
        .await
        .unwrap();
    let repeat = music_listening::start(&state, owner, &start(asset, "first-play"))
        .await
        .unwrap();
    assert!(
        first
            .token
            .as_ref()
            .is_some_and(|token| Some(token) == repeat.token.as_ref())
    );
    assert!(!first.counted);
    assert!(
        music_listening::history(&state, owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // A claimed seek cannot manufacture elapsed wall time.
    let immediate = progress(&state, owner, &first, 9.0, false).await;
    assert!(!immediate.counted);
    assert!(immediate.played_seconds < 1.0);
    let fresh = music_listening::start(&state, owner, &start(asset, "real-play"))
        .await
        .unwrap();
    elapsed(&state, &fresh, 4).await;
    let partial = progress(&state, owner, &fresh, 4.0, false).await;
    assert!(!partial.counted);
    elapsed(&state, &partial, 3).await;
    let counted = progress(&state, owner, &partial, 6.0, false).await;
    assert!(counted.counted);
    let page = music_listening::history(&state, owner, None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let id = page.items[0].id.clone();
    elapsed(&state, &counted, 20).await;
    let replay = progress(&state, owner, &counted, 6.0, false).await;
    assert!((replay.played_seconds - counted.played_seconds).abs() < f64::EPSILON);
    let finished = progress(&state, owner, &replay, 8.0, true).await;
    assert!(finished.finished);
    assert!(finished.counted);
    elapsed(&state, &finished, 20).await;
    let replay = progress(&state, owner, &finished, 999.0, true).await;
    assert!((replay.played_seconds - finished.played_seconds).abs() < f64::EPSILON);
    let page = music_listening::history(&state, owner, None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn private_history_resolves_current_grants_blocks_and_account_retirement(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    assert!(matches!(
        music_listening::start(&state, viewer, &start(asset, "no-access")).await,
        Err(ApiError::NotFound)
    ));
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    let session = music_listening::start(&state, viewer, &start(asset, "shared-play"))
        .await
        .unwrap();
    elapsed(&state, &session, 10).await;
    let session = progress(&state, viewer, &session, 8.0, false).await;
    assert!(session.counted);
    assert!(matches!(
        music_listening::progress(
            &state,
            owner,
            session.token.as_deref().unwrap(),
            &Progress {
                played_seconds: 8.0,
                finish: true
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    assert_eq!(
        music_listening::history(&state, viewer, None)
            .await
            .unwrap()
            .items[0]
            .track
            .title,
        "Listen recording"
    );
    music_assets::grant(&state, owner, asset, viewer, false)
        .await
        .unwrap();
    let page = music_listening::history(&state, viewer, None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert!(page.before.is_none());
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("Listen recording")
    );
    assert!(matches!(
        music_listening::progress(
            &state,
            viewer,
            session.token.as_deref().unwrap(),
            &Progress {
                played_seconds: 8.0,
                finish: true
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        music_listening::history(&state, viewer, None)
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
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_listen_sessions WHERE account_id=$1"
        )
        .bind(viewer)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        0
    );
    assert!(matches!(
        music_listening::history(&state, viewer, None).await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn opt_out_and_erasure_cancel_pending_reports_and_thresholds_are_revisioned(pool: PgPool) {
    let (state, owner, _, asset) = fixture(pool).await;
    let session = music_listening::start(&state, owner, &start(asset, "pending"))
        .await
        .unwrap();
    let initial = music_listening::preferences(&state, owner).await.unwrap();
    assert_eq!(initial.revision, 0);
    let off = music_listening::set_preferences(
        &state,
        owner,
        &PreferenceChange {
            revision: "0".into(),
            history_enabled: false,
            minimum_seconds: 30,
            minimum_percent: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(off.revision, 1);
    assert!(!off.history_enabled);
    elapsed(&state, &session, 10).await;
    assert!(matches!(
        music_listening::progress(
            &state,
            owner,
            session.token.as_deref().unwrap(),
            &Progress {
                played_seconds: 9.0,
                finish: true
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    assert!(
        !music_listening::start(&state, owner, &start(asset, "disabled"))
            .await
            .unwrap()
            .enabled
    );
    let on = music_listening::set_preferences(
        &state,
        owner,
        &PreferenceChange {
            revision: off.revision.to_string(),
            history_enabled: true,
            minimum_seconds: 2,
            minimum_percent: 100,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        music_listening::set_preferences(
            &state,
            owner,
            &PreferenceChange {
                revision: "1".into(),
                history_enabled: true,
                minimum_seconds: 2,
                minimum_percent: 100
            }
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert_eq!(serde_json::to_value(&on).unwrap()["revision"], "2");
    let session = music_listening::start(&state, owner, &start(asset, "enabled"))
        .await
        .unwrap();
    elapsed(&state, &session, 3).await;
    assert!(progress(&state, owner, &session, 3.0, false).await.counted);
    music_listening::erase(&state, owner).await.unwrap();
    assert!(
        music_listening::history(&state, owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(matches!(
        music_listening::progress(
            &state,
            owner,
            session.token.as_deref().unwrap(),
            &Progress {
                played_seconds: 4.0,
                finish: true
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn queue_starts_are_owned_and_nonfinite_reports_cannot_create_history(pool: PgPool) {
    use plamenu::music_queues::{self, Action, Change, Source};
    let (state, owner, viewer, asset) = fixture(pool).await;
    let queue = music_queues::change(
        &state,
        owner,
        "laptop",
        &Change {
            revision: "0".into(),
            action: Action::Add {
                sources: vec![Source {
                    remote_id: None,
                    episode_id: None,
                    asset_id: Some(asset.to_string()),
                    status_id: None,
                }],
            },
        },
    )
    .await
    .unwrap();
    let input = Start {
        remote_id: None,
        episode_id: None,
        request_key: "queue-play".into(),
        asset_id: None,
        status_id: None,
        queue_device: Some(queue.device),
        queue_entry_id: Some(queue.entries[0].entry_id.clone()),
    };
    assert!(matches!(
        music_listening::start(&state, viewer, &input).await,
        Err(ApiError::NotFound)
    ));
    let session = music_listening::start(&state, owner, &input).await.unwrap();
    for seconds in [-1.0, f64::NAN, f64::INFINITY, 604_801.0] {
        assert!(matches!(
            music_listening::progress(
                &state,
                owner,
                session.token.as_deref().unwrap(),
                &Progress {
                    played_seconds: seconds,
                    finish: false
                }
            )
            .await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    assert!(
        music_listening::history(&state, owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    music_assets::delete(&state, owner, asset).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_listen_sessions")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn listening_api_requires_explicit_write_scope_and_protects_private_results(pool: PgPool) {
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
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "listening-tests",
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
    for (t, scopes) in [(&read, "read:statuses"), (&write, "write:statuses")] {
        oauth::create_token(&state.pool, &hash_secret(t), app.id, Some(u.id), scopes)
            .await
            .unwrap();
    }
    let app = plamenu::build_router(state.clone());
    for path in [
        "/api/v1/music/history",
        "/api/v1/music/listening/preferences",
    ] {
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
    }
    let input = serde_json::json!({"request_key":"api-explicit-play","asset_id":asset.to_string()})
        .to_string();
    for (token, expected) in [(&read, StatusCode::FORBIDDEN), (&write, StatusCode::OK)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/music/listening/sessions")
                    .method("POST")
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(input.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            let v: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["counted"], false);
            assert!(v["token"].as_str().is_some_and(|t| t.len() == 64));
        }
    }
    assert!(
        music_listening::history(&state, owner, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
