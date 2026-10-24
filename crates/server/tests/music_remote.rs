mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState, music_mine, music_remote,
    music_saved::{self, Kind, Reference},
};
use plamenu_ap::activity::PUBLIC;
use plamenu_db::PgPool;
use serde_json::{Value, json};
use std::sync::Arc;
const ROOT: &str = "https://songhive.test/library";
const TRACK: &str = "https://songhive.test/tracks/one";
const AUDIO: &str = "https://songhive.test/audio/one";
async fn fixture(pool: PgPool) -> (AppState, i64, RemoteUser, Arc<StubFederation>, Value) {
    let viewer = create_local_account(&pool, "alice", "Alice").await.id;
    let publisher = RemoteUser::new("songhive.test", "publisher");
    let catalogue = RemoteUser::new("songhive.test", "catalogue");
    let stub = StubFederation::with_actors([publisher.actor.clone(), catalogue.actor.clone()]);
    let root = json!({"id":ROOT,"type":"Library","actor":publisher.actor.id,"to":[PUBLIC],"name":"Origin library","items":[{
        "id":AUDIO,"type":"Audio","attributedTo":publisher.actor.id,"to":[PUBLIC],"name":"Audio rendition",
        "attachment":[{"type":"Audio","mediaType":"audio/mpeg","url":"https://songhive.test/files/one.mp3"}],
        "track":{"id":TRACK,"type":"Track","name":"Origin track","attributedTo":publisher.actor.id,
            "album":{"id":"https://songhive.test/albums/one","type":"Album","name":"Origin album","attributedTo":catalogue.actor.id}}
    }]});
    stub.objects.lock().unwrap().insert(
        "https://songhive.test/albums/one".into(),
        root["items"][0]["track"]["album"].clone(),
    );
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), root.clone());
    (
        test_state_with(pool, stub.clone()),
        viewer,
        publisher,
        stub,
        root,
    )
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_remote_saved_music_is_private_and_never_copies_the_catalogue(pool: PgPool) {
    let (state, viewer, _, _, _) = fixture(pool).await;
    let root = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let audio: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(AUDIO)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: audio.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(saved.reference.id, track.to_string());
    let again = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(saved.id, again.id);
    assert!(
        music_saved::contains(
            &state,
            viewer,
            &Reference {
                kind: Kind::Remote,
                id: audio.to_string()
            }
        )
        .await
        .unwrap()
    );
    let mine = music_mine::page(
        &state,
        viewer,
        &music_mine::Request {
            kind: music_mine::Filter::Tracks,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(mine.items.len(), 1);
    assert_eq!(mine.items[0].source["name"], "Origin track");
    assert!(!mine.items[0].membership.owned);
    assert!(mine.items[0].membership.saved);
    let copied:(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM music_assets),(SELECT count(*) FROM music_artists),(SELECT count(*) FROM music_albums),(SELECT count(*) FROM statuses)").fetch_one(&state.pool).await.unwrap();
    assert_eq!(copied, (0, 0, 0, 0));
    assert_eq!(root.uri, ROOT);
    let owner_id: i64 =
        sqlx::query_scalar("SELECT controller_id FROM music_remote_resources WHERE uri=$1")
            .bind(TRACK)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO blocks(id,account_id,target_account_id) VALUES($1,$2,$3)")
        .bind(plamenu_db::id::next())
        .bind(viewer)
        .bind(owner_id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_remote::get(&state, track, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_mine::page(&state, viewer, &music_mine::Request::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let page = music_saved::page(&state, viewer, None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].source.is_none());
    music_saved::remove(&state, viewer, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_identity_controller_and_embedded_private_content_fail_without_partial_cache(
    pool: PgPool,
) {
    let (state, viewer, _, stub, root) = fixture(pool).await;
    for invalid in [
        {
            let mut doc = root.clone();
            doc["id"] = json!("https://songhive.test/wrong");
            doc
        },
        {
            let mut doc = root.clone();
            doc["actor"] = json!("https://evil.test/actor");
            doc
        },
        {
            let mut doc = root.clone();
            doc["items"][0]["to"] = json!(["https://songhive.test/users/private"]);
            doc
        },
    ] {
        stub.objects.lock().unwrap().insert(ROOT.into(), invalid);
        assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_remote_resources")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let mut bad_album = root["items"][0]["track"]["album"].clone();
    bad_album["attributedTo"] = json!("https://songhive.test/users/unverified");
    stub.objects
        .lock()
        .unwrap()
        .insert("https://songhive.test/albums/one".into(), bad_album);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_remote_resources")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn signed_controller_updates_and_deletes_preserve_removable_unavailable_bookmarks(
    pool: PgPool,
) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    let row = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: row.id.to_string(),
        },
    )
    .await
    .unwrap();
    let mut updated = root;
    updated["name"] = json!("Updated library");
    let app = plamenu::build_router(state.clone());
    for object in [updated, json!({"id":ROOT,"type":"Tombstone"})] {
        let kind = if object["type"] == "Tombstone" {
            "Delete"
        } else {
            "Update"
        };
        let raw = json!({"id":format!("https://songhive.test/activities/{kind}"),"type":kind,"actor":publisher.actor.id,"object":object});
        let body = serde_json::to_vec(&raw).unwrap();
        let signed = publisher.signer().sign_post(
            common::TEST_DOMAIN,
            "/inbox",
            &body,
            std::time::SystemTime::now(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/inbox")
                    .header("host", signed.host)
                    .header("date", signed.date)
                    .header("digest", signed.digest)
                    .header("signature", signed.signature)
                    .header("content-type", "application/activity+json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        if kind == "Update" {
            assert_eq!(
                music_remote::get(&state, row.id, viewer)
                    .await
                    .unwrap()
                    .name,
                "Updated library"
            );
        }
    }
    assert!(matches!(
        music_remote::get(&state, row.id, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
    assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
    music_saved::remove(&state, viewer, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn fetched_snapshot_cannot_overwrite_controller_delete_or_newer_update(pool: PgPool) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    let row = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let sender = plamenu_db::account::find_by_uri(&state.pool, &publisher.actor.id)
        .await
        .unwrap()
        .unwrap();
    stub.gate_activitypub_fetches();
    let before = stub.fetches.lock().unwrap().len();
    let pending_state = state.clone();
    let pending =
        tokio::spawn(async move { music_remote::import(&pending_state, viewer, ROOT).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if stub.fetches.lock().unwrap().len() > before {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    root["name"] = json!("Latest signed name");
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Update","actor":publisher.actor.id,"object":root}),
    )
    .await
    .unwrap();
    stub.open_activitypub_fetches();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(
        music_remote::get(&state, row.id, viewer)
            .await
            .unwrap()
            .name,
        "Latest signed name"
    );
    let intruder = RemoteUser::new("songhive.test", "intruder");
    let wrong = plamenu::remote::refresh_remote_actor(&state, &intruder.actor)
        .await
        .unwrap();
    music_remote::incoming(
        &state,
        &wrong,
        &json!({"type":"Delete","actor":intruder.actor.id,"object":ROOT}),
    )
    .await
    .unwrap();
    assert!(music_remote::get(&state, row.id, viewer).await.is_ok());
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Delete","actor":publisher.actor.id,"object":ROOT}),
    )
    .await
    .unwrap();
    assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
    assert!(matches!(
        music_remote::get(&state, row.id, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn addressed_private_updates_revoke_cache_and_confirmed_public_updates_restore_it(
    pool: PgPool,
) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    let row = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let sender = plamenu_db::account::find_by_uri(&state.pool, &publisher.actor.id)
        .await
        .unwrap()
        .unwrap();
    root["to"] = json!([publisher.actor.id]);
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Update","actor":publisher.actor.id,"object":root}),
    )
    .await
    .unwrap();
    assert!(music_remote::get(&state, row.id, viewer).await.is_err());
    root["to"] = json!([PUBLIC]);
    root["name"] = json!("Public again");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    let restored = music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert_eq!(restored.id, row.id);
    assert_eq!(restored.name, "Public again");
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn signed_embedded_updates_refresh_saved_tracks_but_cannot_write_another_controller(
    pool: PgPool,
) {
    let (state, viewer, publisher, _, mut root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    root["items"][0]["track"]["name"] = json!("Current signed track name");
    root["items"][0]["track"]["album"]["name"] = json!("Forged catalogue album name");
    assert_eq!(
        signed_update(&state, &publisher, root).await,
        axum::http::StatusCode::ACCEPTED
    );
    let saved = music_saved::page(&state, viewer, None).await.unwrap();
    assert_eq!(
        saved.items[0].source.as_ref().unwrap()["name"],
        "Current signed track name"
    );
    let album: i64 = sqlx::query_scalar(
        "SELECT id FROM music_remote_resources WHERE uri='https://songhive.test/albums/one'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        music_remote::get(&state, album, viewer).await.unwrap().name,
        "Origin album"
    );
    let cache: i64 = sqlx::query_scalar("SELECT count(*) FROM music_remote_resources")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(cache, 4);
}

async fn signed_update(
    state: &AppState,
    publisher: &RemoteUser,
    object: Value,
) -> axum::http::StatusCode {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let raw = json!({"id":format!("https://songhive.test/activities/{}",plamenu_db::id::next()),"type":"Update","actor":publisher.actor.id,"object":object});
    let body = serde_json::to_vec(&raw).unwrap();
    let signed = publisher.signer().sign_post(
        common::TEST_DOMAIN,
        "/inbox",
        &body,
        std::time::SystemTime::now(),
    );
    plamenu::build_router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/inbox")
                .header("host", signed.host)
                .header("date", signed.date)
                .header("digest", signed.digest)
                .header("signature", signed.signature)
                .header("content-type", "application/activity+json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_album_authority_overrides_embedded_credits_and_private_album_does_not_hide_track(
    pool: PgPool,
) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    root["items"][0]["track"]["album"]["attributedTo"] = json!(publisher.actor.id);
    root["items"][0]["track"]["album"]["name"] = json!("Embedded credit name");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let album: i64 = sqlx::query_scalar(
        "SELECT id FROM music_remote_resources WHERE uri='https://songhive.test/albums/one'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        music_remote::get(&state, album, viewer).await.unwrap().name,
        "Origin album"
    );
    let actor_uri:String=sqlx::query_scalar("SELECT a.uri FROM music_remote_resources r JOIN accounts a ON a.id=r.controller_id WHERE r.id=$1").bind(album).fetch_one(&state.pool).await.unwrap();
    assert_ne!(actor_uri, publisher.actor.id);
    stub.objects
        .lock()
        .unwrap()
        .remove("https://songhive.test/albums/one");
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert!(music_remote::get(&state, album, viewer).await.is_err());
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        music_remote::get(&state, track, viewer).await.unwrap().name,
        "Origin track"
    );
    let stored: (String, String, Option<String>) =
        sqlx::query_as("SELECT name,summary,audio_url FROM music_remote_resources WHERE id=$1")
            .bind(album)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(stored, (String::new(), String::new(), None));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn proven_permalink_resolves_canonical_audio_and_rejects_cross_origin_identity(pool: PgPool) {
    let (state, viewer, publisher, stub, _) = fixture(pool).await;
    let canonical = json!({"id":AUDIO,"type":"Audio","attributedTo":publisher.actor.id,"to":[PUBLIC],"name":"Canonical audio","track":{"id":TRACK,"type":"Track","name":"Track"}});
    stub.objects
        .lock()
        .unwrap()
        .insert(TRACK.into(), canonical.clone());
    stub.objects.lock().unwrap().insert(AUDIO.into(), canonical);
    assert_eq!(
        music_remote::import(&state, viewer, TRACK)
            .await
            .unwrap()
            .uri,
        AUDIO
    );
    stub.objects.lock().unwrap().insert("https://songhive.test/permalink".into(),json!({"id":"https://other.example/audio/one","type":"Audio","attributedTo":publisher.actor.id,"to":[PUBLIC]}));
    assert!(
        music_remote::import(&state, viewer, "https://songhive.test/permalink")
            .await
            .is_err()
    );
    assert!(
        !stub
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("https://other.example/"))
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn library_first_page_is_guarded_and_cached_contents_fold_renditions_once(pool: PgPool) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    let first = format!("{ROOT}?page=1");
    let next = format!("{ROOT}?page=2");
    let mut second = root["items"][0].clone();
    second["id"] = json!("https://songhive.test/audio/two");
    let items = json!([root["items"][0].clone(), second]);
    root.as_object_mut().unwrap().remove("items");
    root["first"] = json!(first);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    stub.objects.lock().unwrap().insert(first.clone(),json!({"id":first,"type":"CollectionPage","partOf":ROOT,"actor":publisher.actor.id,"orderedItems":items,"next":next}));
    let library = music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert!(!stub.fetches.lock().unwrap().contains(&next));
    let contents = music_remote::contents(&state, library.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(contents.items.len(), 1);
    assert_eq!(contents.items[0].uri, TRACK);
    assert!(contents.before.is_none());
    assert!(
        music_remote::contents(&state, library.id, viewer, Some(0))
            .await
            .is_err()
    );
    stub.objects.lock().unwrap().get_mut(&first).unwrap()["partOf"] =
        json!("https://songhive.test/unrelated");
    assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
    assert_eq!(
        music_remote::get(&state, library.id, viewer)
            .await
            .unwrap()
            .name,
        "Origin library"
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn confirmed_origin_refusal_redacts_cached_metadata_and_retains_private_bookmarks(
    pool: PgPool,
) {
    let (state, viewer, _, stub, _) = fixture(pool).await;
    let root = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: root.id.to_string(),
        },
    )
    .await
    .unwrap();
    stub.objects.lock().unwrap().remove(ROOT);
    assert!(matches!(
        music_remote::import(&state, viewer, ROOT).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(music_remote::get(&state, root.id, viewer).await.is_err());
    assert_eq!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0].id,
        saved.id
    );
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
}

async fn web_request(
    app: &axum::Router,
    cookie: &str,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> axum::response::Response {
    use axum::{
        body::Body,
        http::{Request, header},
    };
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn response_text(response: axum::response::Response) -> String {
    use http_body_util::BodyExt;
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
async fn login(state: &AppState, viewer: i64) -> (String, String) {
    use plamenu::auth::{hash_password, hash_secret};
    plamenu_db::user::create(
        &state.pool,
        viewer,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let response = web_request(
        &plamenu::build_router(state.clone()),
        "",
        "POST",
        "/login",
        &[
            ("email", "alice@plamenu.test"),
            ("password", "fixture-password"),
        ],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    (cookie, csrf)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_remote_lookup_is_csrf_protected_and_saved_music_stays_in_single_workspace(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    let (state, viewer, _, stub, _) = fixture(pool).await;
    let (cookie, csrf) = login(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let forbidden = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/remote/import",
        &[("csrf", "invalid"), ("url", ROOT)],
    )
    .await;
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert!(stub.fetches.lock().unwrap().is_empty());
    let response = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/remote/import",
        &[("csrf", &csrf), ("url", ROOT)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let path = response.headers()["location"].to_str().unwrap().to_owned();
    let page = web_request(&app, &cookie, "GET", &path, &[]).await;
    assert_eq!(page.headers()["Cache-Control"], "private, no-store");
    assert_eq!(page.headers()["Referrer-Policy"], "no-referrer");
    let html = response_text(page).await;
    assert!(html.contains("Origin library"));
    assert!(html.contains("Origin track"));
    assert!(html.contains("Tracks"));
    assert!(html.contains("name=\"kind\" value=\"remote\""));
    let id = path.rsplit('/').next().unwrap();
    let saved = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/saved",
        &[
            ("csrf", &csrf),
            ("kind", "remote"),
            ("id", id),
            ("operation", "save"),
            ("return_to", &path),
        ],
    )
    .await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let mine =
        response_text(web_request(&app, &cookie, "GET", "/music/mine?kind=libraries", &[]).await)
            .await;
    assert!(mine.contains("Origin library"));
    assert!(mine.contains("Find remote music"));
    assert!(!mine.contains("href=\"/music/remote\" class=\"nav-link\""));
    let removed = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/saved",
        &[
            ("csrf", &csrf),
            ("kind", "remote"),
            ("id", id),
            ("operation", "remove"),
        ],
    )
    .await;
    assert_eq!(removed.status(), StatusCode::SEE_OTHER);
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
async fn api_tokens(state: &AppState, viewer: i64) -> (String, String) {
    use plamenu::auth::{generate_secret, hash_secret};
    use plamenu_db::{oauth, user};
    let account = user::find_by_account_id(&state.pool, viewer)
        .await
        .unwrap()
        .unwrap();
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "remote-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let mut tokens = Vec::new();
    for scope in ["read:statuses", "write:statuses"] {
        let token = generate_secret();
        oauth::create_token(
            &state.pool,
            &hash_secret(&token),
            app.id,
            Some(account.id),
            scope,
        )
        .await
        .unwrap();
        tokens.push(token);
    }
    (tokens.remove(0), tokens.remove(0))
}
async fn api_request(
    app: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    input: Value,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_api_requires_separate_read_write_scopes_and_never_exposes_file_urls(pool: PgPool) {
    use axum::http::StatusCode;
    let (state, viewer, _, stub, _) = fixture(pool).await;
    login(&state, viewer).await;
    let (read, write) = api_tokens(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let invalid = api_request(
        &app,
        &read,
        "POST",
        "/api/v1/music/remote/import",
        json!({"url":ROOT}),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::FORBIDDEN);
    assert!(stub.fetches.lock().unwrap().is_empty());
    let imported = api_request(
        &app,
        &write,
        "POST",
        "/api/v1/music/remote/import",
        json!({"url":ROOT}),
    )
    .await;
    assert_eq!(imported.status(), StatusCode::OK);
    assert_eq!(imported.headers()["Vary"], "Authorization, Cookie");
    let row: Value = serde_json::from_str(&response_text(imported).await).unwrap();
    assert!(row["id"].is_string());
    assert!(row["fetched_at"].is_string());
    let path = format!(
        "/api/v1/music/remote/{}/contents",
        row["id"].as_str().unwrap()
    );
    assert_eq!(
        api_request(&app, &write, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let contents = api_request(&app, &read, "GET", &path, Value::Null).await;
    assert_eq!(contents.status(), StatusCode::OK);
    assert_eq!(contents.headers()["Cache-Control"], "private, no-store");
    let json = response_text(contents).await;
    assert!(!json.contains("audio_url"));
    assert!(!json.contains("controller_id"));
    assert!(!json.contains("files/one.mp3"));
    let page: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}

async fn playback_fixture(pool: PgPool) -> (AppState, i64, String, Arc<StubFederation>, i64) {
    let (state, viewer, _, stub, _) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    login(&state, viewer).await;
    let (read, _) = api_tokens(&state, viewer).await;
    stub.media.lock().unwrap().insert(
        "https://songhive.test/files/one.mp3".into(),
        plamenu_federation::FetchedMedia {
            final_url: "https://songhive.test/files/one.mp3".into(),
            content_type: "audio/mpeg".into(),
            bytes: b"0123456789abcdef".to_vec(),
        },
    );
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    (state, viewer, read, stub, track)
}
async fn audio_request(
    app: &axum::Router,
    token: Option<&str>,
    id: i64,
    range: Option<&str>,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let mut request = Request::builder().uri(format!("/music/remote/{id}/stream"));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_track_playback_selects_proven_rendition_and_preserves_ranges_without_listens(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    let (state, _, read, stub, track) = playback_fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    assert_eq!(
        audio_request(&app, None, track, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(stub.media_fetches.lock().unwrap().is_empty());
    let response = audio_request(&app, Some(&read), track, Some("bytes=2-7")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["Content-Range"], "bytes 2-7/16");
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(response.headers()["Content-Type"], "audio/mpeg");
    assert_eq!(response_text(response).await, "234567");
    let response = audio_request(&app, Some(&read), track, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_text(response).await, "0123456789abcdef");
    let listens: i64 = sqlx::query_scalar("SELECT count(*) FROM music_listen_sessions")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(listens, 0);
    assert_eq!(
        stub.enclosure_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn active_remote_body_stops_on_source_delete_while_origin_is_stalled(pool: PgPool) {
    use http_body_util::BodyExt;
    use std::sync::atomic::Ordering;
    let (state, _, read, stub, track) = playback_fixture(pool).await;
    *stub.enclosure_chunking.lock().unwrap() = Some((4, 3000));
    let response = audio_request(
        &plamenu::build_router(state.clone()),
        Some(&read),
        track,
        None,
    )
    .await;
    let mut body = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "0123");
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),changed_at=clock_timestamp(),name='',summary='',audio_url=NULL WHERE uri=$1").bind(AUDIO).execute(&state.pool).await.unwrap();
    let denied = tokio::time::timeout(std::time::Duration::from_millis(1500), body.frame())
        .await
        .unwrap()
        .unwrap();
    assert!(denied.is_err());
    drop(body);
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 0);
    assert_eq!(stub.enclosure_chunks_sent.load(Ordering::SeqCst), 1);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn active_remote_body_rechecks_token_revocation_and_releases_listener_on_drop(pool: PgPool) {
    use http_body_util::BodyExt;
    use std::sync::atomic::Ordering;
    let (state, _, read, stub, track) = playback_fixture(pool).await;
    *stub.enclosure_chunking.lock().unwrap() = Some((4, 3000));
    let response = audio_request(
        &plamenu::build_router(state.clone()),
        Some(&read),
        track,
        None,
    )
    .await;
    let mut body = response.into_body();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    sqlx::query("UPDATE oauth_tokens SET revoked_at=clock_timestamp() WHERE token_hash=$1")
        .bind(plamenu::auth::hash_secret(&read))
        .execute(&state.pool)
        .await
        .unwrap();
    let denied = tokio::time::timeout(std::time::Duration::from_millis(1500), body.frame())
        .await
        .unwrap()
        .unwrap();
    assert!(denied.is_err());
    drop(body);
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 0);
    assert_eq!(stub.enclosure_chunks_sent.load(Ordering::SeqCst), 1);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_listener_limit_is_finite_and_disconnection_admits_another_listener(pool: PgPool) {
    use axum::http::StatusCode;
    use std::sync::atomic::Ordering;
    let (state, _, read, stub, track) = playback_fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    let mut responses = Vec::new();
    for _ in 0..16 {
        let response = audio_request(&app, Some(&read), track, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 16);
    assert_eq!(
        audio_request(&app, Some(&read), track, None).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(responses.pop());
    let response = audio_request(&app, Some(&read), track, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    drop(responses);
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn deleting_audio_hides_derived_saved_track_but_keeps_independent_album_authority(
    pool: PgPool,
) {
    let (state, viewer, publisher, _, _) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    let sender = plamenu_db::account::find_by_uri(&state.pool, &publisher.actor.id)
        .await
        .unwrap()
        .unwrap();
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Delete","actor":publisher.actor.id,"object":AUDIO}),
    )
    .await
    .unwrap();
    assert!(music_remote::get(&state, track, viewer).await.is_err());
    assert!(
        music_remote::contents(&state, saved.reference.id.parse().unwrap(), viewer, None)
            .await
            .is_err()
    );
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
    let album: i64 = sqlx::query_scalar(
        "SELECT id FROM music_remote_resources WHERE uri='https://songhive.test/albums/one'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        music_remote::get(&state, album, viewer).await.unwrap().name,
        "Origin album"
    );
    music_saved::remove(&state, viewer, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn unknown_embedded_proof_does_not_break_signed_update_or_rewrite_known_track(pool: PgPool) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    root["items"][0]["id"] = json!("https://songhive.test/audio/new");
    root["items"][0]["track"]["name"] = json!("Fresh source track name");
    assert_eq!(
        signed_update(&state, &publisher, root.clone()).await,
        axum::http::StatusCode::ACCEPTED
    );
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        music_remote::get(&state, track, viewer).await.unwrap().name,
        "Origin track"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_remote_resources")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 4);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert_eq!(
        music_remote::get(&state, track, viewer).await.unwrap().name,
        "Fresh source track name"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn explicit_remote_page_lookup_resolves_parent_and_advances_only_on_user_request(
    pool: PgPool,
) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    let first = format!("{ROOT}?page=1");
    let second = format!("{ROOT}?page=2");
    let third = format!("{ROOT}?page=3");
    let original = root["items"][0].clone();
    let mut more = original.clone();
    more["id"] = json!("https://songhive.test/audio/more");
    more["name"] = json!("Second recording");
    more["track"]["id"] = json!("https://songhive.test/tracks/more");
    more["track"]["name"] = json!("Second track");
    root.as_object_mut().unwrap().remove("items");
    root["first"] = json!(first);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    stub.objects.lock().unwrap().insert(
        first.clone(),
        json!({"id":first,"type":"CollectionPage","partOf":ROOT,"items":[original],"next":second}),
    );
    stub.objects.lock().unwrap().insert(second.clone(),json!({"id":second,"type":"CollectionPage","partOf":ROOT,"actor":publisher.actor.id,"items":[more],"next":third}));
    let library = music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert_eq!(library.next_page_uri.as_deref(), Some(second.as_str()));
    assert!(!stub.fetches.lock().unwrap().contains(&second));
    let loaded = music_remote::import(&state, viewer, &second).await.unwrap();
    assert_eq!(loaded.id, library.id);
    assert_eq!(loaded.next_page_uri.as_deref(), Some(third.as_str()));
    assert!(!stub.fetches.lock().unwrap().contains(&third));
    let contents = music_remote::contents(&state, library.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(contents.items.len(), 2);
    assert!(
        contents
            .items
            .iter()
            .any(|item| item.name == "Second track")
    );
    let repeated = music_remote::import(&state, viewer, &second).await.unwrap();
    assert_eq!(repeated.id, library.id);
    assert_eq!(
        music_remote::contents(&state, library.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    let (cookie, _) = login(&state, viewer).await;
    let html = response_text(
        web_request(
            &plamenu::build_router(state.clone()),
            &cookie,
            "GET",
            &format!("/music/remote/{}", library.id),
            &[],
        )
        .await,
    )
    .await;
    assert!(html.contains("Load more from origin"));
    assert!(html.contains(&third));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn explicit_remote_pages_refuse_foreign_parent_and_unvalidated_next_before_fetch(
    pool: PgPool,
) {
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    let page = format!("{ROOT}?page=2");
    stub.objects.lock().unwrap().insert(page.clone(),json!({"id":page,"type":"CollectionPage","partOf":"https://other.example/library","items":[]}));
    assert!(music_remote::import(&state, viewer, &page).await.is_err());
    assert!(
        !stub
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("https://other.example/"))
    );
    let first = format!("{ROOT}?page=1");
    root["first"] = json!(first);
    root.as_object_mut().unwrap().remove("items");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    stub.objects.lock().unwrap().insert(first.clone(),json!({"id":first,"type":"CollectionPage","partOf":ROOT,"items":[],"next":"https://other.example/page"}));
    assert!(music_remote::import(&state, viewer, ROOT).await.is_err());
    assert!(
        !stub
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("https://other.example/"))
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_remote_resources")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_metadata_is_typed_inherited_displayed_and_redacted_on_withdrawal(pool: PgPool) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    root["items"][0]["duration"] = json!("PT1M12S");
    root["items"][0]["tag"] = json!([{"type":"Hashtag","name":"#Ambient"}]);
    let track = &mut root["items"][0]["track"];
    track["artists"] = json!([{"name":"Remote musician"}]);
    track["album"]["released"] = json!("2025-01-01");
    track["position"] = json!(3);
    track["disc"] = json!(2);
    track["musicbrainzId"] = json!("6db39f0b-dc09-4dad-a77a-4bc5a6f27977");
    track["image"] = json!({"url":"https://songhive.test/cover.jpg"});
    track["secret"] = json!("Never cache this field");
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), root.clone());
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let resource = music_remote::get(&state, id, viewer).await.unwrap();
    assert_eq!(resource.details.artist, "Remote musician");
    assert_eq!(resource.details.album, "Origin album");
    assert_eq!(resource.details.release_year, Some(2025));
    assert_eq!(resource.details.duration, Some(72.0));
    assert_eq!(resource.details.tags, vec!["ambient"]);
    let entity = resource.entity();
    assert_eq!(entity["details"]["disc_number"], 2);
    assert!(entity["details"].get("image_url").is_none());
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: id.to_string(),
        },
    )
    .await
    .unwrap();
    for query in ["Remote musician", "Origin album", "ambient"] {
        let page = music_mine::page(
            &state,
            viewer,
            &music_mine::Request {
                kind: music_mine::Filter::Tracks,
                q: query.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.items.len(), 1, "Search failed for {query}");
        assert_eq!(
            page.items[0].source["image_url"],
            format!("/music/remote/{id}/artwork")
        );
    }
    assert!(
        music_mine::page(
            &state,
            viewer,
            &music_mine::Request {
                q: "Never cache".into(),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    let stored: Value =
        sqlx::query_scalar("SELECT to_jsonb(details) FROM music_remote_resources WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(!stored.to_string().contains("Never cache"));
    assert_eq!(stored["image_url"], "https://songhive.test/cover.jpg");
    let (cookie, _) = login(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let html =
        response_text(web_request(&app, &cookie, "GET", &format!("/music/remote/{id}"), &[]).await)
            .await;
    let html = html.replace(['\u{2068}', '\u{2069}'], "");
    for text in [
        "Remote musician",
        "Origin album",
        "2025",
        "Track number",
        "Disc number",
        "ambient",
        "Playable duration: 72 seconds",
    ] {
        assert!(html.contains(text), "Missing {text}");
    }
    assert!(!html.contains("https://songhive.test/cover.jpg"));
    root["items"][0]["track"]["position"] = json!(4);
    root["items"][0]["duration"] = json!(80);
    assert_eq!(
        signed_update(&state, &publisher, root).await,
        axum::http::StatusCode::ACCEPTED
    );
    let current = music_remote::get(&state, id, viewer).await.unwrap();
    assert_eq!(current.details.track_number, Some(4));
    assert_eq!(current.details.duration, Some(80.0));
    let owner: i64 =
        sqlx::query_scalar("SELECT controller_id FROM music_remote_resources WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    let details: Value =
        sqlx::query_scalar("SELECT to_jsonb(details) FROM music_remote_resources WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(
        details
            .as_object()
            .unwrap()
            .values()
            .all(|value| value.is_null()
                || value.as_str() == Some("")
                || value.as_array().is_some_and(Vec::is_empty))
    );
    assert!(matches!(
        music_remote::get(&state, id, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_artwork_is_decoded_protected_and_never_embeds_origin_url(pool: PgPool) {
    use axum::{body::to_bytes, http::StatusCode};
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    let url = "https://songhive.test/cover.png";
    root["items"][0]["track"]["image"] = json!({"type":"Image","url":url});
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let (cookie, _) = login(&state, viewer).await;
    let (read, write) = api_tokens(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/remote/{id}/artwork");
    let image = image::RgbImage::from_pixel(64, 32, image::Rgb([60, 80, 120]));
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    stub.serve_media(url, "text/html", output.into_inner());
    assert_eq!(
        api_request(&app, &write, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(stub.media_fetches.lock().unwrap().is_empty());
    let response = api_request(&app, &read, "GET", &path, Value::Null).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
    assert_eq!(response.headers()["Content-Type"], "image/jpeg");
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (64, 32));
    let html =
        response_text(web_request(&app, &cookie, "GET", &format!("/music/remote/{id}"), &[]).await)
            .await;
    assert!(html.contains(&path));
    assert!(!html.contains(url));
    stub.serve_media(
        url,
        "image/png",
        b"<svg><script>danger</script></svg>".to_vec(),
    );
    assert_eq!(
        api_request(&app, &read, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    stub.serve_media(url, "image/png", vec![0; 8 * 1024 * 1024 + 1]);
    assert_eq!(
        api_request(&app, &read, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    let count = stub.media_fetches.lock().unwrap().len();
    assert_eq!(
        api_request(&app, &read, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(stub.media_fetches.lock().unwrap().len(), count);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_artwork_rechecks_source_and_token_after_origin_fetch(pool: PgPool) {
    use axum::http::StatusCode;
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    let url = "https://songhive.test/cover.png";
    root["items"][0]["track"]["image"] = json!({"url":url});
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    login(&state, viewer).await;
    let (read, _) = api_tokens(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let image = image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3]));
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    stub.serve_media(url, "image/png", output.into_inner());
    for revoke_token in [false, true] {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *stub.media_fetch_gate.lock().unwrap() = Some(gate.clone());
        let expected = stub.media_fetches.lock().unwrap().len() + 1;
        let app = app.clone();
        let token = read.clone();
        let path = format!("/music/remote/{id}/artwork");
        let request =
            tokio::spawn(async move { api_request(&app, &token, "GET", &path, Value::Null).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stub.media_fetches.lock().unwrap().len() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if revoke_token {
            sqlx::query("UPDATE oauth_tokens SET revoked_at=now() WHERE token_hash=$1")
                .bind(plamenu::auth::hash_secret(&read))
                .execute(&state.pool)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE music_remote_track_proofs SET details.image_url=$2 WHERE uri=(SELECT uri FROM music_remote_resources WHERE id=$1)").bind(id).bind("https://songhive.test/new-cover.png").execute(&state.pool).await.unwrap();
        }
        gate.add_permits(1);
        assert_eq!(request.await.unwrap().status(), StatusCode::NOT_FOUND);
        sqlx::query("UPDATE music_remote_track_proofs SET details.image_url=$2 WHERE uri=(SELECT uri FROM music_remote_resources WHERE id=$1)").bind(id).bind(url).execute(&state.pool).await.unwrap();
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_album_cover_fallback_rechecks_its_independent_authority(pool: PgPool) {
    use axum::http::StatusCode;
    let (state, viewer, _, stub, _) = fixture(pool).await;
    let album_uri = "https://songhive.test/albums/one";
    let url = "https://songhive.test/album-cover.png";
    stub.objects.lock().unwrap().get_mut(album_uri).unwrap()["image"] = json!({"url":url});
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let root = music_remote::get(&state, id, viewer).await.unwrap();
    assert_eq!(root.details.image_url.as_deref(), Some(url));
    let stored: Value =
        sqlx::query_scalar("SELECT to_jsonb(details) FROM music_remote_resources WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(
        stored["image_url"].is_null(),
        "Cover is a current reference, not a copied credit"
    );
    let (cookie, _) = login(&state, viewer).await;
    let (read, _) = api_tokens(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/remote/{id}/artwork");
    let image = image::RgbImage::from_pixel(32, 32, image::Rgb([60, 80, 120]));
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    stub.serve_media(url, "image/png", output.into_inner());
    assert_eq!(
        api_request(&app, &read, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    let album_owner: i64 =
        sqlx::query_scalar("SELECT controller_id FROM music_remote_resources WHERE uri=$1")
            .bind(album_uri)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO blocks(id,account_id,target_account_id) VALUES($1,$2,$3)")
        .bind(plamenu_db::id::next())
        .bind(viewer)
        .bind(album_owner)
        .execute(&state.pool)
        .await
        .unwrap();
    let current = music_remote::get(&state, id, viewer).await.unwrap();
    assert_eq!(current.name, "Origin track");
    assert!(current.artwork_url().is_none());
    let fetches = stub.media_fetches.lock().unwrap().len();
    assert_eq!(
        api_request(&app, &read, "GET", &path, Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(stub.media_fetches.lock().unwrap().len(), fetches);
    let html =
        response_text(web_request(&app, &cookie, "GET", &format!("/music/remote/{id}"), &[]).await)
            .await;
    assert!(!html.contains(&path));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_cache_and_saved_erasure_roll_back_and_retire_without_deleted_marker(pool: PgPool) {
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    root["items"][0]["track"]["artists"] = json!([{"name":"Private retained credit"}]);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    let resource = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = resource.controller_id;
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: resource.id.to_string(),
        },
    )
    .await
    .unwrap();
    let mut tx = state.pool.begin().await.unwrap();
    plamenu_db::music::retire_account_assets(&mut tx, owner)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(music_remote::get(&state, resource.id, viewer).await.is_ok());
    assert_eq!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0].id,
        saved.id
    );
    let mut tx = state.pool.begin().await.unwrap();
    plamenu_db::music::retire_account_assets(&mut tx, viewer)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(music_remote::get(&state, resource.id, viewer).await.is_ok());
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: resource.id.to_string(),
        },
    )
    .await
    .unwrap();
    let mut tx = state.pool.begin().await.unwrap();
    plamenu_db::music::retire_account_assets(&mut tx, owner)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM accounts WHERE id=$1")
            .bind(owner)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(!deleted);
    let leaked:i64=sqlx::query_scalar("SELECT count(*) FROM music_remote_resources WHERE controller_id=$1 AND (details IS DISTINCT FROM ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details OR name!='' OR summary!='' OR audio_url IS NOT NULL OR next_page_uri IS NOT NULL OR tombstoned_at IS NULL)").bind(owner).fetch_one(&state.pool).await.unwrap();
    assert_eq!(leaked, 0);
    assert!(matches!(
        music_remote::get(&state, resource.id, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
}

fn remote_source(id: i64) -> plamenu::music_queues::Source {
    plamenu::music_queues::Source {
        remote_id: Some(id.to_string()),
        asset_id: None,
        status_id: None,
        episode_id: None,
    }
}
async fn playable_remote_fixture(pool: PgPool) -> (AppState, i64, Arc<StubFederation>, i64, i64) {
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    root["items"][0]["duration"] = json!(10);
    root["items"][0]["track"]["artists"] = json!([{"name":"Origin performer"}]);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let audio: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(AUDIO)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    (state, viewer, stub, audio, track)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_queue_folds_audio_identity_reloads_seeks_and_hides_revoked_sources(pool: PgPool) {
    use plamenu::music_queues::{self, Action, Change};
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let empty = music_queues::get(&state, viewer, "remote-device")
        .await
        .unwrap();
    let queue = music_queues::change(
        &state,
        viewer,
        &empty.device,
        &Change {
            revision: empty.revision,
            action: Action::Add {
                sources: vec![remote_source(audio)],
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 1);
    assert_eq!(queue.entries[0].title, "Origin track");
    assert_eq!(queue.entries[0].artist, "Origin performer");
    assert!((queue.entries[0].duration - 10.0).abs() < f64::EPSILON);
    let stored: Option<i64> = sqlx::query_scalar("SELECT remote_id FROM music_queue_entries")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stored, Some(track));
    let selected = music_queues::change(
        &state,
        viewer,
        &queue.device,
        &Change {
            revision: queue.revision,
            action: Action::Select {
                entry_id: queue.entries[0].entry_id.clone(),
            },
        },
    )
    .await
    .unwrap();
    let seek = music_queues::change(
        &state,
        viewer,
        &selected.device,
        &Change {
            revision: selected.revision,
            action: Action::Seek {
                seconds: 4.0,
                entry_id: selected.entries[0].entry_id.clone(),
            },
        },
    )
    .await
    .unwrap();
    let reload = music_queues::get(&state, viewer, &seek.device)
        .await
        .unwrap();
    assert!((reload.position_seconds - 4.0).abs() < f64::EPSILON);
    assert!(
        reload.entries[0]
            .stream_url
            .contains(&format!("/remote/{track}/stream"))
    );
    let mut invalid = remote_source(track);
    invalid.status_id = Some("1".into());
    assert!(matches!(
        music_queues::change(
            &state,
            viewer,
            &reload.device,
            &Change {
                revision: reload.revision.clone(),
                action: Action::Add {
                    sources: vec![invalid]
                }
            }
        )
        .await,
        Err(plamenu::error::ApiError::Unprocessable(_))
    ));
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    let hidden = music_queues::get(&state, viewer, &reload.device)
        .await
        .unwrap();
    assert!(hidden.entries.is_empty());
    assert!(hidden.current_entry_id.is_none());
    assert_eq!(hidden.revision, reload.revision);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_favourites_fold_renditions_and_keep_distinct_null_local_sources(pool: PgPool) {
    use plamenu::music_favourites;
    let (state, viewer, stub, audio, track) = playable_remote_fixture(pool).await;
    let first = music_favourites::save(&state, viewer, &remote_source(audio))
        .await
        .unwrap();
    let again = music_favourites::save(&state, viewer, &remote_source(track))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(first.remote_id, Some(track.to_string()));
    let second_uri = "https://songhive.test/tracks/two";
    let actor = stub.objects.lock().unwrap().get(ROOT).unwrap()["actor"].clone();
    let second_doc = json!({"id":"https://songhive.test/audio/two","type":"Audio","attributedTo":actor,"to":[PUBLIC],"duration":20,"attachment":[{"type":"Audio","mediaType":"audio/mpeg","url":"https://songhive.test/files/two.mp3"}],"track":{"id":second_uri,"type":"Track","name":"Second track"}});
    stub.objects
        .lock()
        .unwrap()
        .insert("https://songhive.test/audio/two".into(), second_doc);
    music_remote::import(&state, viewer, "https://songhive.test/audio/two")
        .await
        .unwrap();
    let second: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(second_uri)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let saved_second = music_favourites::save(&state, viewer, &remote_source(second))
        .await
        .unwrap();
    assert_ne!(saved_second.id, first.id);
    assert_eq!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    assert_eq!(
        music_mine::page(&state, viewer, &music_mine::Request::default())
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    music_favourites::remove(&state, viewer, &remote_source(audio))
        .await
        .unwrap();
    assert!(
        !music_favourites::contains(&state, viewer, &remote_source(track))
            .await
            .unwrap()
    );
    assert!(
        music_favourites::contains(&state, viewer, &remote_source(second))
            .await
            .unwrap()
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE id=$1")
        .bind(second)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    music_favourites::remove(&state, viewer, &remote_source(second))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_listening_is_explicit_canonical_deduplicated_and_currently_authorized(
    pool: PgPool,
) {
    use plamenu::music_listening::{self, Progress, Start};
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let input = Start {
        remote_id: Some(audio.to_string()),
        asset_id: None,
        status_id: None,
        episode_id: None,
        queue_device: None,
        queue_entry_id: None,
        request_key: "remote-play".into(),
    };
    let started = music_listening::start(&state, viewer, &input)
        .await
        .unwrap();
    let replay = music_listening::start(
        &state,
        viewer,
        &Start {
            remote_id: Some(track.to_string()),
            ..input
        },
    )
    .await
    .unwrap();
    assert_eq!(replay.token, started.token);
    assert!(
        music_listening::history(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let token = started.token.as_deref().unwrap();
    let immediate = music_listening::progress(
        &state,
        viewer,
        token,
        &Progress {
            played_seconds: 6.0,
            finish: false,
        },
    )
    .await
    .unwrap();
    assert!(!immediate.counted);
    sqlx::query("UPDATE music_listen_sessions SET last_report_at=clock_timestamp()-interval '10 seconds',reported_seconds=0 WHERE token=$1").bind(token).execute(&state.pool).await.unwrap();
    let counted = music_listening::progress(
        &state,
        viewer,
        token,
        &Progress {
            played_seconds: 6.0,
            finish: false,
        },
    )
    .await
    .unwrap();
    assert!(counted.counted);
    let history = music_listening::history(&state, viewer, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].track.title, "Origin track");
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT remote_id FROM music_listen_sessions WHERE token=$1"
        )
        .bind(token)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        Some(track)
    );
    assert!(
        music_listening::progress(
            &state,
            viewer,
            token,
            &Progress {
                played_seconds: 6.0,
                finish: false
            }
        )
        .await
        .unwrap()
        .counted
    );
    assert_eq!(
        music_listening::history(&state, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_listening::progress(
            &state,
            viewer,
            token,
            &Progress {
                played_seconds: 9.0,
                finish: true
            }
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_listening::history(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_remote_player_and_favourite_forms_use_exact_csrf_and_existing_workspace(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    let (state, viewer, _, _, track) = playable_remote_fixture(pool).await;
    let (cookie, csrf) = login(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    let source = track.to_string();
    let path = format!("/music/remote/{track}");
    let html = response_text(web_request(&app, &cookie, "GET", &path, &[]).await).await;
    assert!(html.contains(&format!("/music/player?remote_id={track}")));
    assert!(html.contains("name=\"remote_id\""));
    let invalid = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/favourites",
        &[
            ("csrf", "invalid"),
            ("remote_id", &source),
            ("operation", "save"),
        ],
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    let saved = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/favourites",
        &[
            ("csrf", &csrf),
            ("remote_id", &source),
            ("operation", "save"),
        ],
    )
    .await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let favourites =
        response_text(web_request(&app, &cookie, "GET", "/music/favourites", &[]).await).await;
    assert!(favourites.contains(&format!("/music/player?remote_id={track}")));
    assert!(favourites.contains("Origin track"));
    let preview = response_text(
        web_request(
            &app,
            &cookie,
            "GET",
            &format!("/music/player?remote_id={track}"),
            &[],
        )
        .await,
    )
    .await;
    assert!(preview.contains("name=\"remote_id\""));
    let queued = web_request(
        &app,
        &cookie,
        "POST",
        "/web/music/player",
        &[
            ("csrf", &csrf),
            ("device", "web"),
            ("revision", "0"),
            ("action", "add"),
            ("remote_id", &source),
        ],
    )
    .await;
    assert_eq!(queued.status(), StatusCode::SEE_OTHER);
    let player = response_text(web_request(&app, &cookie, "GET", "/music/player", &[]).await).await;
    assert!(player.contains("Origin track"));
    assert!(!player.contains("https://songhive.test/files/one.mp3"));
}

async fn counted_remote_listen(
    state: &AppState,
    viewer: i64,
    remote: Option<i64>,
    status: Option<i64>,
    hour: i32,
) {
    let id = plamenu_db::id::next();
    sqlx::query("INSERT INTO music_listen_sessions(id,token,account_id,request_key,remote_id,status_id,duration,threshold_seconds,played_seconds,counted_at,history_id) VALUES($1,$2,$3,$4,$5,$6,10,5,6,'2026-03-08T00:00:00Z'::timestamptz+$7::integer*interval '1 hour',$8)")
        .bind(id).bind(format!("{id:064x}")).bind(viewer).bind(format!("counted_{id}")).bind(remote).bind(status).bind(hour).bind(plamenu_db::id::next()).execute(&state.pool).await.unwrap();
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_statistics_keep_status_id_namespace_and_current_canonical_credit_links(
    pool: PgPool,
) {
    use plamenu::music_stats::{self, ReleaseGroup, Request};
    let (state, viewer, stub, _, track) = playable_remote_fixture(pool).await;
    let mut root = stub.objects.lock().unwrap().get(ROOT).unwrap().clone();
    let artist_uri = "https://songhive.test/artists/one";
    root["items"][0]["track"]["genre"] = json!("Ambient");
    root["items"][0]["track"]["album"]["released"] = json!("2021-01-01");
    root["items"][0]["track"]["artists"] =
        json!([{"type":"Artist","id":artist_uri,"name":"Origin performer"}]);
    let catalogue = stub
        .objects
        .lock()
        .unwrap()
        .get("https://songhive.test/albums/one")
        .unwrap()["attributedTo"]
        .clone();
    stub.objects.lock().unwrap().insert(
        artist_uri.into(),
        json!({"id":artist_uri,"type":"Artist","name":"Origin performer","attributedTo":catalogue}),
    );
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), root.clone());
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let author = plamenu_db::account::find_by_uri(&state.pool, root["actor"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    let doc = json!({"id":"https://songhive.test/social/independent","type":"Audio","attributedTo":root["actor"],"name":"Origin track","to":[PUBLIC],"url":[{"type":"Link","mediaType":"audio/mpeg","href":"https://songhive.test/independent.mp3"}],"track":{"name":"Origin track","artists":[{"name":"Origin performer"}],"album":{"name":"Origin album","released":"2021-01-01"}}});
    let post = plamenu::ingest::ingest_remote_note(&state, &author, &doc)
        .await
        .unwrap();
    // Numeric IDs in different tables do not establish shared source identity.
    sqlx::query("UPDATE music_remote_resources SET id=$1 WHERE id=$2")
        .bind(post.id)
        .bind(track)
        .execute(&state.pool)
        .await
        .unwrap();
    counted_remote_listen(&state, viewer, Some(post.id), None, 12).await;
    counted_remote_listen(&state, viewer, Some(post.id), None, 13).await;
    counted_remote_listen(&state, viewer, None, Some(post.id), 14).await;
    let args = Request {
        from: Some("2026-03-01".into()),
        to: Some("2026-03-31".into()),
        tz: Some("UTC".into()),
        release_group: ReleaseGroup::Year,
        ..Default::default()
    };
    let report = music_stats::report(&state, viewer, &args).await.unwrap();
    assert_eq!(report.play_count, 3);
    assert_eq!(report.top.tracks.len(), 2);
    let remote = report
        .top
        .tracks
        .iter()
        .find(|entry| entry.url.as_deref() == Some(format!("/music/remote/{}", post.id).as_str()))
        .unwrap();
    assert_eq!(remote.play_count, 2);
    assert!(
        remote
            .artist_url
            .as_deref()
            .is_some_and(|url| url.starts_with("/music/remote/"))
    );
    assert_eq!(report.clock.iter().map(|hour| hour.count).sum::<i64>(), 3);
    assert_eq!(
        report.plays.iter().map(|bucket| bucket.count).sum::<i64>(),
        3
    );
    assert!(
        report
            .top
            .genres
            .iter()
            .any(|entry| entry.name == "ambient" && entry.play_count == 2)
    );
    assert_eq!(
        report
            .releases
            .iter()
            .find(|bucket| bucket.bucket == "2021")
            .unwrap()
            .count,
        3
    );
    sqlx::query("INSERT INTO blocks(id,account_id,target_account_id) VALUES($1,$2,$3)")
        .bind(plamenu_db::id::next())
        .bind(viewer)
        .bind(author.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let hidden = music_stats::report(&state, viewer, &args).await.unwrap();
    assert_eq!(hidden.play_count, 0);
    assert!(hidden.top.tracks.is_empty());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn inherited_audio_audience_tracks_library_withdrawal_but_explicit_public_audio_survives(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    root["items"][0].as_object_mut().unwrap().remove("to");
    let mut explicit = root["items"][0].clone();
    explicit["id"] = json!("https://songhive.test/audio/explicit");
    explicit["to"] = json!([PUBLIC]);
    explicit["track"]["id"] = json!("https://songhive.test/tracks/explicit");
    explicit["track"]["name"] = json!("Independent public track");
    root["items"].as_array_mut().unwrap().push(explicit);
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), root.clone());
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let inherited: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let explicit: i64 = sqlx::query_scalar(
        "SELECT id FROM music_remote_resources WHERE uri='https://songhive.test/tracks/explicit'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    for id in [inherited, explicit] {
        music_saved::save(
            &state,
            viewer,
            &Reference {
                kind: Kind::Remote,
                id: id.to_string(),
            },
        )
        .await
        .unwrap();
    }
    let proof: (String, String) =
        sqlx::query_as("SELECT proof_uri,proof_kind FROM music_remote_resources WHERE uri=$1")
            .bind(AUDIO)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(proof, (ROOT.into(), "library".into()));
    login(&state, viewer).await;
    let (read, _) = api_tokens(&state, viewer).await;
    let app = plamenu::build_router(state.clone());
    root["to"] = json!([publisher.actor.id]);
    assert_eq!(
        signed_update(&state, &publisher, root).await,
        StatusCode::ACCEPTED
    );
    assert!(matches!(
        music_remote::get(&state, inherited, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert_eq!(
        music_remote::get(&state, explicit, viewer)
            .await
            .unwrap()
            .name,
        "Independent public track"
    );
    let before = stub.media_fetches.lock().unwrap().len();
    assert_eq!(
        audio_request(&app, Some(&read), inherited, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(stub.media_fetches.lock().unwrap().len(), before);
    stub.serve_media(
        "https://songhive.test/files/one.mp3",
        "audio/mpeg",
        b"public audio".to_vec(),
    );
    let response = audio_request(&app, Some(&read), explicit, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"public audio");
    let saved = music_saved::page(&state, viewer, None).await.unwrap();
    assert_eq!(saved.items.len(), 2);
    assert_eq!(
        saved
            .items
            .iter()
            .filter(|item| item.source.is_none())
            .count(),
        1
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cached_proof_cycles_and_changed_proof_types_never_authorize_old_metadata(pool: PgPool) {
    let (state, viewer, stub, audio, track) = playable_remote_fixture(pool).await;
    sqlx::query("UPDATE music_remote_resources SET proof_uri=$1,proof_kind='track' WHERE id=$2")
        .bind(TRACK)
        .bind(audio)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_remote::get(&state, track, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(matches!(
        music_remote::get(&state, audio, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    let original = stub.objects.lock().unwrap().get(ROOT).unwrap()["items"][0].clone();
    stub.objects.lock().unwrap().insert(AUDIO.into(), original);
    music_remote::import(&state, viewer, AUDIO).await.unwrap();
    assert!(music_remote::get(&state, track, viewer).await.is_ok());
    sqlx::query("UPDATE music_remote_resources SET kind='library',proof_kind='library',media_of=NULL WHERE id=$1").bind(audio).execute(&state.pool).await.unwrap();
    assert!(music_remote::get(&state, audio, viewer).await.is_ok());
    assert!(matches!(
        music_remote::get(&state, track, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn legacy_library_audio_uses_canonical_publisher_and_can_be_looked_up_by_permalink(
    pool: PgPool,
) {
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    let canonical = root["items"][0].clone();
    let catalogue = canonical["track"]["album"]["attributedTo"].clone();
    root["actor"] = catalogue.clone();
    root["items"][0]["id"] = json!(TRACK);
    root["items"][0]["attributedTo"] = catalogue.clone();
    root["items"][0]["track"]["attributedTo"] = catalogue;
    root["items"][0]["name"] = json!("Library wrapper title");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    stub.objects
        .lock()
        .unwrap()
        .insert(TRACK.into(), canonical.clone());
    stub.objects.lock().unwrap().insert(AUDIO.into(), canonical);
    let library = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let items = music_remote::contents(&state, library.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(items.items.len(), 1);
    assert_eq!(items.items[0].name, "Origin track");
    assert_eq!(items.items[0].kind, "track");
    assert_eq!(items.items[0].uri, TRACK);
    assert_ne!(items.items[0].controller_id, library.controller_id);
    let lookup = music_remote::import(&state, viewer, TRACK).await.unwrap();
    assert_eq!(lookup.uri, AUDIO);
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: lookup.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(saved.reference.id, items.items[0].id.to_string());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT kind FROM music_remote_resources WHERE uri=$1")
            .bind(TRACK)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        "track"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn shared_remote_library_membership_is_independent_and_refresh_removes_only_that_parent(
    pool: PgPool,
) {
    let (state, viewer, _, stub, root) = fixture(pool).await;
    let first = music_remote::import(&state, viewer, ROOT).await.unwrap();
    let other_uri = "https://songhive.test/library/second";
    let mut other = root.clone();
    other["id"] = json!(other_uri);
    other["name"] = json!("Second library");
    stub.objects.lock().unwrap().insert(other_uri.into(), other);
    let second = music_remote::import(&state, viewer, other_uri)
        .await
        .unwrap();
    let first_items = music_remote::contents(&state, first.id, viewer, None)
        .await
        .unwrap();
    let second_items = music_remote::contents(&state, second.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(first_items.items.len(), 1);
    assert_eq!(second_items.items.len(), 1);
    assert_eq!(first_items.items[0].id, second_items.items[0].id);
    let target = first_items.items[0].id;
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: target.to_string(),
        },
    )
    .await
    .unwrap();
    let mut empty = root;
    empty["items"] = json!([]);
    stub.objects.lock().unwrap().insert(ROOT.into(), empty);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    assert!(
        music_remote::contents(&state, first.id, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        music_remote::contents(&state, second.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(music_remote::get(&state, target, viewer).await.is_ok());
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_some()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_member_pages_accumulate_lazily_and_exhausted_first_page_retires_old_later_pages(
    pool: PgPool,
) {
    let (state, viewer, _, stub, root) = fixture(pool).await;
    let mut index = root.clone();
    index["items"] = json!([]);
    index["first"] = json!("https://songhive.test/library?page=1");
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), index.clone());
    for page in 1..=3 {
        let mut audio = root["items"][0].clone();
        audio["id"] = json!(format!("https://songhive.test/audio/page-{page}"));
        audio["track"]["id"] = json!(format!("https://songhive.test/tracks/page-{page}"));
        audio["track"]["name"] = json!(format!("Page {page} track"));
        let uri = format!("https://songhive.test/library?page={page}");
        let mut body = json!({"id":uri,"type":"CollectionPage","partOf":ROOT,"items":[audio]});
        if page < 3 {
            body["next"] = json!(format!("https://songhive.test/library?page={}", page + 1));
        }
        stub.objects.lock().unwrap().insert(uri, body);
    }
    let library = music_remote::import(&state, viewer, ROOT).await.unwrap();
    music_remote::import(&state, viewer, "https://songhive.test/library?page=2")
        .await
        .unwrap();
    music_remote::import(&state, viewer, "https://songhive.test/library?page=3")
        .await
        .unwrap();
    assert_eq!(
        music_remote::contents(&state, library.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    let first_uri = "https://songhive.test/library?page=1";
    let mut first = stub.objects.lock().unwrap().get(first_uri).unwrap().clone();
    first.as_object_mut().unwrap().remove("next");
    stub.objects.lock().unwrap().insert(first_uri.into(), first);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let contents = music_remote::contents(&state, library.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(contents.items.len(), 1);
    assert_eq!(contents.items[0].name, "Page 1 track");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM music_remote_members WHERE parent_uri=$1"
        )
        .bind(ROOT)
        .fetch_one(&state.pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn withdrawing_one_rendition_selects_remaining_public_metadata_and_bytes(pool: PgPool) {
    use axum::http::StatusCode;
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    root["items"][0]["track"]["name"] = json!("Withdrawn snapshot title");
    root["items"][0]["track"]["summary"] = json!("Withdrawn snapshot description");
    let mut alternative = root["items"][0].clone();
    let alternative_uri = "https://songhive.test/audio/alternative";
    alternative["id"] = json!(alternative_uri);
    alternative["track"]["name"] = json!("Remaining public title");
    alternative["track"]["summary"] = json!("Remaining public description");
    alternative["attachment"][0]["url"] = json!("https://songhive.test/files/alternative.mp3");
    root["items"].as_array_mut().unwrap().push(alternative);
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_remote_track_proofs WHERE uri=$1")
            .bind(TRACK)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        2
    );
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    let sender = plamenu_db::account::find_by_uri(&state.pool, &publisher.actor.id)
        .await
        .unwrap()
        .unwrap();
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Delete","actor":publisher.actor.id,"object":AUDIO}),
    )
    .await
    .unwrap();
    let current = music_remote::get(&state, track, viewer).await.unwrap();
    assert_eq!(current.name, "Remaining public title");
    assert_eq!(current.summary, "Remaining public description");
    assert_eq!(current.proof_uri, alternative_uri);
    let saved = music_saved::page(&state, viewer, None).await.unwrap();
    assert_eq!(
        saved.items[0].source.as_ref().unwrap()["name"],
        "Remaining public title"
    );
    let (cookie, _) = login(&state, viewer).await;
    let (read, _) = api_tokens(&state, viewer).await;
    stub.serve_media(
        "https://songhive.test/files/alternative.mp3",
        "audio/mpeg",
        b"remaining bytes".to_vec(),
    );
    let app = plamenu::build_router(state.clone());
    let html = response_text(
        web_request(&app, &cookie, "GET", &format!("/music/remote/{track}"), &[]).await,
    )
    .await;
    assert!(html.contains("Remaining public title"));
    assert!(!html.contains("Withdrawn snapshot title"));
    let response = audio_request(&app, Some(&read), track, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .as_ref(),
        b"remaining bytes"
    );
    music_remote::incoming(
        &state,
        &sender,
        &json!({"type":"Delete","actor":publisher.actor.id,"object":alternative_uri}),
    )
    .await
    .unwrap();
    assert!(matches!(
        music_remote::get(&state, track, viewer).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
}

async fn remote_collection(
    state: &AppState,
    owner: i64,
    kind: &str,
    visibility: &str,
) -> plamenu::music_collections::Collection {
    plamenu::music_collections::create(
        state,
        owner,
        &plamenu::music_collections::New {
            name: "Remote listening room".into(),
            description: String::new(),
            kind: kind.into(),
            visibility: visibility.into(),
        },
    )
    .await
    .unwrap()
}
async fn add_remote_entry(state: &AppState, owner: i64, collection: i64, remote: i64) {
    let revision = plamenu::music_collections::get(state, collection, Some(owner))
        .await
        .unwrap()
        .revision;
    plamenu::music_collections::add(
        state,
        owner,
        collection,
        &plamenu::music_collections::Add {
            remote_id: Some(remote.to_string()),
            asset_id: None,
            status_id: None,
            episode_id: None,
            revision: revision.to_string(),
        },
    )
    .await
    .unwrap();
}
async fn public_audio_request(
    app: &axum::Router,
    path: &str,
    range: Option<&str>,
) -> axum::response::Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let mut request = Request::builder().uri(path);
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_collection_aliases_deduplicate_libraries_and_preserve_playlist_occurrences(
    pool: PgPool,
) {
    use plamenu::music_collections::{self, Reorder};
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let library = remote_collection(&state, viewer, "library", "private").await;
    add_remote_entry(&state, viewer, library.id, audio).await;
    let revision = music_collections::get(&state, library.id, Some(viewer))
        .await
        .unwrap()
        .revision;
    add_remote_entry(&state, viewer, library.id, track).await;
    assert_eq!(
        music_collections::get(&state, library.id, Some(viewer))
            .await
            .unwrap()
            .revision,
        revision
    );
    let entries = music_collections::entries(&state, library.id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].remote_id, Some(track));
    let playlist = remote_collection(&state, viewer, "playlist", "private").await;
    add_remote_entry(&state, viewer, playlist.id, audio).await;
    add_remote_entry(&state, viewer, playlist.id, track).await;
    let mut entries = music_collections::entries(&state, playlist.id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.remote_id == Some(track)));
    entries.reverse();
    let revision = music_collections::get(&state, playlist.id, Some(viewer))
        .await
        .unwrap()
        .revision;
    let order = Reorder {
        revision: revision.to_string(),
        entry_ids: entries.iter().map(|entry| entry.id.to_string()).collect(),
    };
    music_collections::reorder(&state, viewer, playlist.id, &order)
        .await
        .unwrap();
    assert!(matches!(
        music_collections::reorder(&state, viewer, playlist.id, &order).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    let playables = music_collections::playables(&state, playlist.id, Some(viewer))
        .await
        .unwrap();
    assert_eq!(
        playables
            .iter()
            .map(|item| item.entry_id.clone())
            .collect::<Vec<_>>(),
        order.entry_ids
    );
    assert_eq!(
        music_collections::statistics(&state, playlist.id, Some(viewer))
            .await
            .unwrap()
            .track_count,
        2
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_collections::playables(&state, playlist.id, Some(viewer))
            .await
            .unwrap()
            .is_empty()
    );
    let revision = music_collections::get(&state, playlist.id, Some(viewer))
        .await
        .unwrap()
        .revision;
    music_collections::remove(&state, viewer, playlist.id, revision, entries[0].id)
        .await
        .unwrap();
    assert_eq!(
        music_collections::entries(&state, playlist.id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_remote_collection_playback_requires_exact_current_parent_and_entry(pool: PgPool) {
    use axum::http::StatusCode;
    use plamenu::music_collections;
    let (state, viewer, _, stub, track) = playback_fixture(pool).await;
    let collection = remote_collection(&state, viewer, "playlist", "public").await;
    add_remote_entry(&state, viewer, collection.id, track).await;
    let item = music_collections::playables(&state, collection.id, None)
        .await
        .unwrap()
        .remove(0);
    let app = plamenu::build_router(state.clone());
    assert!(
        item.stream_url
            .contains(&format!("/public/{}/", collection.id))
    );
    let response = public_audio_request(&app, &item.stream_url, Some("bytes=2-7")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["Content-Range"], "bytes 2-7/16");
    assert_eq!(response_text(response).await, "234567");
    let wrong = item.stream_url.replace(
        &format!("/{track}/public/"),
        &format!("/{}/public/", track + 1),
    );
    assert_eq!(
        public_audio_request(&app, &wrong, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let feed = public_audio_request(
        &app,
        &format!("/music/feeds/collections/{}.rss", collection.id),
        None,
    )
    .await;
    assert_eq!(feed.status(), StatusCode::OK);
    let xml = response_text(feed).await;
    assert!(xml.contains("Origin track"));
    assert!(xml.contains(&item.stream_url));
    let before = stub.media_fetches.lock().unwrap().len();
    let current = music_collections::get(&state, collection.id, Some(viewer))
        .await
        .unwrap();
    let private = music_collections::update(
        &state,
        viewer,
        collection.id,
        current.revision,
        &music_collections::New {
            name: current.name,
            description: current.description,
            kind: current.kind,
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        public_audio_request(&app, &item.stream_url, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(stub.media_fetches.lock().unwrap().len(), before);
    let current = music_collections::update(
        &state,
        viewer,
        collection.id,
        private.revision,
        &music_collections::New {
            name: private.name,
            description: private.description,
            kind: private.kind,
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    music_collections::remove(
        &state,
        viewer,
        collection.id,
        current.revision,
        item.entry_id.parse().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        public_audio_request(&app, &item.stream_url, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(stub.media_fetches.lock().unwrap().len(), before);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stalled_public_remote_body_stops_when_collection_membership_is_removed(pool: PgPool) {
    use http_body_util::BodyExt;
    use plamenu::music_collections;
    use std::sync::atomic::Ordering;
    let (state, viewer, _, stub, track) = playback_fixture(pool).await;
    let collection = remote_collection(&state, viewer, "playlist", "public").await;
    add_remote_entry(&state, viewer, collection.id, track).await;
    let item = music_collections::playables(&state, collection.id, None)
        .await
        .unwrap()
        .remove(0);
    *stub.enclosure_chunking.lock().unwrap() = Some((4, 3000));
    let response = public_audio_request(
        &plamenu::build_router(state.clone()),
        &item.stream_url,
        None,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let mut body = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "0123");
    let current = music_collections::get(&state, collection.id, Some(viewer))
        .await
        .unwrap();
    music_collections::remove(
        &state,
        viewer,
        collection.id,
        current.revision,
        item.entry_id.parse().unwrap(),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1500), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop(body);
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 0);
    assert_eq!(stub.enclosure_chunks_sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        public_audio_request(&plamenu::build_router(state), &item.stream_url, None)
            .await
            .status(),
        axum::http::StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_collection_remote_url_form_requires_csrf_and_keeps_origin_ownership(pool: PgPool) {
    use axum::http::StatusCode;
    use plamenu::music_collections;
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let (cookie, csrf) = login(&state, viewer).await;
    let collection = remote_collection(&state, viewer, "library", "private").await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/web/music/collections/{}/add", collection.id);
    let url = format!("/music/remote/{audio}");
    let revision = collection.revision.to_string();
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[("url", &url), ("revision", &revision), ("csrf", "wrong")]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        music_collections::entries(&state, collection.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[("url", &url), ("revision", &revision), ("csrf", &csrf)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        music_collections::entries(&state, collection.id)
            .await
            .unwrap()[0]
            .remote_id,
        Some(track)
    );
    let html = response_text(
        web_request(
            &app,
            &cookie,
            "GET",
            &format!("/music/collections/{}", collection.id),
            &[],
        )
        .await,
    )
    .await;
    assert!(html.contains("Origin track"));
    assert!(html.contains(&format!("/music/remote/{track}")));
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM music_assets),(SELECT count(*) FROM statuses),(SELECT count(*) FROM music_saved_resources)").fetch_one(&state.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_bulk_append_is_atomic_revisioned_and_library_aliases_are_idempotent(pool: PgPool) {
    use plamenu::music_collections;
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let library = remote_collection(&state, viewer, "library", "private").await;
    music_collections::add_remote_sources(
        &state,
        viewer,
        library.id,
        library.revision,
        &[audio, track],
    )
    .await
    .unwrap();
    let current = music_collections::get(&state, library.id, Some(viewer))
        .await
        .unwrap();
    assert_eq!(current.revision, library.revision + 1);
    assert_eq!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()
            .len(),
        1
    );
    music_collections::add_remote_sources(&state, viewer, library.id, current.revision, &[track])
        .await
        .unwrap();
    assert_eq!(
        music_collections::get(&state, library.id, Some(viewer))
            .await
            .unwrap()
            .revision,
        current.revision
    );
    assert!(matches!(
        music_collections::add_remote_sources(
            &state,
            viewer,
            library.id,
            library.revision,
            &[track]
        )
        .await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    let playlist = remote_collection(&state, viewer, "playlist", "public").await;
    assert!(
        music_collections::add_remote_sources(
            &state,
            viewer,
            playlist.id,
            playlist.revision,
            &[track, i64::MAX]
        )
        .await
        .is_err()
    );
    assert!(
        music_collections::entries(&state, playlist.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        music_collections::get(&state, playlist.id, Some(viewer))
            .await
            .unwrap()
            .revision,
        playlist.revision
    );
    music_collections::add_remote_sources(
        &state,
        viewer,
        playlist.id,
        playlist.revision,
        &[audio, track],
    )
    .await
    .unwrap();
    let entries = music_collections::entries(&state, playlist.id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.remote_id == Some(track)));
    assert_eq!(
        music_collections::get(&state, playlist.id, Some(viewer))
            .await
            .unwrap()
            .revision,
        playlist.revision + 1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_remote_bulk_controls_reject_forged_members_and_stale_collection_revisions(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    use plamenu::music_collections;
    let (state, viewer, _, _, track) = playable_remote_fixture(pool).await;
    let (cookie, csrf) = login(&state, viewer).await;
    let root: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(ROOT)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let library = remote_collection(&state, viewer, "library", "private").await;
    let app = plamenu::build_router(state.clone());
    let html = response_text(
        web_request(&app, &cookie, "GET", &format!("/music/remote/{root}"), &[]).await,
    )
    .await;
    assert!(html.contains("id=\"remote-collect\""));
    assert!(html.contains("name=\"remote\""));
    assert!(html.contains(&format!(
        "type=\"checkbox\" name=\"remote\" value=\"{track}\""
    )));
    let path = format!("/web/music/remote/{root}/collect");
    let collection = format!("{}:{}", library.id, library.revision);
    let selected = track.to_string();
    let forged = i64::MAX.to_string();
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[
                ("csrf", "wrong"),
                ("collection", &collection),
                ("remote", &selected)
            ]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[
                ("csrf", &csrf),
                ("collection", &collection),
                ("remote", &selected),
                ("remote", &forged)
            ]
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[
                ("csrf", &csrf),
                ("collection", &collection),
                ("remote", &selected)
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        web_request(
            &app,
            &cookie,
            "POST",
            &path,
            &[
                ("csrf", &csrf),
                ("collection", &collection),
                ("remote", &selected)
            ]
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let queue = plamenu::music_queues::get(&state, viewer, "remote-bulk-device")
        .await
        .unwrap();
    let queue = plamenu::music_queues::change(
        &state,
        viewer,
        "remote-bulk-device",
        &plamenu::music_queues::Change {
            revision: queue.revision,
            action: plamenu::music_queues::Action::Collection {
                collection_id: library.id.to_string(),
                replace: false,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 1);
    assert_eq!(queue.entries[0].title, "Origin track");
    assert!(
        queue.entries[0]
            .stream_url
            .contains(&format!("/music/remote/{track}/stream"))
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn shared_remote_collection_playback_cannot_escape_membership_and_revokes_stalled_bodies(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use plamenu::{music_collections, music_shares};
    use std::sync::atomic::Ordering;
    let (state, viewer, _, stub, track) = playback_fixture(pool).await;
    let collection = remote_collection(&state, viewer, "playlist", "private").await;
    add_remote_entry(&state, viewer, collection.id, track).await;
    let link = music_shares::create(
        &state,
        viewer,
        &music_shares::New {
            reference: Reference {
                kind: Kind::Playlist,
                id: collection.id.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap();
    assert!(
        music_shares::create(
            &state,
            viewer,
            &music_shares::New {
                reference: Reference {
                    kind: Kind::Remote,
                    id: track.to_string()
                },
                expires_at: None,
            }
        )
        .await
        .is_err()
    );
    let entry = music_collections::entries(&state, collection.id)
        .await
        .unwrap()[0]
        .id;
    let path = format!(
        "/music/share/remote/{track}/{entry}/stream?token={}",
        link.token
    );
    let app = plamenu::build_router(state.clone());
    let response = public_audio_request(&app, &path, Some("bytes=4-7")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response_text(response).await, "4567");
    let invalid = format!(
        "/music/share/remote/{track}/{}/stream?token={}",
        entry + 1,
        link.token
    );
    assert_eq!(
        public_audio_request(&app, &invalid, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let preview = format!("/music/share?token={}", link.token);
    let html = response_text(public_audio_request(&app, &preview, None).await).await;
    assert!(html.contains("Origin track"));
    assert!(html.contains(&format!(
        "/music/share/remote/{track}/{entry}/stream?token="
    )));
    let feed = public_audio_request(
        &app,
        &format!(
            "/music/feeds/collections/{}.rss?token={}",
            collection.id, link.token
        ),
        None,
    )
    .await;
    assert_eq!(feed.status(), StatusCode::OK);
    let xml = response_text(feed).await;
    assert!(xml.contains("Origin track"));
    assert!(xml.contains(&format!(
        "/music/share/remote/{track}/{entry}/stream?token="
    )));
    *stub.enclosure_chunking.lock().unwrap() = Some((4, 3000));
    let mut body = public_audio_request(&app, &path, None).await.into_body();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    music_shares::revoke(&state, viewer, link.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1500), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop(body);
    assert_eq!(stub.enclosure_active.load(Ordering::SeqCst), 0);
    assert_eq!(
        public_audio_request(&app, &path, None).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_remote_collection_embeds_preserve_repeats_and_cross_origin_ranges_then_revoke(
    pool: PgPool,
) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use plamenu::music_collections;
    use tower::ServiceExt;
    let (state, viewer, _, _, track) = playback_fixture(pool).await;
    let collection = remote_collection(&state, viewer, "playlist", "public").await;
    let current = music_collections::get(&state, collection.id, Some(viewer))
        .await
        .unwrap();
    music_collections::add_remote_sources(
        &state,
        viewer,
        collection.id,
        current.revision,
        &[track, track],
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/embed/collections/{}", collection.id);
    let frame = public_audio_request(&app, &path, None).await;
    assert_eq!(frame.status(), StatusCode::OK);
    let html = response_text(frame).await;
    assert_eq!(html.matches("<audio ").count(), 2);
    let sources = music_collections::playables(&state, collection.id, None)
        .await
        .unwrap();
    for source in &sources {
        assert!(html.contains(&source.stream_url));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&sources[0].stream_url)
                .header("Origin", "https://widget.test")
                .header("Range", "bytes=1-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["Access-Control-Allow-Origin"], "*");
    assert!(
        !response
            .headers()
            .contains_key("Access-Control-Allow-Credentials")
    );
    assert_eq!(response_text(response).await, "123");
    let preflight = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri(&sources[0].stream_url)
                .header("Origin", "https://widget.test")
                .header("Access-Control-Request-Method", "GET")
                .header("Access-Control-Request-Headers", "Range")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    let current = music_collections::get(&state, collection.id, Some(viewer))
        .await
        .unwrap();
    music_collections::update(
        &state,
        viewer,
        collection.id,
        current.revision,
        &music_collections::New {
            name: current.name,
            description: current.description,
            kind: current.kind,
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        public_audio_request(&app, &path, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let denied = public_audio_request(&app, &sources[0].stream_url, None).await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(denied.headers()["Access-Control-Allow-Origin"], "*");
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_collection_feeds_use_current_rendition_mime_and_caption_without_fetching_audio(
    pool: PgPool,
) {
    use axum::http::StatusCode;
    let (state, viewer, _, stub, mut root) = fixture(pool).await;
    root["items"][0]["attachment"][0]["mediaType"] = json!("audio/ogg; codecs=vorbis");
    root["items"][0]["track"]["summary"] = json!("<p>Current origin caption</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(ROOT.into(), root.clone());
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let collection = remote_collection(&state, viewer, "playlist", "public").await;
    add_remote_entry(&state, viewer, collection.id, track).await;
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/feeds/collections/{}.rss", collection.id);
    let feed = public_audio_request(&app, &path, None).await;
    assert_eq!(feed.status(), StatusCode::OK);
    let old_etag = feed.headers()["ETag"].clone();
    let xml = response_text(feed).await;
    assert!(xml.contains("type=\"audio/ogg\""));
    assert!(xml.contains("Current origin caption"));
    assert!(!xml.contains("type=\"audio/mpeg\""));
    assert!(!xml.contains("songhive.test/files/"));
    assert!(stub.media_fetches.lock().unwrap().is_empty());
    root["items"][0]["track"]["summary"] = json!("<p>Updated origin caption</p>");
    root["items"][0]["attachment"][0]["mediaType"] = json!("audio/flac");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let feed = public_audio_request(&app, &path, None).await;
    assert_ne!(feed.headers()["ETag"], old_etag);
    let xml = response_text(feed).await;
    assert!(xml.contains("type=\"audio/flac\""));
    assert!(xml.contains("Updated origin caption"));
    assert!(!xml.contains("Current origin caption"));
    assert!(stub.media_fetches.lock().unwrap().is_empty());
    let atom = public_audio_request(
        &app,
        &format!("/music/feeds/collections/{}.atom", collection.id),
        None,
    )
    .await;
    assert_eq!(atom.status(), StatusCode::OK);
    assert!(response_text(atom).await.contains("type=\"audio/flac\""));
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_federated_library_lists_imported_publication_uri_without_republishing_it(
    pool: PgPool,
) {
    use plamenu::music_library_federation;
    let (state, viewer, _, audio, track) = playable_remote_fixture(pool).await;
    let library = remote_collection(&state, viewer, "library", "public").await;
    add_remote_entry(&state, viewer, library.id, audio).await;
    add_remote_entry(&state, viewer, library.id, track).await;
    let index = music_library_federation::document(&state, library.id, None)
        .await
        .unwrap();
    assert_eq!(index["totalItems"], 1);
    let page = music_library_federation::document(&state, library.id, Some(1))
        .await
        .unwrap();
    assert_eq!(page["items"], json!([AUDIO]));
    assert!(page["actor"].as_str().unwrap().contains("alice"));
    let copied: (i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM statuses),(SELECT count(*) FROM music_assets),(SELECT count(*) FROM music_publications)").fetch_one(&state.pool).await.unwrap();
    assert_eq!(copied, (0, 0, 0));
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        music_library_federation::document(&state, library.id, None)
            .await
            .unwrap()["totalItems"],
        0
    );
    assert_eq!(
        music_library_federation::document(&state, library.id, Some(1))
            .await
            .unwrap()["items"],
        json!([])
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn saved_legacy_remote_audio_folds_only_verified_publication_controller_and_uri(
    pool: PgPool,
) {
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let object = root["items"][0].clone();
    let status = plamenu::ingest::ingest_remote_note(&state, &owner, &object)
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let reference = Reference {
        kind: Kind::Audio,
        id: status.id.to_string(),
    };
    let saved = music_saved::save(&state, viewer, &reference).await.unwrap();
    assert_eq!(saved.reference.kind, Kind::Remote);
    assert_eq!(saved.reference.id, track.to_string());
    let again = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(again.id, saved.id);
    assert!(
        music_saved::contains(&state, viewer, &reference)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_saved_resources")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_saved_remote")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        1
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_saved::save(&state, viewer, &reference).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    music_saved::remove_reference(&state, viewer, &reference)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_saved_remote")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    let local = common::create_local_account(&state.pool, "bob", "Bob").await;
    sqlx::query("UPDATE statuses SET account_id=$2 WHERE id=$1")
        .bind(status.id)
        .bind(local.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let foreign = music_saved::save(&state, viewer, &reference).await.unwrap();
    assert_eq!(foreign.reference.kind, Kind::Audio);
    assert_ne!(foreign.id, saved.id);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn legacy_remote_source_creations_share_canonical_collection_queue_favourite_and_listen_identity(
    pool: PgPool,
) {
    use plamenu::{music_collections, music_favourites, music_listening, music_queues};
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let status = plamenu::ingest::ingest_remote_note(&state, &owner, &root["items"][0])
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let source = music_queues::Source {
        status_id: Some(status.id.to_string()),
        remote_id: None,
        asset_id: None,
        episode_id: None,
    };
    let library = remote_collection(&state, viewer, "library", "private").await;
    music_collections::add(
        &state,
        viewer,
        library.id,
        &music_collections::Add {
            status_id: source.status_id.clone(),
            remote_id: None,
            asset_id: None,
            episode_id: None,
            revision: library.revision.to_string(),
        },
    )
    .await
    .unwrap();
    add_remote_entry(&state, viewer, library.id, track).await;
    assert_eq!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let queue = music_queues::get(&state, viewer, "legacy-device")
        .await
        .unwrap();
    let queue = music_queues::change(
        &state,
        viewer,
        "legacy-device",
        &music_queues::Change {
            revision: queue.revision,
            action: music_queues::Action::Add {
                sources: vec![source.clone()],
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 1);
    let favourite = music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    assert_eq!(favourite.remote_id, Some(track.to_string()));
    assert_eq!(
        music_favourites::save(&state, viewer, &remote_source(track))
            .await
            .unwrap()
            .id,
        favourite.id
    );
    assert!(
        music_favourites::contains(&state, viewer, &source)
            .await
            .unwrap()
    );
    let listen = music_listening::Start {
        status_id: source.status_id.clone(),
        remote_id: None,
        asset_id: None,
        episode_id: None,
        queue_device: None,
        queue_entry_id: None,
        request_key: "legacy-listen".into(),
    };
    music_listening::start(&state, viewer, &listen)
        .await
        .unwrap();
    let stored:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT remote_id FROM music_collection_entries WHERE collection_id=$1),(SELECT remote_id FROM music_queue_entries LIMIT 1),(SELECT remote_id FROM music_listen_sessions LIMIT 1)").bind(library.id).fetch_one(&state.pool).await.unwrap();
    assert_eq!(stored, (track, track, track));
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_favourites::save(&state, viewer, &source).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(matches!(
        music_listening::start(
            &state,
            viewer,
            &music_listening::Start {
                request_key: "withdrawn-listen".into(),
                ..listen
            }
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    assert!(
        !music_favourites::contains(&state, viewer, &source)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cached_public_track_never_widens_private_legacy_audio_post_audience(pool: PgPool) {
    use plamenu::{music_collections, music_favourites, music_queues};
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let status = plamenu::ingest::ingest_remote_note(&state, &owner, &root["items"][0])
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET visibility='direct' WHERE id=$1")
        .bind(status.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let library = remote_collection(&state, viewer, "library", "private").await;
    let source = music_queues::Source {
        status_id: Some(status.id.to_string()),
        remote_id: None,
        asset_id: None,
        episode_id: None,
    };
    assert!(matches!(
        music_collections::add(
            &state,
            viewer,
            library.id,
            &music_collections::Add {
                status_id: source.status_id.clone(),
                remote_id: None,
                asset_id: None,
                episode_id: None,
                revision: library.revision.to_string()
            }
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(matches!(
        music_favourites::save(&state, viewer, &source).await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    let queue = music_queues::get(&state, viewer, "private-alias")
        .await
        .unwrap();
    assert!(matches!(
        music_queues::change(
            &state,
            viewer,
            "private-alias",
            &music_queues::Change {
                revision: queue.revision,
                action: music_queues::Action::Add {
                    sources: vec![source]
                }
            }
        )
        .await,
        Err(plamenu::error::ApiError::NotFound)
    ));
    assert!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    add_remote_entry(&state, viewer, library.id, track).await;
    assert_eq!(
        music_collections::entries(&state, library.id)
            .await
            .unwrap()[0]
            .remote_id,
        Some(track)
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn historical_saved_audio_uses_current_imported_metadata_and_hides_withdrawn_origin(
    pool: PgPool,
) {
    let (state, viewer, publisher, stub, mut root) = fixture(pool).await;
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let status = plamenu::ingest::ingest_remote_note(&state, &owner, &root["items"][0])
        .await
        .unwrap();
    let saved = music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Audio,
            id: status.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(saved.reference.kind, Kind::Audio);
    root["items"][0]["track"]["name"] = json!("Current canonical title");
    stub.objects.lock().unwrap().insert(ROOT.into(), root);
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let page = music_saved::page(&state, viewer, None).await.unwrap();
    assert_eq!(page.items[0].id, saved.id);
    assert_eq!(
        page.items[0].source.as_ref().unwrap()["title"],
        "Current canonical title"
    );
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=clock_timestamp() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_saved::page(&state, viewer, None).await.unwrap().items[0]
            .source
            .is_none()
    );
    assert!(
        music_mine::page(&state, viewer, &music_mine::Request::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    music_saved::remove(&state, viewer, saved.id.parse().unwrap())
        .await
        .unwrap();
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn historical_audio_memberships_fold_current_tracks_without_widening_post_access(
    pool: PgPool,
) {
    use plamenu::{music_favourites, music_queues::Source};
    use plamenu_db::id;
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let status = plamenu::ingest::ingest_remote_note(&state, &owner, &root["items"][0])
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let audio: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(AUDIO)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let historical = id::next();
    sqlx::query("INSERT INTO music_favourites(id,account_id,status_id) VALUES($1,$2,$3)")
        .bind(historical)
        .bind(viewer)
        .bind(status.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO music_saved_resources(id,account_id,kind,status_id) VALUES($1,$2,'audio',$3)",
    )
    .bind(id::next())
    .bind(viewer)
    .bind(status.id)
    .execute(&state.pool)
    .await
    .unwrap();
    // Pre-canonicalization remote saves may coexist with historical post saves.
    sqlx::query("INSERT INTO music_saved_remote(id,account_id,resource_id) VALUES($1,$2,$3)")
        .bind(id::next())
        .bind(viewer)
        .bind(audio)
        .execute(&state.pool)
        .await
        .unwrap();
    music_saved::save(
        &state,
        viewer,
        &Reference {
            kind: Kind::Remote,
            id: track.to_string(),
        },
    )
    .await
    .unwrap();
    let source = Source {
        asset_id: None,
        status_id: None,
        episode_id: None,
        remote_id: Some(track.to_string()),
    };
    assert!(
        music_favourites::contains(&state, viewer, &source)
            .await
            .unwrap()
    );
    let saved = music_favourites::save(&state, viewer, &source)
        .await
        .unwrap();
    assert_eq!(saved.id, historical.to_string());
    assert_eq!(saved.remote_id, Some(track.to_string()));
    assert!(saved.status_id.is_none());
    let favourites = music_favourites::page(&state, viewer, None).await.unwrap();
    assert_eq!(favourites.items.len(), 1);
    assert_eq!(favourites.items[0].track.title, "Origin track");
    assert_eq!(favourites.items[0].remote_id, Some(track.to_string()));
    let request = music_mine::Request {
        kind: music_mine::Filter::Tracks,
        ..Default::default()
    };
    let mine = music_mine::page(&state, viewer, &request).await.unwrap();
    assert_eq!(mine.items.len(), 1);
    assert_eq!(mine.items[0].reference.id, track.to_string());
    assert_eq!(mine.items[0].reference.kind, Kind::Remote);
    assert!(mine.items[0].membership.saved && mine.items[0].membership.favourite);
    // The legacy private post cannot borrow the public cached track's audience.
    sqlx::query("UPDATE statuses SET visibility='direct' WHERE id=$1")
        .bind(status.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let mine = music_mine::page(&state, viewer, &request).await.unwrap();
    assert_eq!(mine.items.len(), 1);
    assert!(mine.items[0].membership.saved);
    assert!(!mine.items[0].membership.favourite);
    sqlx::query("UPDATE statuses SET visibility='public' WHERE id=$1")
        .bind(status.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE music_remote_resources SET unavailable_at=now() WHERE uri=$1")
        .bind(AUDIO)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_favourites::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        music_mine::page(&state, viewer, &request)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // A canonical remove also removes the account's historical alias after withdrawal.
    music_favourites::remove(&state, viewer, &source)
        .await
        .unwrap();
    assert!(
        !music_favourites::contains(&state, viewer, &source)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_favourites WHERE account_id=$1")
            .bind(viewer)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_favourite_representatives_are_unique_across_legacy_audio_and_page_boundaries(
    pool: PgPool,
) {
    use plamenu::music_favourites;
    let (state, viewer, publisher, _, root) = fixture(pool).await;
    music_remote::import(&state, viewer, ROOT).await.unwrap();
    let owner = plamenu::remote::store_remote_actor(&state.pool, &publisher.actor)
        .await
        .unwrap();
    let post = plamenu::ingest::ingest_remote_note(&state, &owner, &root["items"][0])
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(TRACK)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let audio: i64 = sqlx::query_scalar("SELECT id FROM music_remote_resources WHERE uri=$1")
        .bind(AUDIO)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_favourites(id,account_id,status_id) VALUES($1,$2,$3)")
        .bind(plamenu_db::id::next())
        .bind(viewer)
        .bind(post.id)
        .execute(&state.pool)
        .await
        .unwrap();
    for remote in [track, audio] {
        sqlx::query("INSERT INTO music_favourites(id,account_id,remote_id) VALUES($1,$2,$3)")
            .bind(plamenu_db::id::next())
            .bind(viewer)
            .bind(remote)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    let artist = plamenu_db::id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Owner artist')")
        .bind(artist)
        .bind(viewer)
        .execute(&state.pool)
        .await
        .unwrap();
    let assets = (0..51).map(|_| plamenu_db::id::next()).collect::<Vec<_>>();
    let favs = (0..51).map(|_| plamenu_db::id::next()).collect::<Vec<_>>();
    sqlx::query("INSERT INTO music_assets(id,owner_id,artist_id,title,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) SELECT id,$2,$3,'Track','music'||id||'.orig','music'||id||'.mp3',1,1,lpad(to_hex(id),64,'0'),1,'mp3' FROM unnest($1::bigint[]) id").bind(&assets).bind(viewer).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_favourites(id,account_id,asset_id) SELECT fav,$3,asset FROM unnest($1::bigint[],$2::bigint[]) AS rows(asset,fav)").bind(assets).bind(favs).bind(viewer).execute(&state.pool).await.unwrap();
    let first = music_favourites::page(&state, viewer, None).await.unwrap();
    assert_eq!(first.items.len(), 50);
    let second = music_favourites::page(
        &state,
        viewer,
        first.before.as_deref().map(|s| s.parse().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(second.before.is_none());
    assert_eq!(
        second
            .items
            .iter()
            .filter(|i| i.remote_id.as_deref() == Some(track.to_string().as_str()))
            .count(),
        1
    );
    assert!(
        !first
            .items
            .iter()
            .any(|a| second.items.iter().any(|b| a.id == b.id))
    );
}
