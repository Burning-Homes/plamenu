mod common;
use common::{StubFederation, create_local_account};
use plamenu::{
    AppState,
    error::ApiError,
    music_downloads::{self, Request},
    music_queues::Source,
};
use plamenu_db::{PgPool, id};
use std::{io::Read as _, sync::Arc};
struct Fixture {
    state: AppState,
    owner: i64,
    viewer: i64,
    asset: i64,
    bytes: Vec<u8>,
    stub: Arc<StubFederation>,
    _root: tempfile::TempDir,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tone.wav");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=441:duration=0.2",
                "-c:a",
                "pcm_s16le",
                "-y"
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let bytes = std::fs::read(path).unwrap();
    let mut config = common::test_config();
    config.media_dir = root.path().to_owned();
    config.music.min_free_mib = 0;
    let store = Arc::new(plamenu::storage::MemoryStore::default());
    let stub = StubFederation::with_actors([]);
    let state = AppState::new(pool, config, stub.clone(), store).unwrap();
    let artist = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) VALUES($1,$2,$3,'../Title / \\ strange',$4,$5,'wav',$6,1,$7,0.2)")
        .bind(asset).bind(owner).bind(artist).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(i64::try_from(bytes.len()).unwrap()).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    state
        .media
        .put(&format!("music{asset}.orig"), bytes.clone())
        .await
        .unwrap();
    Fixture {
        state,
        owner,
        viewer,
        asset,
        bytes,
        stub,
        _root: root,
    }
}
fn source(asset: i64) -> Source {
    Source {
        asset_id: Some(asset.to_string()),
        status_id: None,
        remote_id: None,
        episode_id: None,
    }
}
fn request(sources: Vec<Source>) -> Request {
    Request {
        sources,
        kind: None,
        id: None,
        label: Some("My download".into()),
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn zip_jobs_preserve_order_and_repeats_safe_names_private_access_and_cleanup_debt(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let row = music_downloads::create(
        &f.state,
        f.owner,
        &request(vec![source(f.asset), source(f.asset)]),
    )
    .await
    .unwrap();
    assert!(matches!(
        music_downloads::get(&f.state, f.viewer, row.id).await,
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        music_downloads::authorize_file(&f.state, f.owner, row.id).await,
        Err(ApiError::NotFound)
    ));
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    let ready = music_downloads::authorize_file(&f.state, f.owner, row.id)
        .await
        .unwrap();
    assert_eq!(ready.included, vec![0, 1]);
    let notice = ready.notification_id.unwrap();
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM notifications WHERE account_id=$1 AND kind='music.download'"
        )
        .bind(f.owner)
        .fetch_one(&f.state.pool)
        .await
        .unwrap(),
        1
    );
    let key = ready.file_name.unwrap();
    assert!(
        !plamenu_db::media::public_file_allowed(&f.state.pool, &key)
            .await
            .unwrap()
    );
    let bytes = f.state.media.get(&key).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(zip.len(), 2);
    let mut names = Vec::new();
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).unwrap();
        assert!(!file.name().contains('/') && !file.name().contains('\\'));
        assert!(
            std::path::Path::new(file.name())
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        );
        names.push(file.name().to_owned());
        let mut payload = Vec::new();
        file.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, f.bytes);
    }
    assert_ne!(names[0], names[1]);
    music_downloads::remove(&f.state, f.owner, row.id, ready.revision, false)
        .await
        .unwrap();
    let debt: i64 =
        sqlx::query_scalar("SELECT size_bytes FROM music_storage_debt WHERE file_name=$1")
            .bind(&key)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, ready.size_bytes);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications WHERE id=$1")
            .bind(notice)
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        music_downloads::get(&f.state, f.owner, row.id)
            .await
            .is_err()
    );
}
async fn shared_collection(f: &Fixture) -> i64 {
    use plamenu::music_collections::{self, Add, New};
    let row = music_collections::create(
        &f.state,
        f.owner,
        &New {
            name: "Shared library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let row =
        music_collections::set_contents_sharing(&f.state, f.owner, row.id, row.revision, true)
            .await
            .unwrap();
    music_collections::add(
        &f.state,
        f.owner,
        row.id,
        &Add {
            revision: row.revision.to_string(),
            asset_id: Some(f.asset.to_string()),
            status_id: None,
            remote_id: None,
            episode_id: None,
        },
    )
    .await
    .unwrap();
    row.id
}
#[sqlx::test(migrations = "../db/migrations")]
async fn archive_container_scope_and_current_asset_permissions_revoke_completed_downloads(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    assert!(
        music_downloads::create(&f.state, f.viewer, &request(vec![source(f.asset)]))
            .await
            .is_err()
    );
    let collection = shared_collection(&f).await;
    let input = Request {
        sources: Vec::new(),
        kind: Some("collection".into()),
        id: Some(collection.to_string()),
        label: None,
    };
    let row = music_downloads::create(&f.state, f.viewer, &input)
        .await
        .unwrap();
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    assert!(
        music_downloads::authorize_file(&f.state, f.viewer, row.id)
            .await
            .is_ok()
    );
    let parent = plamenu::music_collections::get(&f.state, collection, Some(f.owner))
        .await
        .unwrap();
    plamenu::music_collections::set_contents_sharing(
        &f.state,
        f.owner,
        collection,
        parent.revision,
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        music_downloads::authorize_file(&f.state, f.viewer, row.id).await,
        Err(ApiError::NotFound)
    ));
    let hidden = music_downloads::get(&f.state, f.viewer, row.id)
        .await
        .unwrap()
        .entity(&f.state)
        .await
        .unwrap();
    assert!(hidden["items"][0]["source"].is_null());
    assert_eq!(hidden["label"], "Music download");
}
async fn remote(f: &Fixture) -> i64 {
    let publisher = common::RemoteUser::new("origin.example", "publisher");
    f.stub
        .actors
        .lock()
        .unwrap()
        .insert(publisher.actor.id.clone(), publisher.actor.clone());
    let uri = "https://origin.example/audio/one";
    f.stub.objects.lock().unwrap().insert(uri.into(),serde_json::json!({"id":uri,"type":"Audio","attributedTo":publisher.actor.id,"to":[plamenu_ap::activity::PUBLIC],"name":"Origin tone","attachment":[{"type":"Audio","mediaType":"audio/wav","url":"https://origin.example/tone.wav"}]}));
    f.stub.media.lock().unwrap().insert(
        "https://origin.example/tone.wav".into(),
        plamenu_federation::FetchedMedia {
            final_url: "https://origin.example/tone.wav".into(),
            content_type: "audio/wav".into(),
            bytes: f.bytes.clone(),
        },
    );
    plamenu::music_remote::import(&f.state, f.owner, uri)
        .await
        .unwrap()
        .id
}
async fn wait_for_origin(f: &Fixture) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if f.stub
                .enclosure_active
                .load(std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[sqlx::test(migrations = "../db/migrations")]
async fn cancelling_live_archive_stops_the_origin_body_and_never_publishes_late_bytes(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let remote = remote(&f).await;
    *f.stub.enclosure_chunking.lock().unwrap() = Some((256, 100));
    let row = music_downloads::create(
        &f.state,
        f.owner,
        &request(vec![Source {
            asset_id: None,
            status_id: None,
            episode_id: None,
            remote_id: Some(remote.to_string()),
        }]),
    )
    .await
    .unwrap();
    let state = f.state.clone();
    let id = row.id;
    let task = tokio::spawn(async move { music_downloads::worker::run(&state, id).await });
    wait_for_origin(&f).await;
    let running = music_downloads::get(&f.state, f.owner, row.id)
        .await
        .unwrap();
    music_downloads::remove(&f.state, f.owner, row.id, running.revision, true)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        music_downloads::get(&f.state, f.owner, row.id)
            .await
            .unwrap()
            .state,
        "cancelled"
    );
    assert_eq!(
        f.stub
            .enclosure_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        f.state
            .media
            .get(running.file_name.as_ref().unwrap())
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn leases_and_active_limits_fence_reclaims_stale_edits_and_retirement(pool: PgPool) {
    let mut f = fixture(pool).await;
    std::sync::Arc::make_mut(&mut f.state.config)
        .music
        .download_jobs = 1;
    let input = request(vec![source(f.asset)]);
    let one = music_downloads::create(&f.state, f.owner, &input)
        .await
        .unwrap();
    let two = music_downloads::create(&f.state, f.owner, &input)
        .await
        .unwrap();
    assert!(
        music_downloads::create(&f.state, f.owner, &input)
            .await
            .is_err()
    );
    let old = music_downloads::worker::claim(&f.state, one.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        music_downloads::worker::claim(&f.state, two.id)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE music_downloads SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(one.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let current = music_downloads::worker::claim(&f.state, one.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old.file_name, current.file_name);
    assert!(
        music_downloads::worker::heartbeat(&f.state, &old)
            .await
            .is_err()
    );
    music_downloads::worker::heartbeat(&f.state, &current)
        .await
        .unwrap();
    assert!(matches!(
        music_downloads::remove(&f.state, f.owner, one.id, old.revision, true).await,
        Err(ApiError::Conflict(_))
    ));
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_downloads::get(&f.state, f.owner, one.id)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_downloads WHERE owner_id=$1")
            .bind(f.owner)
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_storage_debt WHERE owner_id=$1")
            .bind(f.owner)
            .fetch_one(&f.state.pool)
            .await
            .unwrap()
            > 0
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn unavailable_members_are_reported_and_archives_expire_with_durable_cleanup(pool: PgPool) {
    let f = fixture(pool).await;
    let remote = remote(&f).await;
    f.stub.media.lock().unwrap().clear();
    let row = music_downloads::create(
        &f.state,
        f.owner,
        &request(vec![
            source(f.asset),
            Source {
                asset_id: None,
                status_id: None,
                remote_id: Some(remote.to_string()),
                episode_id: None,
            },
        ]),
    )
    .await
    .unwrap();
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    let ready = music_downloads::authorize_file(&f.state, f.owner, row.id)
        .await
        .unwrap();
    assert_eq!(ready.included, vec![0]);
    assert_eq!(ready.item_errors.as_array().unwrap().len(), 1);
    let key = ready.file_name.unwrap();
    sqlx::query("UPDATE music_downloads SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(row.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_downloads::authorize_file(&f.state, f.owner, row.id)
            .await
            .is_err()
    );
    music_downloads::worker::expire(&f.state).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM media_cleanup_jobs WHERE file_name=$1)"
        )
        .bind(key)
        .fetch_one(&f.state.pool)
        .await
        .unwrap()
    );
}

async fn credentials(state: &AppState, owner: i64) -> (String, String) {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let user = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("password-for-tests").unwrap(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "download-tests",
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
    for (raw, scope) in [(&read, "read:statuses"), (&write, "write:statuses")] {
        oauth::create_token(&state.pool, &hash_secret(raw), app.id, Some(user.id), scope)
            .await
            .unwrap();
    }
    (read, write)
}
async fn send(
    app: &axum::Router,
    token: &str,
    method: &str,
    url: &str,
    body: Vec<u8>,
    form: bool,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;
    let request = Request::builder().method(method).uri(url);
    let request = if form {
        request
            .header("Cookie", token)
            .header("Content-Type", "application/x-www-form-urlencoded")
    } else {
        request
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
    };
    app.clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}
async fn native_cookie(app: &axum::Router) -> String {
    let response = send(
        app,
        "",
        "POST",
        "/login",
        serde_urlencoded::to_string([
            ("email", "alice@plamenu.test"),
            ("password", "password-for-tests"),
        ])
        .unwrap()
        .into_bytes(),
        true,
    )
    .await;
    assert!(response.status().is_redirection());
    response
        .headers()
        .get_all("Set-Cookie")
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|header| header.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn download_api_and_native_forms_keep_scopes_csrf_and_one_music_workspace(pool: PgPool) {
    use axum::http::StatusCode;
    use http_body_util::BodyExt as _;
    let f = fixture(pool).await;
    let (read, write) = credentials(&f.state, f.owner).await;
    let app = plamenu::build_router(f.state.clone());
    let input = serde_json::json!({"sources":[{"asset_id":f.asset.to_string()}],"label":"Private download"});
    let denied = send(
        &app,
        &read,
        "POST",
        "/api/v1/music/downloads",
        serde_json::to_vec(&input).unwrap(),
        false,
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let created = send(
        &app,
        &write,
        "POST",
        "/api/v1/music/downloads",
        serde_json::to_vec(&input).unwrap(),
        false,
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert!(
        created.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    let value: serde_json::Value =
        serde_json::from_slice(&created.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let id = value["id"].as_str().unwrap().parse::<i64>().unwrap();
    assert_eq!(
        send(
            &app,
            &write,
            "GET",
            "/api/v1/music/downloads",
            Vec::new(),
            false
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let full = native_cookie(&app).await;
    let csrf =
        plamenu::auth::hash_secret(&format!("plamenu-csrf:{}", full.split_once('=').unwrap().1));
    let form = serde_urlencoded::to_string([
        ("csrf", "wrong"),
        ("kind", "asset"),
        ("id", &f.asset.to_string()),
        ("label", "Form download"),
    ])
    .unwrap();
    assert_eq!(
        send(
            &app,
            &full,
            "POST",
            "/web/music/downloads/create",
            form.into_bytes(),
            true
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let preview = send(
        &app,
        &full,
        "GET",
        &format!("/music/downloads?kind=asset&id={}", f.asset),
        Vec::new(),
        true,
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let html = String::from_utf8(
        preview
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("Download ZIP")
            && html.contains("/music/podcasts")
            && html.contains("/music/radios")
    );
    let form = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("kind", "asset"),
        ("id", &f.asset.to_string()),
        ("label", "Form download"),
    ])
    .unwrap();
    assert!(
        send(
            &app,
            &full,
            "POST",
            "/web/music/downloads/create",
            form.into_bytes(),
            true
        )
        .await
        .status()
        .is_redirection()
    );
    music_downloads::worker::run(&f.state, id).await.unwrap();
    check_ready_notification(&app, &full, id).await;
    check_bulk_selection(&app, &full, &csrf, f.asset).await;
    let file = send(
        &app,
        &read,
        "GET",
        &format!("/music/downloads/{id}/file"),
        Vec::new(),
        false,
    )
    .await;
    assert_eq!(file.status(), StatusCode::OK);
    assert_eq!(file.headers()["content-type"], "application/zip");
    assert!(
        file.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .starts_with(b"PK")
    );
    assert_eq!(
        send(
            &app,
            &write,
            "GET",
            &format!("/music/downloads/{id}/file"),
            Vec::new(),
            false
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

async fn check_ready_notification(app: &axum::Router, cookie: &str, id: i64) {
    use http_body_util::BodyExt as _;
    let token = cookie.split_once('=').unwrap().1;
    for path in [
        "/api/v1/notifications?types[]=music.download",
        "/api/v2/notifications?types[]=music.download",
    ] {
        let response = send(app, token, "GET", path, Vec::new(), false).await;
        assert!(response.status().is_success());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!("/music/downloads/{id}")));
        assert!(!text.contains("musicdownload") && !text.contains("../Title"));
    }
}
async fn check_bulk_selection(app: &axum::Router, cookie: &str, csrf: &str, asset: i64) {
    let form = serde_urlencoded::to_string([
        ("csrf", csrf),
        ("asset_id", &asset.to_string()),
        ("asset_id", &asset.to_string()),
    ])
    .unwrap();
    let response = send(
        app,
        cookie,
        "POST",
        "/web/music/downloads/create",
        form.into_bytes(),
        true,
    )
    .await;
    assert!(response.status().is_redirection());
    assert!(
        response.headers()["location"]
            .to_str()
            .unwrap()
            .starts_with("/music/downloads/")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn archive_byte_ranges_preserve_current_token_authority_after_response_headers(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let f = fixture(pool).await;
    let (read, _) = credentials(&f.state, f.owner).await;
    let row = music_downloads::create(
        &f.state,
        f.owner,
        &request(vec![source(f.asset), source(f.asset)]),
    )
    .await
    .unwrap();
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/music/downloads/{}/file", row.id))
                .header("Authorization", format!("Bearer {read}"))
                .header("Range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(
        response.headers()["content-range"]
            .to_str()
            .unwrap()
            .starts_with("bytes 0-3/")
    );
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        b"PK\x03\x04"
    );
    let response = send(
        &plamenu::build_router(f.state.clone()),
        &read,
        "GET",
        &format!("/music/downloads/{}/file", row.id),
        Vec::new(),
        false,
    )
    .await;
    let mut body = response.into_body();
    assert!(
        !body
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE oauth_tokens SET revoked_at=now() WHERE token_hash=$1")
        .bind(plamenu::auth::hash_secret(&read))
        .execute(&f.state.pool)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    assert!(body.frame().await.unwrap().is_err());
}

async fn hidden_prefix(f: &Fixture) -> i64 {
    let public = id::next();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,visibility,original_file,stream_file,original_format,original_bytes,stream_bytes,audio_hash,duration) SELECT $2,owner_id,artist_id,'Visible tail','public',$3,$4,original_format,original_bytes,stream_bytes,$5,duration FROM music_assets WHERE id=$1")
        .bind(f.asset).bind(public).bind(format!("music{public}.orig")).bind(format!("music{public}.mp3")).bind(format!("{public:064x}")).execute(&f.state.pool).await.unwrap();
    let mut bytes = f.bytes.clone();
    *bytes.last_mut().unwrap() ^= 1;
    f.state
        .media
        .put(&format!("music{public}.orig"), bytes)
        .await
        .unwrap();
    let collection = plamenu::music_collections::create(
        &f.state,
        f.owner,
        &plamenu::music_collections::New {
            name: "Public playlist".into(),
            description: String::new(),
            kind: "playlist".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let ids = (0..52).map(|_| id::next()).collect::<Vec<_>>();
    let mut assets = vec![f.asset; 51];
    assets.push(public);
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) SELECT entry,$1,asset,ordinality::int FROM unnest($2::bigint[],$3::bigint[]) WITH ORDINALITY AS input(entry,asset,ordinality)")
        .bind(collection.id).bind(ids).bind(assets).execute(&f.state.pool).await.unwrap();
    collection.id
}
#[sqlx::test(migrations = "../db/migrations")]
async fn container_archive_limit_applies_after_permissions_across_collection_pages(pool: PgPool) {
    let mut f = fixture(pool).await;
    std::sync::Arc::make_mut(&mut f.state.config)
        .music
        .max_download_items = 1;
    let collection = hidden_prefix(&f).await;
    let input = Request {
        sources: Vec::new(),
        kind: Some("collection".into()),
        id: Some(collection.to_string()),
        label: None,
    };
    let row = music_downloads::create(&f.state, f.viewer, &input)
        .await
        .unwrap();
    let value = row.entity(&f.state).await.unwrap();
    assert_eq!(value["items"].as_array().unwrap().len(), 1);
    assert_eq!(value["items"][0]["source"]["title"], "Visible tail");
    music_downloads::worker::run(&f.state, row.id)
        .await
        .unwrap();
    assert_eq!(
        music_downloads::authorize_file(&f.state, f.viewer, row.id)
            .await
            .unwrap()
            .included,
        vec![0]
    );
}
