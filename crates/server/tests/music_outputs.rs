mod common;
use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{
    error::ApiError,
    music_outputs::{self, Command, New},
};
use plamenu_db::PgPool;
fn input(mount: &str, visibility: &str) -> New {
    New {
        mount: mount.into(),
        name: "Listening room".into(),
        description: "Native broadcast".into(),
        genre: "Ambient".into(),
        format: "mp3".into(),
        bitrate: Some(128),
        sample_rate: 44100,
        visibility: visibility.into(),
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn output_management_is_revisioned_private_tokens_are_one_time_and_retirement_revokes_access(
    pool: PgPool,
) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("listening-room", "private"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    let token = created.listen_token.unwrap();
    assert!(
        !serde_json::to_string(&created.output)
            .unwrap()
            .contains(&token)
    );
    assert!(matches!(
        music_outputs::get(&state, id, Some(bob.id)).await,
        Err(ApiError::NotFound)
    ));
    assert!(
        music_outputs::list(&state, None, false, None, "")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        music_outputs::authorize(&state, id, None).await,
        Err(ApiError::NotFound)
    ));
    let current = music_outputs::authorize(&state, id, Some(&token))
        .await
        .unwrap();
    assert_ne!(current.listen_token_hash.as_deref(), Some(token.as_str()));
    assert!(matches!(
        music_outputs::command(
            &state,
            bob.id,
            id,
            &Command {
                revision: current.revision.to_string(),
                operation: "play".into()
            }
        )
        .await,
        Err(ApiError::NotFound)
    ));
    let playing = music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: current.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(playing.transport, "playing");
    assert!(matches!(
        music_outputs::command(
            &state,
            alice.id,
            id,
            &Command {
                revision: current.revision.to_string(),
                operation: "pause".into()
            }
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    let rotated = music_outputs::rotate_token(&state, alice.id, id, playing.revision)
        .await
        .unwrap();
    assert!(matches!(
        music_outputs::authorize(&state, id, Some(&token)).await,
        Err(ApiError::NotFound)
    ));
    let current = music_outputs::authorize(&state, id, rotated.listen_token.as_deref())
        .await
        .unwrap();
    let disabled = music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: current.revision.to_string(),
            operation: "disable".into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        music_outputs::authorize(&state, id, rotated.listen_token.as_deref()).await,
        Err(ApiError::NotFound)
    ));
    let updated = music_outputs::update(
        &state,
        alice.id,
        id,
        disabled.revision,
        &input("public-room", "public"),
    )
    .await
    .unwrap();
    assert!(updated.listen_token.is_none());
    let enabled = music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: updated.output["revision"].as_str().unwrap().into(),
            operation: "enable".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        music_outputs::authorize(&state, id, None).await.unwrap().id,
        id
    );
    assert!(
        music_outputs::get(&state, id, None)
            .await
            .unwrap()
            .entity(None)
            .get("device")
            .is_none()
    );
    assert!(matches!(
        music_outputs::create(&state, bob.id, &input("public-room", "public")).await,
        Err(ApiError::Conflict(_))
    ));
    sqlx::query("UPDATE accounts SET deleted_at=clock_timestamp() WHERE id=$1")
        .bind(alice.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_outputs::authorize(&state, id, None).await,
        Err(ApiError::NotFound)
    ));
    assert!(
        music_outputs::get(&state, enabled.id, Some(alice.id))
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn output_worker_lease_is_single_flight_and_transport_or_token_generation_fences_old_worker(
    pool: PgPool,
) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("lease-room", "private"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    let row = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    assert!(
        music_outputs::worker::claim(&state, id)
            .await
            .unwrap()
            .is_none()
    );
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        music_outputs::worker::claim(&state, id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        music_outputs::worker::heartbeat(&state, &claimed)
            .await
            .unwrap()
    );
    assert!(matches!(
        music_outputs::worker::selected(&state, &claimed).await,
        Err(ApiError::NotFound)
    ));
    sqlx::query("UPDATE music_outputs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    let successor = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    assert!(successor.generation > claimed.generation);
    assert_ne!(successor.worker_lease, claimed.worker_lease);
    assert!(
        !music_outputs::worker::heartbeat(&state, &claimed)
            .await
            .unwrap()
    );
    assert!(
        music_outputs::worker::heartbeat(&state, &successor)
            .await
            .unwrap()
    );
    let current = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    music_outputs::rotate_token(&state, alice.id, id, current.revision)
        .await
        .unwrap();
    assert!(
        !music_outputs::worker::heartbeat(&state, &successor)
            .await
            .unwrap()
    );
    let third = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    let current = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: current.revision.to_string(),
            operation: "pause".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        !music_outputs::worker::heartbeat(&state, &third)
            .await
            .unwrap()
    );
    assert!(
        music_outputs::worker::claim(&state, id)
            .await
            .unwrap()
            .is_none()
    );
}

async fn real_asset(state: &plamenu::AppState, owner: i64) -> i64 {
    real_asset_duration(state, owner, 0.4).await
}
async fn real_asset_duration(state: &plamenu::AppState, owner: i64, duration: f64) -> i64 {
    let asset = plamenu_db::id::next();
    let name = format!("music{asset}.orig");
    let path = state.config.media_dir.join(&name);
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=437:duration={duration}"),
                "-c:a",
                "pcm_s16le",
                "-f",
                "wav",
                "-y"
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let bytes = i64::try_from(std::fs::metadata(&path).unwrap().len()).unwrap();
    let artist:i64=sqlx::query_scalar("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Broadcast artist') ON CONFLICT(owner_id,name) DO UPDATE SET name=EXCLUDED.name RETURNING id")
        .bind(plamenu_db::id::next())
        .bind(owner)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,'Broadcast tone',$3,'public',$4,$5,'wav',$6,1,$7,$8)")
        .bind(asset).bind(owner).bind(artist).bind(&name).bind(format!("music{asset}.mp3")).bind(bytes).bind(format!("{asset:064x}")).bind(duration).execute(&state.pool).await.unwrap();
    asset
}
async fn real_output_codec(pool: PgPool, codec: &str) {
    real_output_source(pool, codec, "asset").await;
}
async fn origin_source(
    state: &plamenu::AppState,
    stub: &StubFederation,
    owner: i64,
    asset: i64,
    kind: &str,
) -> plamenu::music_queues::Source {
    use plamenu::music_queues::Source;
    use plamenu_federation::FetchedMedia;
    let url = "https://origin.example/tone.wav";
    let bytes = std::fs::read(state.config.media_dir.join(format!("music{asset}.orig"))).unwrap();
    stub.media.lock().unwrap().insert(
        url.into(),
        FetchedMedia {
            final_url: url.into(),
            content_type: "audio/wav".into(),
            bytes,
        },
    );
    *stub.enclosure_chunking.lock().unwrap() = Some((2048, 1));
    if kind == "episode" {
        let show = plamenu_db::id::next();
        let episode = plamenu_db::id::next();
        sqlx::query("INSERT INTO music_podcasts(id,feed_url,title) VALUES($1,'https://origin.example/feed.xml','Show')").bind(show).execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO music_podcast_episodes(id,podcast_id,guid,title,audio_url,audio_type) VALUES($1,$2,'one','Tone',$3,'audio/wav')").bind(episode).bind(show).bind(url).execute(&state.pool).await.unwrap();
        return Source {
            asset_id: None,
            status_id: None,
            remote_id: None,
            episode_id: Some(episode.to_string()),
        };
    }
    let publisher = common::RemoteUser::new("origin.example", "publisher");
    stub.actors
        .lock()
        .unwrap()
        .insert(publisher.actor.id.clone(), publisher.actor.clone());
    let uri = "https://origin.example/audio/one";
    stub.objects.lock().unwrap().insert(uri.into(),serde_json::json!({"id":uri,"type":"Audio","attributedTo":publisher.actor.id,"to":[plamenu_ap::activity::PUBLIC],"name":"Tone","attachment":[{"type":"Audio","mediaType":"audio/wav","url":url}]}));
    let remote = plamenu::music_remote::import(state, owner, uri)
        .await
        .unwrap();
    Source {
        asset_id: None,
        status_id: None,
        remote_id: Some(remote.id.to_string()),
        episode_id: None,
    }
}
async fn real_output_source(pool: PgPool, codec: &str, kind: &str) {
    use plamenu::{
        AppState,
        music_queues::{self, Action, Change, Source},
    };
    use std::sync::Arc;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = dir.path().join("media");
    let store = Arc::new(plamenu::storage::LocalDiskStore::new(config.media_dir.clone()).unwrap());
    let stub = StubFederation::with_actors([]);
    let state = AppState::new(pool, config, stub.clone(), store).unwrap();
    let asset = real_asset(&state, alice.id).await;
    let mut settings = input("real-codec", "public");
    settings.format = codec.into();
    let created = music_outputs::create(&state, alice.id, &settings)
        .await
        .unwrap();
    let output: i64 = created.output["id"].as_str().unwrap().parse().unwrap();
    let row = music_outputs::get(&state, output, Some(alice.id))
        .await
        .unwrap();
    let empty = music_queues::get(&state, alice.id, &row.device)
        .await
        .unwrap();
    let source = if kind == "asset" {
        Source {
            asset_id: Some(asset.to_string()),
            status_id: None,
            remote_id: None,
            episode_id: None,
        }
    } else {
        origin_source(&state, &stub, alice.id, asset, kind).await
    };
    music_queues::change(
        &state,
        alice.id,
        &row.device,
        &Change {
            revision: empty.revision,
            action: Action::Add {
                sources: vec![source.clone(), source],
            },
        },
    )
    .await
    .unwrap();
    music_outputs::command(
        &state,
        alice.id,
        output,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        music_outputs::worker::run(&state, output, env!("CARGO_BIN_EXE_plamenu").into()),
    )
    .await
    .unwrap()
    .unwrap();
    let chunks: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT bytes FROM music_output_chunks WHERE output_id=$1 ORDER BY sequence",
    )
    .bind(output)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    let encoded = dir.path().join(format!("broadcast.{codec}"));
    std::fs::write(&encoded, chunks.concat()).unwrap();
    let decoded = dir.path().join("decoded.pcm");
    assert!(
        std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "fatal", "-i"])
            .arg(&encoded)
            .args(["-f", "s16le", "-ac", "2", "-ar", "44100", "-y"])
            .arg(&decoded)
            .status()
            .unwrap()
            .success()
    );
    assert!(std::fs::metadata(decoded).unwrap().len() > 100_000);
    let finished = music_outputs::get(&state, output, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(finished.transport, "paused");
    assert!(finished.worker_lease.is_none());
    assert_eq!(
        stub.enclosure_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_listener_body_revokes_current_tokens_and_releases_connection_lease(
    pool: PgPool,
) {
    use http_body_util::BodyExt;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("listener-room", "private"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    let token = created.listen_token.unwrap();
    let row = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    music_outputs::worker::publish(&state, &claimed, 1, b"live audio bytes")
        .await
        .unwrap();
    let response = music_outputs::stream::serve(&state, id, Some(&token))
        .await
        .unwrap();
    assert_eq!(response.headers()["Content-Type"], "audio/mpeg");
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "live audio bytes"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_output_listeners WHERE output_id=$1"
        )
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        1
    );
    let current = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    music_outputs::rotate_token(&state, alice.id, id, current.revision)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop(body);
    for _ in 0..20 {
        if sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_output_listeners WHERE output_id=$1",
        )
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
            == 0
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("Listener lease remained after disconnect");
}

async fn web(
    app: &axum::Router,
    cookie: &str,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Cookie", cookie)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_stream_forms_use_one_music_workspace_csrf_and_current_owner_session_for_private_audio(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    plamenu_db::user::create(
        &state.pool,
        alice.id,
        Some("alice@plamenu.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = web(
        &app,
        "",
        "POST",
        "/login",
        &[
            ("email", "alice@plamenu.test"),
            ("password", "fixture-password"),
        ],
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("Set-Cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let settings = [
        ("csrf", csrf.as_str()),
        ("mount", "native-room"),
        ("name", "Native listening room"),
        ("description", "Listen here"),
        ("genre", "Ambient"),
        ("format", "mp3"),
        ("bitrate", "128"),
        ("sample_rate", "44100"),
        ("visibility", "private"),
    ];
    let mut invalid = settings;
    invalid[0] = ("csrf", "wrong");
    assert_eq!(
        web(&app, &cookie, "POST", "/web/music/streams/create", &invalid)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        music_outputs::list(&state, Some(alice.id), true, None, "")
            .await
            .unwrap()
            .is_empty()
    );
    let created = web(
        &app,
        &cookie,
        "POST",
        "/web/music/streams/create",
        &settings,
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let html = text(created).await;
    assert!(html.contains("Private listener link"));
    let row = music_outputs::list(&state, Some(alice.id), true, None, "")
        .await
        .unwrap()
        .remove(0);
    let page = text(
        web(
            &app,
            &cookie,
            "GET",
            &format!("/music/streams/{}", row.id),
            &[],
        )
        .await,
    )
    .await;
    assert!(page.contains(&format!("/music/player?device={}", row.device)));
    assert!(page.contains("Stream settings"));
    assert!(!page.contains("?token="));
    assert_stream_destination(&app, &cookie, &row).await;
    let directory = text(web(&app, &cookie, "GET", "/music/streams?mine=true", &[]).await).await;
    assert!(directory.contains("Native listening room"));
    assert!(directory.contains("/music/streams"));
    assert!(directory.contains("/music/radios"));
    assert!(directory.contains("/music/podcasts"));
    music_outputs::command(
        &state,
        alice.id,
        row.id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, row.id)
        .await
        .unwrap()
        .unwrap();
    music_outputs::worker::publish(&state, &claimed, 1, b"owner audio")
        .await
        .unwrap();
    let path = format!("/music/streams/{}/audio", row.id);
    assert_eq!(
        web(&app, "", "GET", &path, &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    let audio = web(&app, &cookie, "GET", &path, &[]).await;
    assert_eq!(audio.status(), StatusCode::OK);
    let mut body = audio.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "owner audio"
    );
    sqlx::query("UPDATE oauth_tokens SET revoked_at=clock_timestamp() WHERE token_hash=$1")
        .bind(plamenu::auth::hash_secret(
            cookie.split_once('=').unwrap().1,
        ))
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_encodes_two_queue_occurrences_into_one_decodable_mp3_stream(pool: PgPool) {
    real_output_codec(pool, "mp3").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_encodes_two_queue_occurrences_into_one_decodable_ogg_stream(pool: PgPool) {
    real_output_codec(pool, "ogg").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_encodes_two_queue_occurrences_into_one_decodable_opus_stream(pool: PgPool) {
    real_output_codec(pool, "opus").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_encodes_guarded_podcast_enclosures_into_one_decodable_stream(pool: PgPool) {
    real_output_source(pool, "mp3", "episode").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_output_encodes_verified_remote_audio_into_one_decodable_stream(pool: PgPool) {
    real_output_source(pool, "mp3", "remote").await;
}

fn late_ogg_fixture(dir: &std::path::Path, codec: &str) -> (Vec<u8>, Vec<u8>) {
    let source_file = dir.join("source.ogg");
    let codec_name = if codec == "opus" {
        "libopus"
    } else {
        "libvorbis"
    };
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=437:duration=3",
                "-c:a",
                codec_name,
                "-b:a",
                "128k",
                "-f",
                "ogg",
                "-y"
            ])
            .arg(&source_file)
            .status()
            .unwrap()
            .success()
    );
    let bytes = std::fs::read(source_file).unwrap();
    let mut cursor = 0;
    let mut pages = Vec::new();
    let mut packets = 0;
    let mut header = Vec::new();
    while cursor < bytes.len() {
        assert_eq!(&bytes[cursor..cursor + 4], b"OggS");
        let segments = usize::from(bytes[cursor + 26]);
        let size = 27
            + segments
            + bytes[cursor + 27..cursor + 27 + segments]
                .iter()
                .map(|n| usize::from(*n))
                .sum::<usize>();
        packets += bytes[cursor + 27..cursor + 27 + segments]
            .iter()
            .filter(|n| **n < 255)
            .count();
        let page = bytes[cursor..cursor + size].to_vec();
        if packets <= if codec == "opus" { 2 } else { 3 } {
            header.extend_from_slice(&page);
        }
        pages.push(page);
        cursor += size;
    }
    assert!(!header.is_empty());
    (header, pages.pop().unwrap())
}

async fn late_ogg_listener(pool: PgPool, codec: &str) {
    use http_body_util::BodyExt;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let dir = tempfile::tempdir().unwrap();
    let (header, payload) = late_ogg_fixture(dir.path(), codec);
    let mut settings = input("late-listener", "public");
    settings.format = codec.into();
    let created = music_outputs::create(&state, alice.id, &settings)
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    let row = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE music_outputs SET codec_header=$2 WHERE id=$1")
        .bind(id)
        .bind(header)
        .execute(&state.pool)
        .await
        .unwrap();
    let mut sequence = 0;
    for chunk in payload.chunks(payload.len().div_ceil(2)) {
        sequence += 1;
        music_outputs::worker::publish(&state, &claimed, sequence, chunk)
            .await
            .unwrap();
    }
    let response = music_outputs::stream::serve(&state, id, None)
        .await
        .unwrap();
    assert_eq!(response.headers()["Content-Type"], "audio/ogg");
    let mut body = response.into_body();
    let mut received = Vec::new();
    for _ in 0..2 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        received.extend_from_slice(&frame.into_data().unwrap());
    }
    drop(body);
    let output = dir.path().join("joined.ogg");
    std::fs::write(&output, received).unwrap();
    let decoded = dir.path().join("joined.pcm");
    assert!(
        std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "fatal", "-i"])
            .arg(output)
            .args(["-f", "s16le", "-y"])
            .arg(&decoded)
            .status()
            .unwrap()
            .success()
    );
    assert!(std::fs::metadata(decoded).unwrap().len() > 1000);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn late_ogg_listener_gets_initialization_and_complete_audio_pages(pool: PgPool) {
    late_ogg_listener(pool, "ogg").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn late_opus_listener_gets_initialization_and_complete_audio_pages(pool: PgPool) {
    late_ogg_listener(pool, "opus").await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn broadcast_metadata_is_opt_in_ordered_and_head_does_not_admit_a_listener(pool: PgPool) {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("radio-room", "public"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: "1".into(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    for (sequence, title, length) in [
        (1, "First 'song';", 10000),
        (2, "Second song", 16384),
        (3, "Last song", 6384),
    ] {
        sqlx::query("UPDATE music_outputs SET now_playing=$2 WHERE id=$1")
            .bind(id)
            .bind(title)
            .execute(&state.pool)
            .await
            .unwrap();
        music_outputs::worker::publish(&state, &claimed, sequence, &vec![b'a'; length])
            .await
            .unwrap();
    }
    let app = plamenu::build_router(state.clone());
    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/music/live/radio-room")
                .header("Icy-MetaData", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), axum::http::StatusCode::OK);
    assert_eq!(head.headers()["icy-metaint"], "16384");
    assert_eq!(head.headers()["icy-name"], "Listening room");
    assert_eq!(head.headers()["accept-ranges"], "none");
    assert_eq!(head.headers()["access-control-allow-origin"], "*");
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_output_listeners")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    let plain = music_outputs::stream::serve(&state, id, None)
        .await
        .unwrap();
    assert!(!plain.headers().contains_key("icy-metaint"));
    let mut plain = plain.into_body();
    for length in [10000, 16384, 6384] {
        assert_eq!(
            plain.frame().await.unwrap().unwrap().into_data().unwrap(),
            vec![b'a'; length]
        );
    }
    drop(plain);
    let icy = app
        .oneshot(
            Request::builder()
                .uri("/music/live/radio-room")
                .header("Icy-MetaData", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = icy.into_body();
    let mut bytes = Vec::new();
    for _ in 0..3 {
        bytes.extend_from_slice(&body.frame().await.unwrap().unwrap().into_data().unwrap());
    }
    assert_eq!(&bytes[..16384], vec![b'a'; 16384]);
    let size = usize::from(bytes[16384]) * 16;
    assert!(
        String::from_utf8_lossy(&bytes[16385..16385 + size])
            .starts_with("StreamTitle='Second song';")
    );
    let second = 16385 + size + 16384;
    let size = usize::from(bytes[second]) * 16;
    assert!(
        String::from_utf8_lossy(&bytes[second + 1..second + 1 + size])
            .starts_with("StreamTitle='Last song';")
    );
}

async fn checkpoint_fixture(
    pool: PgPool,
) -> (
    plamenu::AppState,
    plamenu::music_outputs::Output,
    plamenu::music_queues::Queue,
    tempfile::TempDir,
) {
    use plamenu::music_queues::{self, Action, Change, Source};
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = dir.path().join("media");
    let store = std::sync::Arc::new(
        plamenu::storage::LocalDiskStore::new(config.media_dir.clone()).unwrap(),
    );
    let state =
        plamenu::AppState::new(pool, config, StubFederation::with_actors([]), store).unwrap();
    let asset = real_asset(&state, alice.id).await;
    let created = music_outputs::create(&state, alice.id, &input("checkpoint", "private"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    let row = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    let queue = music_queues::change(
        &state,
        alice.id,
        &row.device,
        &Change {
            revision: "0".into(),
            action: Action::Add {
                sources: vec![Source {
                    asset_id: Some(asset.to_string()),
                    status_id: None,
                    episode_id: None,
                    remote_id: None,
                }],
            },
        },
    )
    .await
    .unwrap();
    let queue = music_queues::change(
        &state,
        alice.id,
        &row.device,
        &Change {
            revision: queue.revision,
            action: Action::Next { ended: false },
        },
    )
    .await
    .unwrap();
    (state, row, queue, dir)
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn checkpoint_never_overwrites_a_seek_or_a_reclaimed_worker_and_stop_resets_queue(
    pool: PgPool,
) {
    use plamenu::music_queues::{self, Action, Change};
    let (state, row, queue, _dir) = checkpoint_fixture(pool).await;
    let owner = row.owner_id;
    let id = row.id;
    music_outputs::command(
        &state,
        owner,
        id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        music_outputs::playback::checkpoint(&state, &claimed, &queue, 0.1)
            .await
            .unwrap()
    );
    let saved = music_queues::get(&state, owner, &row.device).await.unwrap();
    assert_eq!(saved.revision, queue.revision);
    assert!((saved.position_seconds - 0.1).abs() < f64::EPSILON);
    let seek = music_queues::change(
        &state,
        owner,
        &row.device,
        &Change {
            revision: saved.revision,
            action: Action::Seek {
                entry_id: saved.current_entry_id.unwrap(),
                seconds: 0.2,
            },
        },
    )
    .await
    .unwrap();
    assert!(
        !music_outputs::playback::checkpoint(&state, &claimed, &queue, 0.3)
            .await
            .unwrap()
    );
    assert!(
        (music_queues::get(&state, owner, &row.device)
            .await
            .unwrap()
            .position_seconds
            - 0.2)
            .abs()
            < f64::EPSILON
    );
    sqlx::query("UPDATE music_outputs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    let replacement = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !music_outputs::playback::checkpoint(&state, &claimed, &seek, 0.3)
            .await
            .unwrap()
    );
    assert!(
        music_outputs::playback::checkpoint(&state, &replacement, &seek, 0.25)
            .await
            .unwrap()
    );
    music_outputs::command(
        &state,
        owner,
        id,
        &Command {
            revision: replacement.revision.to_string(),
            operation: "stop".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        !music_outputs::playback::checkpoint(&state, &replacement, &seek, 0.3)
            .await
            .unwrap()
    );
    assert!(
        music_queues::get(&state, owner, &row.device)
            .await
            .unwrap()
            .position_seconds
            .abs()
            < f64::EPSILON
    );
}

async fn assert_stream_destination(app: &axum::Router, cookie: &str, row: &music_outputs::Output) {
    let page = text(
        web(
            app,
            cookie,
            "GET",
            &format!("/music/player?device={}", row.device),
            &[],
        )
        .await,
    )
    .await;
    assert!(page.contains("Playback continues when you close this page."));
    assert!(page.contains("music-output-audio"));
    assert!(!page.contains("music-player-audio"));
    assert!(page.contains("Playback destination"));
    assert!(page.contains("Native listening room"));
    assert!(page.contains(&format!("/web/music/streams/{}/command", row.id)));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stream_restart_applies_current_role_bitrate_and_encoder_failure_releases_lease(
    pool: PgPool,
) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("role-limits", "public"))
        .await
        .unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: "1".into(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let mut lower = state.clone();
    std::sync::Arc::make_mut(&mut lower.config)
        .music
        .user_max_bitrate = 64;
    music_outputs::worker::run(&lower, id, env!("CARGO_BIN_EXE_plamenu").into())
        .await
        .unwrap();
    let limited = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(limited.bitrate, 64);
    assert_eq!(limited.transport, "paused");
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: limited.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let mut broken = state.clone();
    std::sync::Arc::make_mut(&mut broken.config).ffmpeg_path =
        "/nonexistent/plamenu-output-encoder".into();
    assert!(
        music_outputs::worker::run(&broken, id, env!("CARGO_BIN_EXE_plamenu").into())
            .await
            .is_err()
    );
    let failed = music_outputs::get(&state, id, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(failed.transport, "paused");
    assert!(failed.worker_lease.is_none());
    assert!(failed.last_error.is_some());
}

async fn wait_for_checkpoint(state: &plamenu::AppState, output: i64, owner: i64, after: f64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = music_outputs::get(state, output, Some(owner))
                .await
                .unwrap();
            if row.position_seconds > after {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn shutdown_keeps_server_playback_intent_and_restarted_worker_resumes_saved_position(
    pool: PgPool,
) {
    use plamenu::music_queues::{self, Action, Change, Source};
    let (state, row, queue, _dir) = checkpoint_fixture(pool).await;
    let asset = real_asset_duration(&state, row.owner_id, 6.0).await;
    let queue = music_queues::change(
        &state,
        row.owner_id,
        &row.device,
        &Change {
            revision: queue.revision,
            action: Action::Clear,
        },
    )
    .await
    .unwrap();
    music_queues::change(
        &state,
        row.owner_id,
        &row.device,
        &Change {
            revision: queue.revision,
            action: Action::Add {
                sources: vec![Source {
                    asset_id: Some(asset.to_string()),
                    status_id: None,
                    episode_id: None,
                    remote_id: None,
                }],
            },
        },
    )
    .await
    .unwrap();
    music_outputs::command(
        &state,
        row.owner_id,
        row.id,
        &Command {
            revision: row.revision.to_string(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let worker_state = state.clone();
    let output = row.id;
    let worker = tokio::spawn(async move {
        music_outputs::worker::run(&worker_state, output, env!("CARGO_BIN_EXE_plamenu").into())
            .await
    });
    wait_for_checkpoint(&state, row.id, row.owner_id, 0.2).await;
    state.shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let checkpoint = music_outputs::get(&state, row.id, Some(row.owner_id))
        .await
        .unwrap();
    assert_eq!(checkpoint.transport, "playing");
    assert!(checkpoint.worker_lease.is_none());
    let mut restarted = state.clone();
    restarted.shutdown = tokio_util::sync::CancellationToken::new();
    let worker_state = restarted.clone();
    let worker = tokio::spawn(async move {
        music_outputs::worker::run(&worker_state, output, env!("CARGO_BIN_EXE_plamenu").into())
            .await
    });
    wait_for_checkpoint(
        &restarted,
        row.id,
        row.owner_id,
        checkpoint.position_seconds + 0.2,
    )
    .await;
    let live = music_outputs::get(&restarted, row.id, Some(row.owner_id))
        .await
        .unwrap();
    assert!(live.generation > checkpoint.generation);
    assert_eq!(live.current_entry_id, checkpoint.current_entry_id);
    let current = music_queues::get(&restarted, row.owner_id, &row.device)
        .await
        .unwrap();
    music_queues::change(
        &restarted,
        row.owner_id,
        &row.device,
        &Change {
            revision: current.revision,
            action: Action::Seek {
                entry_id: current.current_entry_id.unwrap(),
                seconds: 2.5,
            },
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_listen_sessions WHERE account_id=$1"
        )
        .bind(row.owner_id)
        .fetch_one(&restarted.pool)
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        music_outputs::get(&restarted, row.id, Some(row.owner_id))
            .await
            .unwrap()
            .transport,
        "paused"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn listener_bearer_capability_grants_only_bytes_and_rotation_revokes_it(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let created = music_outputs::create(&state, alice.id, &input("capability", "private"))
        .await
        .unwrap();
    let token = created.listen_token.unwrap();
    let id = created.output["id"].as_str().unwrap().parse().unwrap();
    music_outputs::command(
        &state,
        alice.id,
        id,
        &Command {
            revision: "1".into(),
            operation: "play".into(),
        },
    )
    .await
    .unwrap();
    let claimed = music_outputs::worker::claim(&state, id)
        .await
        .unwrap()
        .unwrap();
    music_outputs::worker::publish(&state, &claimed, 1, b"private radio bytes")
        .await
        .unwrap();
    let app = plamenu::build_router(state.clone());
    for (uri, status) in [
        ("/music/live/capability", StatusCode::OK),
        ("/api/v1/music/outputs", StatusCode::UNAUTHORIZED),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-credentials")
            );
            let mut body = response.into_body();
            assert_eq!(
                body.frame().await.unwrap().unwrap().into_data().unwrap(),
                "private radio bytes"
            );
            music_outputs::rotate_token(&state, alice.id, id, claimed.revision)
                .await
                .unwrap();
            assert!(body.frame().await.unwrap().is_err());
        }
    }
    let denied = app
        .oneshot(
            Request::builder()
                .uri("/music/live/capability")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!denied.status().is_success());
}
