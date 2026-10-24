mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{AppState, music_queues, music_remote};
use plamenu_db::PgPool;
use plamenu_federation::FetchedMedia;
use serde_json::{Value, json};
use std::sync::Arc;
const BASE: &str = "https://songhive.test";
const ID: &str = "3a3306a1-af1a-4a14-8d39-028dc2366606";
const ALBUM: &str = "30193aa4-b98f-427c-bc6e-5586a644f95a";
fn put(stub: &StubFederation, uri: &str, value: &Value) {
    stub.media.lock().unwrap().insert(
        uri.into(),
        FetchedMedia {
            final_url: uri.into(),
            content_type: "application/json".into(),
            bytes: serde_json::to_vec(value).unwrap(),
        },
    );
}
async fn fixture(pool: PgPool) -> (AppState, i64, Arc<StubFederation>, Value) {
    let viewer = create_local_account(&pool, "listener", "Listener").await.id;
    let owner = RemoteUser::new("songhive.test", "musician");
    let mut instance = RemoteUser::new("songhive.test", "catalogue").actor;
    instance.id = format!("{BASE}/ap/actor");
    instance.public_key.id = format!("{BASE}/ap/actor#main-key");
    instance.public_key.owner.clone_from(&instance.id);
    instance.kind = "Application".into();
    let stub = StubFederation::with_actors([owner.actor.clone(), instance.clone()]);
    let album = json!({"id":format!("{BASE}/albums/{ALBUM}"),"type":"Album","name":"Public album","attributedTo":instance.id});
    stub.objects
        .lock()
        .unwrap()
        .insert(format!("{BASE}/albums/{ALBUM}"), album);
    put(
        &stub,
        &format!("{BASE}/api/v1/instance"),
        &json!({"songhive_version":"0.4.10","title":"Public music server"}),
    );
    let track = json!({"id":ID,"title":"Unpublished public song","album_id":ALBUM,"artist_id":"d4acca75-41fd-411e-bbe7-53f03fdc8de4","visibility":"public","paid":false,"duration":10,"audio_url":"/files/track.wav","artist":{"name":"Musician"},"album":{"title":"Public album"},"owner":{"actor_url":owner.actor.id}});
    put(
        &stub,
        &format!("{BASE}/api/v1/tracks/{ID}?include=artist,album,owner"),
        &track,
    );
    put(
        &stub,
        &format!(
            "{BASE}/api/v1/tracks/?album_id={ALBUM}&include=artist,album,owner&limit=24&offset=0"
        ),
        &json!([track]),
    );
    put(
        &stub,
        &format!("{BASE}/api/v1/tracks/?limit=24&offset=0&q=&include=artist%2Calbum%2Cowner"),
        &json!([track]),
    );
    (test_state_with(pool, stub.clone()), viewer, stub, track)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_unpublished_album_plays_without_following_or_fabricating_an_ap_post(pool: PgPool) {
    let (state, viewer, _, _) = fixture(pool).await;
    let album = music_remote::import(&state, viewer, &format!("{BASE}/albums/{ALBUM}"))
        .await
        .unwrap();
    let contents = music_remote::contents(&state, album.id, viewer, None)
        .await
        .unwrap();
    assert_eq!(contents.items.len(), 1);
    let track = &contents.items[0];
    assert_eq!(track.uri, format!("{BASE}/api/v1/tracks/{ID}"));
    assert_eq!(track.catalogue_kind.as_deref(), Some("track"));
    assert_eq!(track.kind, "track");
    assert_eq!(track.proof_uri, track.uri);
    let source = music_remote::stream::source(&state, track.id, viewer)
        .await
        .unwrap();
    assert_eq!(source.url, format!("{BASE}/files/track.wav"));
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM music_follows),(SELECT count(*) FROM music_assets),(SELECT count(*) FROM statuses)").fetch_one(&state.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    let queue = music_queues::get(&state, viewer, "web").await.unwrap();
    let result = music_queues::change(
        &state,
        viewer,
        "web",
        &music_queues::Change {
            revision: queue.revision,
            action: music_queues::Action::Play {
                sources: vec![music_queues::Source {
                    remote_id: Some(track.id.to_string()),
                    asset_id: None,
                    status_id: None,
                    episode_id: None,
                }],
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.current_entry_id.as_deref(),
        Some(result.entries[0].entry_id.as_str())
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn anonymous_catalogue_access_is_rechecked_for_private_paid_deleted_and_changed_media(
    pool: PgPool,
) {
    let (state, viewer, stub, track) = fixture(pool).await;
    let uri = format!("{BASE}/api/v1/tracks/{ID}");
    let endpoint = format!("{uri}?include=artist,album,owner");
    let resource = music_remote::import(&state, viewer, &uri).await.unwrap();
    for changed in [
        json!({"visibility":"private"}),
        json!({"paid":true}),
        json!({"audio_url":"https://other.test/file.mp3"}),
        json!({"owner":{"actor_url":format!("{BASE}/users/other")}}),
    ] {
        let mut value = track.clone();
        for (key, item) in changed.as_object().unwrap() {
            value[key] = item.clone();
        }
        put(&stub, &endpoint, &value);
        assert!(
            music_remote::stream::source(&state, resource.id, viewer)
                .await
                .is_err()
        );
    }
    stub.media.lock().unwrap().remove(&endpoint);
    assert!(
        music_remote::stream::source(&state, resource.id, viewer)
            .await
            .is_err()
    );
    put(&stub, &endpoint, &track);
    assert!(
        music_remote::stream::source(&state, resource.id, viewer)
            .await
            .is_ok()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn server_discovery_keeps_private_and_paid_tracks_out_of_results(pool: PgPool) {
    let (state, viewer, stub, track) = fixture(pool).await;
    let uri = format!("{BASE}/api/v1/tracks/?limit=24&offset=0&q=&include=artist%2Calbum%2Cowner");
    let mut private = track.clone();
    private["visibility"] = json!("private");
    let mut paid = track.clone();
    paid["paid"] = json!(true);
    put(&stub, &uri, &json!([track, private, paid]));
    let page = music_remote::catalogue::browse(&state, viewer, BASE, "tracks", "", 0)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].playable);
    assert!(
        page.items[0]
            .url
            .starts_with(&format!("{BASE}/api/v1/tracks/"))
    );
    let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM music_remote_resources")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(cached, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
#[allow(clippy::too_many_lines)] // One signed-in journey verifies discovery, playback and policy gates.
async fn native_music_landing_album_and_play_flow_needs_no_subscription(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let (state, viewer, _, _) = fixture(pool).await;
    plamenu_db::user::create(
        &state.pool,
        viewer,
        Some("listener@plamenu.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=listener%40plamenu.test&password=fixture-password",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let get = |uri: String| {
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap()
    };
    let landing = app.clone().oneshot(get("/music".into())).await.unwrap();
    let html = String::from_utf8(
        landing
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("music-search"));
    assert!(html.contains("Search music or paste a link"));
    assert!(html.contains("Live radio"));
    assert!(!html.contains("Nothing here yet"));
    let path = format!(
        "/music/open?{}",
        serde_urlencoded::to_string([("url", format!("{BASE}/albums/{ALBUM}"))]).unwrap()
    );
    let response = app.clone().oneshot(get(path)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let detail = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let response = app.clone().oneshot(get(detail.clone())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Unpublished public song"));
    assert!(html.contains("<audio controls"));
    assert!(html.contains("Play these tracks"));
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let id = detail.rsplit('/').next().unwrap();
    let play_path = format!("/web/music/remote/{id}/play");
    let request = Request::builder()
        .method("POST")
        .uri(&play_path)
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("csrf=invalid"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let body = serde_urlencoded::to_string([("csrf", csrf.clone())]).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(play_path)
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/music/player?autoplay=true"
    );
    let queue = music_queues::get(&state, viewer, "web").await.unwrap();
    assert!(queue.current_entry_id.is_some());
    assert_eq!(queue.entries.len(), 1);
    let track_id = queue.entries[0].url.rsplit('/').next().unwrap().to_owned();
    let body =
        serde_urlencoded::to_string([("csrf", csrf.as_str()), ("remote_id", track_id.as_str())])
            .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/web/music/play")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/music/player?autoplay=true"
    );
    let queue = music_queues::get(&state, viewer, "web").await.unwrap();
    assert_eq!(
        queue.entries.len(),
        1,
        "Play replaces the queue and selects the requested song in one action"
    );
    assert_eq!(
        queue.current_entry_id.as_deref(),
        Some(queue.entries[0].entry_id.as_str())
    );
    let revision = queue.revision.clone();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/web/music/play")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf=invalid&remote_id={track_id}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        music_queues::get(&state, viewer, "web")
            .await
            .unwrap()
            .revision,
        revision
    );
    let body = serde_urlencoded::to_string([
        ("csrf", csrf.clone()),
        ("url", format!("{BASE}/api/v1/tracks/{ID}")),
    ])
    .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/web/music/play")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "Catalogue cards play without a separate import screen"
    );
    let response = app
        .clone()
        .oneshot(get("/music?q=Musician".into()))
        .await
        .unwrap();
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Unpublished public song"));
    assert!(html.contains("/web/music/play"));
    let listed = music_remote::catalogue(&state, viewer, "Musician", None)
        .await
        .unwrap();
    assert!(listed.iter().any(|r| r.id.to_string() == track_id));
    let owner = listed
        .iter()
        .find(|r| r.id.to_string() == track_id)
        .unwrap()
        .controller_id;
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert!(
        !music_remote::catalogue(&state, viewer, "Musician", None)
            .await
            .unwrap()
            .iter()
            .any(|r| r.id.to_string() == track_id)
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_live_streams_have_native_playback_and_stop_when_origin_goes_offline(pool: PgPool) {
    let (state, viewer, stub, _) = fixture(pool).await;
    let mut value = json!({"id":ID,"name":"Live radio","visibility":"public","enabled":true,"online":true,"stream_url":"/streams/live.mp3","owner":{"username":"musician","display_name":"Musician"}});
    let endpoint = format!("{BASE}/api/v1/streams/");
    put(&stub, &endpoint, &json!([value]));
    let resource = music_remote::import(&state, viewer, &format!("{BASE}/streams/live.mp3"))
        .await
        .unwrap();
    assert_eq!(resource.catalogue_kind.as_deref(), Some("stream"));
    assert!(
        music_remote::stream::source(&state, resource.id, viewer)
            .await
            .is_ok()
    );
    value["online"] = json!(false);
    put(&stub, &endpoint, &json!([value]));
    assert!(
        music_remote::stream::source(&state, resource.id, viewer)
            .await
            .is_err()
    );
    value["online"] = json!(true);
    value["visibility"] = json!("private");
    put(&stub, &endpoint, &json!([value]));
    assert!(
        music_remote::stream::source(&state, resource.id, viewer)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn artist_browsing_preserves_album_tracks_and_an_empty_origin_page_retires_membership(
    pool: PgPool,
) {
    let (state, viewer, stub, track) = fixture(pool).await;
    let artist_id = track["artist_id"].as_str().unwrap();
    let artist_uri = format!("{BASE}/artists/{artist_id}");
    stub.objects.lock().unwrap().insert(artist_uri.clone(),json!({"id":artist_uri,"type":"Artist","name":"Musician","attributedTo":format!("{BASE}/ap/actor")}));
    put(
        &stub,
        &format!(
            "{BASE}/api/v1/tracks/?artist_id={artist_id}&include=artist,album,owner&limit=24&offset=0"
        ),
        &json!([track]),
    );
    let album = music_remote::import(&state, viewer, &format!("{BASE}/albums/{ALBUM}"))
        .await
        .unwrap();
    let artist = music_remote::import(&state, viewer, &artist_uri)
        .await
        .unwrap();
    assert_eq!(
        music_remote::contents(&state, album.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        music_remote::contents(&state, artist.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    put(
        &stub,
        &format!(
            "{BASE}/api/v1/tracks/?album_id={ALBUM}&include=artist,album,owner&limit=24&offset=0"
        ),
        &json!([]),
    );
    music_remote::catalogue::children(&state, viewer, &album, 0)
        .await
        .unwrap();
    assert!(
        music_remote::contents(&state, album.id, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        music_remote::contents(&state, artist.id, viewer, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}
