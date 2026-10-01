//! Initial live announcements and independent recordings must not silently
//! drop creator-selected terms or content warnings. Scoped cookies stay scoped.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{actions::PostParams, video_metadata::Input};
use plamenu_db::{PgPool, native_live};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tower::ServiceExt;

fn options() -> native_live::PlaybackOptions {
    native_live::PlaybackOptions {
        dvr_window_seconds: 600,
        latency_mode: 1,
    }
}
fn selected() -> Input {
    Input {
        category: Some(json!(15)),
        licence: Some(json!(2)),
        spoiler_text: Some("Flashing test imagery".into()),
        sensitive: Some(json!(false)),
    }
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

async fn create(
    state: &plamenu::AppState,
    owner: &plamenu_db::account::Account,
    channel: i64,
    input: &Input,
) -> Result<plamenu::native_live::Created, plamenu::error::ApiError> {
    plamenu::native_live::create(
        state,
        owner,
        channel,
        "Warning-bearing live",
        "Test pattern",
        Some("en"),
        "public",
        false,
        options(),
        &[],
        input,
    )
    .await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn initial_announcement_metadata_is_validated_before_allocation_and_matches_create(
    pool: PgPool,
) {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let peer = common::RemoteUser::new("remote.test", "viewer");
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_users(&[&peer]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let viewer = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, channel.id, None)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, owner.id, None)
        .await
        .unwrap();
    for input in [
        Input {
            category: Some(json!(19)),
            ..Default::default()
        },
        Input {
            licence: Some(json!(-1)),
            ..Default::default()
        },
        Input {
            licence: Some(json!(10)),
            ..Default::default()
        },
        Input {
            spoiler_text: Some("a".repeat(501)),
            ..Default::default()
        },
    ] {
        assert!(create(&state, &owner, channel.id, &input).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM native_live_broadcasts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "invalid authoring fields must not allocate a broadcast or key"
        );
    }
    let created = create(&state, &owner, channel.id, &selected())
        .await
        .unwrap();
    assert!(created.status.sensitive);
    assert_eq!(created.status.spoiler_text, "Flashing test imagery");
    let wire = plamenu::note::note_for_status(&state, &created.status, &owner)
        .await
        .unwrap();
    assert_eq!(wire["category"]["identifier"], "15");
    assert_eq!(wire["licence"]["identifier"], "2");
    assert_eq!(
        wire["licence"]["url"],
        plamenu::video_metadata::licence_url(2).unwrap()
    );
    assert_eq!(wire["summary"], "Flashing test imagery");
    assert_eq!(wire["sensitive"], true);
    let activity:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create' AND activity->'object'->>'id'=$1 LIMIT 1")
        .bind(wire["id"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
    for key in ["category", "licence", "sensitive", "summary"] {
        assert_eq!(activity["object"][key], wire[key]);
    }
    let announced:Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Announce' AND activity->>'object'=$1 LIMIT 1")
        .bind(wire["id"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        announced["object"], wire["id"],
        "channel subscribers dereference the same labelled canonical Video"
    );
    let default = create(&state, &owner, channel.id, &Input::default())
        .await
        .unwrap();
    let wire = plamenu::note::note_for_status(&state, &default.status, &owner)
        .await
        .unwrap();
    assert!(wire["licence"].is_null() || wire["licence"]["identifier"] == "0");
    assert!(wire["category"].is_null() || wire["category"]["identifier"] == "0");
    assert!(!default.status.sensitive);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn recurring_replay_snapshots_terms_and_keeps_warnings_across_retries_and_new_sessions(
    pool: PgPool,
) {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let live = plamenu::native_live::create(
        &state,
        &owner,
        channel.id,
        "Reusable warning-bearing live",
        "Test pattern",
        Some("en"),
        "unlisted",
        true,
        options(),
        &[],
        &selected(),
    )
    .await
    .unwrap();
    let live_media: i64 =
        sqlx::query_scalar("SELECT media_id FROM video_publications WHERE status_id=$1")
            .bind(live.status.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let distribution = plamenu_db::video_distribution::Policy {
        media_id: live_media,
        download_enabled: false,
        embed_enabled: false,
        mirror_allowed: false,
    };
    plamenu_db::video_distribution::upsert(&pool, distribution)
        .await
        .unwrap();
    let first = native_live::claim_owned(&pool, live.status.id, owner.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        native_live::finish(&pool, &first, native_live::FinishReason::Stopped)
            .await
            .unwrap()
    );
    let replay = native_live::Replay {
        file_name: "episode.mp4",
        poster_name: "episode.jpg",
        file_size: 100,
        poster_size: 20,
        width: Some(320),
        height: Some(180),
        poster_width: 320,
        poster_height: 180,
        duration: 5.,
        frame_rate: Some("15"),
        bitrate: Some(1000),
        blurhash: None,
    };
    let keys = vec!["episode.mp4".into(), "episode.jpg".into()];
    assert!(
        native_live::reserve_replay(
            &pool,
            &first,
            &keys,
            &[100, 20],
            &plamenu_db::video_storage::Limits {
                per_account: 1000,
                global: 1000
            }
        )
        .await
        .unwrap()
    );
    let id = native_live::prepare_recurring_replay(&pool, &first, &replay)
        .await
        .unwrap()
        .unwrap();
    let metadata = plamenu_db::video_metadata::for_media(&pool, &[id])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(
        plamenu_db::video_distribution::for_media(&pool, &[id])
            .await
            .unwrap()[&id],
        plamenu_db::video_distribution::Policy {
            media_id: id,
            ..distribution
        }
    );
    assert_eq!(metadata.category, Some(15));
    assert_eq!(metadata.licence, Some(2));
    assert!(metadata.licence_version_known && metadata.sensitive);
    assert_eq!(metadata.spoiler_text, "Flashing test imagery");
    // A later session/parent clear must not erase an already prepared episode's
    // warning or turn its first Create into an unlabelled video on retry.
    let second = native_live::claim_owned(&pool, live.status.id, owner.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.session_id, second.session_id);
    sqlx::query("UPDATE statuses SET sensitive=false,spoiler_text='' WHERE id=$1")
        .bind(live.status.id)
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::video_distribution::upsert(
        &pool,
        plamenu_db::video_distribution::Policy::default_for(live_media),
    )
    .await
    .unwrap();
    assert_eq!(
        native_live::prepare_recurring_replay(&pool, &first, &replay)
            .await
            .unwrap(),
        Some(id)
    );
    assert_eq!(
        plamenu_db::video_distribution::for_media(&pool, &[id])
            .await
            .unwrap()[&id],
        plamenu_db::video_distribution::Policy {
            media_id: id,
            ..distribution
        }
    );
    let publication = native_live::recurring_replay_publication(&pool, &first)
        .await
        .unwrap()
        .unwrap();
    assert!(publication.sensitive);
    assert_eq!(publication.spoiler_text, "Flashing test imagery");
    let (posted, _) = plamenu::actions::post_recurring_live_replay(
        &state,
        PostParams {
            username: &publication.username,
            text: &publication.text,
            title: Some(&publication.title),
            media_ids: &[id],
            visibility: &publication.visibility,
            language: publication.language.as_deref(),
            spoiler_text: &publication.spoiler_text,
            sensitive: publication.sensitive,
            ..Default::default()
        },
        channel.id,
        &first.session_id,
    )
    .await
    .unwrap();
    assert_ne!(posted.id, live.status.id);
    assert!(posted.sensitive);
    let wire = plamenu::note::note_for_status(&state, &posted, &owner)
        .await
        .unwrap();
    assert_eq!(wire["licence"]["identifier"], "2");
    assert_eq!(wire["category"]["identifier"], "15");
    assert_eq!(wire["summary"], "Flashing test imagery");
    assert_eq!(wire["downloadEnabled"], false);
    assert_eq!(
        native_live::claim_owned(&pool, live.status.id, owner.id)
            .await
            .unwrap()
            .map(|s| s.session_id),
        None
    );
    sqlx::query("UPDATE statuses SET spoiler_text='Newer warning',sensitive=true WHERE id=$1")
        .bind(live.status.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        native_live::finish(&pool, &second, native_live::FinishReason::Stopped)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn html_creation_and_live_controls_respect_csrf_scopes_and_preserve_invalid_form(
    pool: PgPool,
) {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let user = plamenu_db::user::create(&pool, owner.id, Some("alice@example.test"), "unused")
        .await
        .unwrap();
    let web = plamenu::web::session::ensure_web_app(&state).await.unwrap();
    let app = plamenu::build_router(state.clone());
    let mut live_id = 0_i64;
    for scope in ["read", "write:media", "write:statuses", "read write"] {
        let raw = format!("live-{scope}");
        let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
        plamenu_db::oauth::create_token(
            &pool,
            &plamenu::auth::hash_secret(&raw),
            web.id,
            Some(user.id),
            scope,
        )
        .await
        .unwrap();
        let request = |path: String, body: String| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };
        let form = |csrf: &str, category: &str| {
            format!(
                "csrf={csrf}&channel={}&title=Preserved+live&description=Preserved+description&visibility=unlisted&category={category}&licence=2&spoiler_text=Flashing+test+imagery&permanent=yes",
                channel.id
            )
        };
        assert_eq!(
            app.clone()
                .oneshot(request("/web/videos/live".into(), form("wrong", "15")))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        if scope == "read write" {
            let invalid = app
                .clone()
                .oneshot(request("/web/videos/live".into(), form(&csrf, "99")))
                .await
                .unwrap();
            assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(invalid.headers()["Cache-Control"], "private, no-store");
            let body = text(invalid).await;
            for retained in [
                "value=\"Preserved live\"",
                "Preserved description",
                "value=\"Flashing test imagery\"",
                "role=\"alert\"",
            ] {
                assert!(body.contains(retained), "{retained}");
            }
            assert!(!body.contains("name=\"ingest_key\""));
            let valid = app
                .clone()
                .oneshot(request("/web/videos/live".into(), form(&csrf, "15")))
                .await
                .unwrap();
            assert_eq!(valid.status(), StatusCode::OK);
            assert_eq!(valid.headers()["Referrer-Policy"], "no-referrer");
            let body = text(valid).await;
            assert!(body.contains("Stream key"));
            live_id = sqlx::query_scalar("SELECT status_id FROM native_live_broadcasts")
                .fetch_one(&pool)
                .await
                .unwrap();
        } else {
            assert_eq!(
                app.clone()
                    .oneshot(request("/web/videos/live".into(), form(&csrf, "15")))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    assert!(live_id > 0);
    for (scope, rotation, schedule) in [
        ("read", StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
        ("write:media", StatusCode::OK, StatusCode::FORBIDDEN),
        (
            "write:statuses",
            StatusCode::FORBIDDEN,
            StatusCode::SEE_OTHER,
        ),
    ] {
        let raw = format!("live-{scope}");
        let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
        let request = |path: String, body: String| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request(
                    format!("/web/videos/live/{live_id}/rotate-key"),
                    format!("csrf={csrf}")
                ))
                .await
                .unwrap()
                .status(),
            rotation
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    format!("/web/videos/live/{live_id}/schedule"),
                    format!("csrf={csrf}")
                ))
                .await
                .unwrap()
                .status(),
            schedule
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    format!("/web/videos/live/{live_id}/playback"),
                    format!("csrf={csrf}&latency=balanced&dvr_minutes=5")
                ))
                .await
                .unwrap()
                .status(),
            schedule
        );
    }
    // Exercise an actual HTTP upgrade, not an oneshot request missing
    // hyper's OnUpgrade extension. No capture data or stream key is supplied.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for (scope, origin, allowed) in [
        ("read", state.config.domain.as_str(), false),
        ("write:statuses", state.config.domain.as_str(), false),
        ("write:media", "foreign.test", false),
        ("write:media", state.config.domain.as_str(), true),
    ] {
        let mut request = format!("ws://{addr}/web/videos/live/{live_id}/publish")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Cookie",
            format!("__Host-plamenu_session=live-{scope}")
                .parse()
                .unwrap(),
        );
        request
            .headers_mut()
            .insert("Origin", format!("https://{origin}").parse().unwrap());
        let result = tokio_tungstenite::connect_async(request).await;
        if allowed {
            let (mut socket, response) = result.unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            socket.close(None).await.unwrap();
        } else {
            let tokio_tungstenite::tungstenite::Error::Http(response) = result.unwrap_err() else {
                panic!("expected refused HTTP upgrade")
            };
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    server.abort();
    let _ = server.await;
}
