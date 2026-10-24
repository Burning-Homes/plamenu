mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{AppState, music_podcasts};
use plamenu_db::{PgPool, account::Account};
use plamenu_federation::FetchedFeed;
use std::sync::Arc;
use tower::ServiceExt;
const URL: &str = "https://podcast.example/show.xml";
fn feed(title: &str, episode: &str) -> Vec<u8> {
    format!(r#"<rss xmlns:i="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>{title}</title><description><![CDATA[<p>Show notes</p><script>bad()</script>]]></description><i:author>Host</i:author><item><guid>stable-one</guid><title>{episode}</title><description><![CDATA[<p>Episode notes</p>]]></description><enclosure url="audio.mp3" type="audio/mpeg" length="32"/><i:duration>03:10</i:duration><pubDate>Fri, 09 Oct 2026 12:00:00 GMT</pubDate></item></channel></rss>"#).into_bytes()
}
fn serve(stub: &StubFederation, url: &str, body: Option<Vec<u8>>, etag: &str) {
    stub.podcast_feeds.lock().unwrap().insert(
        url.into(),
        FetchedFeed {
            final_url: url.into(),
            bytes: body,
            etag: Some(etag.into()),
            last_modified: Some("Fri, 09 Oct 2026 12:00:00 GMT".into()),
        },
    );
}
async fn fixture(pool: PgPool) -> (AppState, Arc<StubFederation>, Account, Account) {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let viewer = create_local_account(&pool, "bob", "Bob").await;
    let stub = StubFederation::with_actors([]);
    serve(&stub, URL, Some(feed("Show", "First episode")), "\"v1\"");
    let state = test_state_with(pool, stub.clone());
    (state, stub, owner, viewer)
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn podcast_website_resolves_the_feed_and_preserves_its_existing_identity(pool: PgPool) {
    let (state, stub, owner, viewer) = fixture(pool).await;
    let (original, _) = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap();
    let site = "https://podcast.example/";
    serve(&stub,site,Some(br#"<!doctype html><html><head><script>const fake='<link rel="alternate" type="application/rss+xml" href="/fake.xml">';</script><link rel="alternate" type="application/rss+xml" href="show.xml"></head></html>"#.to_vec()),"site");
    serve(
        &stub,
        URL,
        Some(feed("Updated show", "Updated episode")),
        "v2",
    );
    let before = stub.podcast_fetches.lock().unwrap().len();
    let (resolved, created) = music_podcasts::subscribe(&state, viewer.id, site)
        .await
        .unwrap();
    assert!(created);
    assert_eq!(resolved.id, original.id);
    assert_eq!(resolved.feed_url, URL);
    assert_eq!(resolved.title, "Updated show");
    assert_eq!(stub.podcast_fetches.lock().unwrap().len() - before, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_podcasts")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        1
    );
    let episodes = music_podcasts::episodes(&state, resolved.id, viewer.id, false, None)
        .await
        .unwrap();
    assert_eq!(episodes.items[0]["title"], "Updated episode");
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn podcast_website_does_not_crawl_recursively_or_follow_script_links(pool: PgPool) {
    let (state, stub, owner, _) = fixture(pool).await;
    let site = "https://podcast.example/";
    serve(&stub,site,Some(br#"<html><head><script>const fake='<link rel="alternate" type="application/rss+xml" href="/fake.xml">';</script></head></html>"#.to_vec()),"site");
    assert!(
        music_podcasts::subscribe(&state, owner.id, site)
            .await
            .is_err()
    );
    assert_eq!(stub.podcast_fetches.lock().unwrap().len(), 1);
    serve(&stub,site,Some(br#"<html><head><link rel="alternate" type="application/rss+xml" href="show.xml"></head></html>"#.to_vec()),"site");
    serve(&stub,URL,Some(br#"<html><head><link rel="alternate" type="application/rss+xml" href="third.xml"></head></html>"#.to_vec()),"second");
    assert!(
        music_podcasts::subscribe(&state, owner.id, site)
            .await
            .is_err()
    );
    assert_eq!(stub.podcast_fetches.lock().unwrap().len(), 3);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_podcast_subscriptions")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn subscription_refresh_retains_episode_identity_play_state_and_last_good_catalogue(
    pool: PgPool,
) {
    let (state, stub, owner, viewer) = fixture(pool).await;
    let (show, created) = music_podcasts::subscribe(&state, owner.id, &format!("{URL}#ignored"))
        .await
        .unwrap();
    assert!(created);
    assert_eq!(show.title, "Show");
    assert!(!show.description.contains("script"));
    assert!(
        !music_podcasts::subscribe(&state, owner.id, URL)
            .await
            .unwrap()
            .1
    );
    assert_eq!(stub.podcast_fetches.lock().unwrap().len(), 1);
    assert!(
        music_podcasts::list(&state, viewer.id, &music_podcasts::Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let first = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap();
    let episode = first.items[0]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(first.items[0]["duration_seconds"], 190);
    music_podcasts::played(&state, owner.id, episode, true)
        .await
        .unwrap();
    music_podcasts::played(&state, owner.id, episode, true)
        .await
        .unwrap();
    assert!(
        music_podcasts::episode(&state, episode, owner.id)
            .await
            .unwrap()
            .played
    );
    assert!(
        !music_podcasts::episode(&state, episode, viewer.id)
            .await
            .unwrap()
            .played
    );
    serve(&stub, URL, None, "\"v1\"");
    let unchanged = music_podcasts::refresh(&state, show.id, owner.id)
        .await
        .unwrap();
    assert_eq!(unchanged.title, "Show");
    assert_eq!(
        stub.podcast_fetches.lock().unwrap()[1].etag.as_deref(),
        Some("\"v1\"")
    );
    serve(
        &stub,
        URL,
        Some(feed("Renamed show", "Updated episode")),
        "\"v2\"",
    );
    music_podcasts::refresh(&state, show.id, owner.id)
        .await
        .unwrap();
    let updated = music_podcasts::episode(&state, episode, owner.id)
        .await
        .unwrap();
    assert_eq!(updated.title, "Updated episode");
    assert!(updated.played);
    serve(&stub, URL, Some(b"<broken>".to_vec()), "\"broken\"");
    assert!(
        music_podcasts::refresh(&state, show.id, owner.id)
            .await
            .is_err()
    );
    let last = music_podcasts::get(&state, show.id, owner.id)
        .await
        .unwrap();
    assert_eq!(last.title, "Renamed show");
    assert_eq!(last.etag.as_deref(), Some("\"v2\""));
    assert_eq!(
        last.last_error.as_deref(),
        Some("Could not parse podcast feed")
    );
    assert_eq!(
        music_podcasts::episodes(&state, show.id, owner.id, false, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let list = music_podcasts::list(
        &state,
        owner.id,
        &music_podcasts::Query {
            q: Some("renamed".into()),
            sort: Some("unplayed".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0]["unplayed_count"], 0);
    music_podcasts::played(&state, owner.id, episode, false)
        .await
        .unwrap();
    assert!(
        !music_podcasts::episode(&state, episode, owner.id)
            .await
            .unwrap()
            .played
    );
    let published: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(published, 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn opml_round_trips_escaped_titles_and_collects_all_feed_failures(pool: PgPool) {
    let (state, stub, owner, _) = fixture(pool).await;
    serve(
        &stub,
        URL,
        Some(feed("Show &amp; friends", "First")),
        "\"v1\"",
    );
    let input=br#"<opml><body><outline text="Group"><outline xmlUrl="https://podcast.example/show.xml"/><outline xmlUrl="https://missing.example/feed"/><outline xmlUrl="https://also-missing.example/feed"/></outline></body></opml>"#;
    let result = music_podcasts::import_opml(&state, owner.id, input)
        .await
        .unwrap();
    assert_eq!(result["subscribed"], 1);
    assert_eq!(result["failed"], 2);
    assert_eq!(result["errors"].as_array().unwrap().len(), 2);
    let xml = music_podcasts::opml(&state, owner.id).await.unwrap();
    assert!(xml.contains("Show &amp; friends"));
    assert_eq!(
        music_podcasts::feed::parse_opml(xml.as_bytes()).unwrap(),
        vec![URL]
    );
    assert_eq!(
        music_podcasts::import_opml(&state, owner.id, xml.as_bytes())
            .await
            .unwrap()["skipped"],
        1
    );
    let shows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_podcasts")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(shows, 1);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn refresh_lease_serializes_network_requests_and_account_erasure_cleans_subscriptions(
    pool: PgPool,
) {
    let (state, stub, owner, viewer) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    music_podcasts::played(&state, owner.id, episode, true)
        .await
        .unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *stub.media_fetch_gate.lock().unwrap() = Some(gate.clone());
    let cloned = state.clone();
    let job =
        tokio::spawn(async move { music_podcasts::refresh(&cloned, show.id, viewer.id).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if stub.podcast_fetches.lock().unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        music_podcasts::refresh(&state, show.id, owner.id).await,
        Err(plamenu::error::ApiError::Conflict(_))
    ));
    assert_eq!(stub.podcast_fetches.lock().unwrap().len(), 2);
    gate.add_permits(1);
    job.await.unwrap().unwrap();
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(owner.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let left:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM music_podcast_subscriptions WHERE account_id=$1),(SELECT count(*) FROM music_podcast_plays WHERE account_id=$1)").bind(owner.id).fetch_one(&state.pool).await.unwrap();
    assert_eq!(left, (0, 0));
    assert!(
        music_podcasts::subscribe(&state, owner.id, URL)
            .await
            .is_err()
    );
    assert!(
        music_podcasts::played(&state, owner.id, episode, true)
            .await
            .is_err()
    );
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    fields: &[(&str, &str)],
) -> Response {
    let mut request = Request::builder()
        .uri(path)
        .method(method)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    app.clone()
        .oneshot(
            request
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn text(response: Response) -> String {
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
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_podcast_forms_use_one_music_workspace_and_require_csrf(pool: PgPool) {
    let (state, stub, owner, _) = fixture(pool).await;
    plamenu_db::user::create(
        &state.pool,
        owner.id,
        Some("alice@plamenu.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = request(
        &app,
        "POST",
        "/login",
        &[],
        &[
            ("email", "alice@plamenu.test"),
            ("password", "fixture-password"),
        ],
    )
    .await;
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
    let csrf = plamenu::auth::hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    // The file is first: validation must finish before any feed fetch, regardless
    // of multipart field order. A valid upload uses the same import path.
    let xml = format!(r#"<opml><body><outline xmlUrl="{URL}"/></body></opml>"#);
    for (token, expected) in [
        ("invalid", StatusCode::FORBIDDEN),
        (csrf.as_str(), StatusCode::OK),
    ] {
        let multipart = format!(
            "--podcast-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"podcasts.opml\"\r\nContent-Type: text/x-opml\r\n\r\n{xml}\r\n--podcast-test\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{token}\r\n--podcast-test--\r\n"
        );
        let result = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/web/music/podcasts/import-file")
                    .header(header::COOKIE, &cookie)
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=podcast-test",
                    )
                    .body(Body::from(multipart))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), expected);
        assert_eq!(
            stub.podcast_fetches.lock().unwrap().len(),
            usize::from(expected == StatusCode::OK)
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/web/music/podcasts/subscribe",
            &[("Cookie", &cookie)],
            &[("feed_url", URL)]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let subscribed = request(
        &app,
        "POST",
        "/web/music/podcasts/subscribe",
        &[("Cookie", &cookie)],
        &[("feed_url", URL), ("csrf", &csrf)],
    )
    .await;
    assert_eq!(subscribed.status(), StatusCode::SEE_OTHER);
    let location = subscribed.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let show = request(&app, "GET", &location, &[("Cookie", &cookie)], &[]).await;
    assert_eq!(show.status(), StatusCode::OK);
    assert_eq!(show.headers()[header::CACHE_CONTROL], "private, no-store");
    let html = text(show).await;
    assert!(html.contains("First episode"));
    assert!(html.contains("/music/mine"));
    assert!(html.contains("/web/music/play"));
    assert!(!html.contains("class=\"nav-link\" href=\"/music/podcasts\""));
    assert!(!html.contains("<script>bad()"));
    let id = location.rsplit('/').next().unwrap().parse().unwrap();
    let episodes = music_podcasts::episodes(&state, id, owner.id, false, None)
        .await
        .unwrap();
    let episode = episodes.items[0]["id"].as_str().unwrap();
    let page = format!("/music/podcasts/episodes/{episode}");
    let html = text(request(&app, "GET", &page, &[("Cookie", &cookie)], &[]).await).await;
    assert!(html.contains("<audio controls preload=\"none\""));
    assert!(html.contains("https://podcast.example/audio.mp3"));
    let action = format!("/web/music/podcasts/episodes/{episode}/played");
    assert_eq!(
        request(
            &app,
            "POST",
            &action,
            &[("Cookie", &cookie)],
            &[("enabled", "true")]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &action,
            &[("Cookie", &cookie)],
            &[("enabled", "true"), ("csrf", &csrf)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(
        music_podcasts::episode(&state, episode.parse().unwrap(), owner.id)
            .await
            .unwrap()
            .played
    );
    let unsub = format!("/web/music/podcasts/{id}/unsubscribe");
    assert_eq!(
        request(
            &app,
            "POST",
            &unsub,
            &[("Cookie", &cookie)],
            &[("csrf", &csrf)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(
        music_podcasts::list(&state, owner.id, &music_podcasts::Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn episodes_join_playlists_device_queues_and_elapsed_played_reports_without_social_posts(
    pool: PgPool,
) {
    use plamenu::{music_collections, music_listening, music_queues};
    let (state, stub, owner, _) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let playlist = music_collections::create(
        &state,
        owner.id,
        &music_collections::New {
            name: "Episodes".into(),
            kind: "playlist".into(),
            description: String::new(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let current = music_collections::get(&state, playlist.id, Some(owner.id))
            .await
            .unwrap();
        music_collections::add(
            &state,
            owner.id,
            playlist.id,
            &music_collections::Add {
                remote_id: None,
                revision: current.revision.to_string(),
                asset_id: None,
                status_id: None,
                episode_id: Some(episode.clone()),
            },
        )
        .await
        .unwrap();
    }
    let statistics = music_collections::statistics(&state, playlist.id, Some(owner.id))
        .await
        .unwrap();
    assert_eq!(statistics.track_count, 2);
    assert!((statistics.total_duration - 380.0).abs() < 0.000_001);
    let canonical = format!(
        "https://{}/music/podcasts/episodes/{episode}",
        common::TEST_DOMAIN
    );
    assert_eq!(
        music_collections::resolve_episode_reference(&state, &canonical)
            .await
            .unwrap(),
        Some(episode.clone())
    );
    assert!(
        music_collections::resolve_episode_reference(
            &state,
            &format!("{canonical}?token=forbidden")
        )
        .await
        .unwrap()
        .is_none()
    );
    let queue = music_queues::change(
        &state,
        owner.id,
        "web",
        &music_queues::Change {
            revision: "0".into(),
            action: music_queues::Action::Collection {
                collection_id: playlist.id.to_string(),
                replace: false,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 2);
    assert_eq!(
        queue.entries[0].url,
        format!("/music/podcasts/episodes/{episode}")
    );
    assert_eq!(
        queue.entries[0].stream_url,
        format!("/music/podcasts/episodes/{episode}/audio")
    );
    let session = music_listening::start(
        &state,
        owner.id,
        &music_listening::Start {
            remote_id: None,
            request_key: "podcast-play".into(),
            asset_id: None,
            status_id: None,
            episode_id: None,
            queue_device: Some("web".into()),
            queue_entry_id: Some(queue.entries[0].entry_id.clone()),
        },
    )
    .await
    .unwrap();
    let token = session.token.unwrap();
    assert!(
        !music_podcasts::episode(&state, episode.parse().unwrap(), owner.id)
            .await
            .unwrap()
            .played
    );
    for (seconds, counted) in [(15.0, false), (30.0, true)] {
        sqlx::query("UPDATE music_listen_sessions SET last_report_at=clock_timestamp()-interval '15 seconds' WHERE token=$1").bind(&token).execute(&state.pool).await.unwrap();
        let result = music_listening::progress(
            &state,
            owner.id,
            &token,
            &music_listening::Progress {
                played_seconds: seconds,
                finish: counted,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.counted, counted);
    }
    assert!(
        music_podcasts::episode(&state, episode.parse().unwrap(), owner.id)
            .await
            .unwrap()
            .played
    );
    music_podcasts::played(&state, owner.id, episode.parse().unwrap(), false)
        .await
        .unwrap();
    music_listening::progress(
        &state,
        owner.id,
        &token,
        &music_listening::Progress {
            played_seconds: 30.0,
            finish: true,
        },
    )
    .await
    .unwrap();
    assert!(
        !music_podcasts::episode(&state, episode.parse().unwrap(), owner.id)
            .await
            .unwrap()
            .played
    );
    serve(
        &stub,
        URL,
        Some(feed("Updated show", "Current episode")),
        "\"v2\"",
    );
    music_podcasts::refresh(&state, show.id, owner.id)
        .await
        .unwrap();
    let current = music_queues::get(&state, owner.id, "web").await.unwrap();
    assert_eq!(current.entries[0].title, "Current episode");
    assert_eq!(current.entries[0].artist, "Updated show");
    let history = music_listening::history(&state, owner.id, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].track.title, "Current episode");
    sqlx::query("DELETE FROM music_podcast_episodes WHERE id=$1")
        .bind(episode.parse::<i64>().unwrap())
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        music_queues::get(&state, owner.id, "web")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        music_collections::entries(&state, playlist.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        music_listening::history(&state, owner.id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let social: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(social, 0);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn enclosure_endpoint_serves_current_ranges_without_a_local_download_or_played_event(
    pool: PgPool,
) {
    let (state, stub, owner, _) = fixture(pool).await;
    stub.serve_media(
        "https://podcast.example/audio.mp3",
        "audio/mpeg",
        b"0123456789abcdefghijklmnopqrstuv".to_vec(),
    );
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let app = plamenu::build_router(state.clone());
    let path = format!("/music/podcasts/episodes/{episode}/audio");
    let response = request(&app, "GET", &path, &[("Range", "bytes=0-15")], &[]).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-15/32");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(text(response).await, "0123456789abcdef");
    assert!(
        !music_podcasts::episode(&state, episode.parse().unwrap(), owner.id)
            .await
            .unwrap()
            .played
    );
    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM music_assets")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tracks, 0);
    sqlx::query("UPDATE music_podcast_episodes SET audio_url='https://podcast.example/changed.mp3' WHERE id=$1").bind(episode.parse::<i64>().unwrap()).execute(&state.pool).await.unwrap();
    stub.serve_media(
        "https://podcast.example/changed.mp3",
        "audio/mpeg",
        b"changed-audio".to_vec(),
    );
    assert_eq!(
        text(request(&app, "GET", &path, &[], &[]).await).await,
        "changed-audio"
    );
    assert_eq!(
        stub.media_fetches.lock().unwrap().last().unwrap(),
        "https://podcast.example/changed.mp3"
    );
    sqlx::query("DELETE FROM music_podcast_episodes WHERE id=$1")
        .bind(episode.parse::<i64>().unwrap())
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, &[], &[]).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn podcast_api_scopes_preserve_private_subscriptions_and_current_per_listener_counts(
    pool: PgPool,
) {
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    let (state, _, owner, viewer) = fixture(pool).await;
    let app_record = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "podcast-tests",
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
    for (account, email) in [
        (owner.id, "alice@plamenu.test"),
        (viewer.id, "bob@plamenu.test"),
    ] {
        let user = user::create(
            &state.pool,
            account,
            Some(email),
            &hash_password("fixture-password").unwrap(),
        )
        .await
        .unwrap();
        for scope in ["read:statuses", "write:statuses"] {
            let token = generate_secret();
            oauth::create_token(
                &state.pool,
                &hash_secret(&token),
                app_record.id,
                Some(user.id),
                scope,
            )
            .await
            .unwrap();
            tokens.push(format!("Bearer {token}"));
        }
    }
    let app = plamenu::build_router(state.clone());
    assert_eq!(
        request(&app, "GET", "/api/v1/music/podcasts", &[], &[])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/music/podcasts",
            &[("Authorization", &tokens[1])],
            &[]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/music/podcasts",
            &[("Authorization", &tokens[0])],
            &[("feed_url", URL)]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let created = request(
        &app,
        "POST",
        "/api/v1/music/podcasts",
        &[("Authorization", &tokens[1])],
        &[("feed_url", URL)],
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        created.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let created: serde_json::Value = serde_json::from_str(&text(created).await).unwrap();
    assert_eq!(created["episode_count"], 1);
    assert_eq!(created["unplayed_count"], 1);
    assert_eq!(created["following"], true);
    let show = created["id"].as_str().unwrap().parse().unwrap();
    let episode = music_podcasts::episodes(&state, show, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let played = format!("/api/v1/music/podcasts/episodes/{episode}/played");
    assert_eq!(
        request(&app, "PUT", &played, &[("Authorization", &tokens[0])], &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "PUT", &played, &[("Authorization", &tokens[1])], &[])
            .await
            .status(),
        StatusCode::OK
    );
    let path = format!("/api/v1/music/podcasts/{show}");
    for (token, unplayed, following) in [(&tokens[0], 0, true), (&tokens[2], 1, false)] {
        let result = request(&app, "GET", &path, &[("Authorization", token)], &[]).await;
        assert_eq!(result.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_str(&text(result).await).unwrap();
        assert_eq!(result["unplayed_count"], unplayed);
        assert_eq!(result["following"], following);
    }
    let other: serde_json::Value = serde_json::from_str(
        &text(
            request(
                &app,
                "GET",
                "/api/v1/music/podcasts",
                &[("Authorization", &tokens[2])],
                &[],
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert!(other["items"].as_array().unwrap().is_empty());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/music/podcasts/opml",
            &[("Authorization", &tokens[1])],
            &[]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn unknown_episode_duration_keeps_resume_position_and_removal_clears_it(pool: PgPool) {
    use plamenu::music_queues::{self, Action, Change, Source};
    let (state, _, owner, _) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    sqlx::query("UPDATE music_podcast_episodes SET duration_seconds=NULL WHERE id=$1")
        .bind(episode)
        .execute(&state.pool)
        .await
        .unwrap();
    let mut queue = music_queues::change(
        &state,
        owner.id,
        "web",
        &Change {
            revision: "0".into(),
            action: Action::Add {
                sources: vec![Source {
                    remote_id: None,
                    asset_id: None,
                    status_id: None,
                    episode_id: Some(episode.to_string()),
                }],
            },
        },
    )
    .await
    .unwrap();
    let entry = queue.entries[0].entry_id.clone();
    queue = music_queues::change(
        &state,
        owner.id,
        "web",
        &Change {
            revision: queue.revision.clone(),
            action: Action::Select {
                entry_id: entry.clone(),
            },
        },
    )
    .await
    .unwrap();
    for (seconds, expected) in [(123.5, 123.5), (900_000.0, 604_800.0)] {
        queue = music_queues::change(
            &state,
            owner.id,
            "web",
            &Change {
                revision: queue.revision.clone(),
                action: Action::Seek {
                    entry_id: entry.clone(),
                    seconds,
                },
            },
        )
        .await
        .unwrap();
        assert!((queue.position_seconds - expected).abs() < 0.000_001);
        assert!(
            (music_queues::get(&state, owner.id, "web")
                .await
                .unwrap()
                .position_seconds
                - expected)
                .abs()
                < 0.000_001
        );
    }
    sqlx::query("DELETE FROM music_podcast_episodes WHERE id=$1")
        .bind(episode)
        .execute(&state.pool)
        .await
        .unwrap();
    let queue = music_queues::get(&state, owner.id, "web").await.unwrap();
    assert!(queue.current_entry_id.is_none());
    assert!(queue.position_seconds.abs() < f64::EPSILON);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn displayed_episode_queue_and_bulk_played_are_bounded_ordered_and_listener_private(
    pool: PgPool,
) {
    use plamenu::music_queues::{self, Action, Change};
    let (state, stub, owner, viewer) = fixture(pool).await;
    let mut body = String::from("<rss><channel><title>Many episodes</title>");
    for i in 0..53 {
        use std::fmt::Write as _;
        write!(body,"<item><guid>{i}</guid><title>Episode {i}</title><enclosure url=\"https://podcast.example/{i}.mp3\" type=\"audio/mpeg\"/><pubDate>{}</pubDate></item>",time::OffsetDateTime::from_unix_timestamp(1_791_504_000+i*3600).unwrap().format(&time::format_description::well_known::Rfc2822).unwrap()).unwrap();
    }
    body.push_str("</channel></rss>");
    serve(&stub, URL, Some(body.into_bytes()), "\"bulk\"");
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let first = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap();
    assert_eq!(first.items.len(), 50);
    let ids = first
        .items
        .iter()
        .map(|e| e["id"].as_str().unwrap().parse::<i64>().unwrap())
        .collect::<Vec<_>>();
    music_podcasts::played_batch(&state, owner.id, show.id, &ids, true)
        .await
        .unwrap();
    assert_eq!(
        music_podcasts::details(&state, show.id, owner.id)
            .await
            .unwrap()["unplayed_count"],
        3
    );
    assert_eq!(
        music_podcasts::details(&state, show.id, viewer.id)
            .await
            .unwrap()["unplayed_count"],
        53
    );
    let mut mixed = ids.clone();
    mixed[1] = i64::MAX;
    assert!(
        music_podcasts::played_batch(&state, owner.id, show.id, &mixed, false)
            .await
            .is_err()
    );
    assert_eq!(
        music_podcasts::details(&state, show.id, owner.id)
            .await
            .unwrap()["unplayed_count"],
        3
    );
    assert!(
        music_podcasts::played_batch(&state, owner.id, show.id, &[], true)
            .await
            .is_err()
    );
    assert!(
        music_podcasts::played_batch(&state, owner.id, show.id, &vec![ids[0]; 51], true)
            .await
            .is_err()
    );
    let queue = music_queues::change(
        &state,
        owner.id,
        "web",
        &Change {
            revision: "0".into(),
            action: Action::Podcast {
                podcast_id: show.id.to_string(),
                oldest: false,
                offset: None,
                replace: false,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 50);
    assert_eq!(queue.entries[0].title, "Episode 52");
    let queue = music_queues::change(
        &state,
        owner.id,
        "web",
        &Change {
            revision: queue.revision,
            action: Action::Podcast {
                podcast_id: show.id.to_string(),
                oldest: true,
                offset: Some(50),
                replace: true,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(queue.entries.len(), 3);
    assert_eq!(queue.entries[0].title, "Episode 50");
    assert_eq!(queue.entries[2].title, "Episode 52");
    music_podcasts::played_batch(&state, owner.id, show.id, &ids, false)
        .await
        .unwrap();
    assert_eq!(
        music_podcasts::details(&state, show.id, owner.id)
            .await
            .unwrap()["unplayed_count"],
        53
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn podcast_artwork_uses_current_catalogue_urls_and_rejects_active_content(pool: PgPool) {
    let (state, stub, owner, _) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let image = "https://podcast.example/cover.png";
    sqlx::query("UPDATE music_podcasts SET image_url=$2 WHERE id=$1")
        .bind(show.id)
        .bind(image)
        .execute(&state.pool)
        .await
        .unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8, 8)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    stub.serve_media(image, "image/png", png.into_inner());
    let app = plamenu::build_router(state.clone());
    for path in [
        format!("/music/podcasts/shows/{}/image", show.id),
        format!("/music/podcasts/episodes/{episode}/image"),
    ] {
        let response = request(&app, "GET", &path, &[], &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("image/")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(image::load_from_memory(&bytes).is_ok());
    }
    let changed = "https://podcast.example/changed.svg";
    sqlx::query("UPDATE music_podcast_episodes SET image_url=$2 WHERE id=$1")
        .bind(episode)
        .bind(changed)
        .execute(&state.pool)
        .await
        .unwrap();
    stub.serve_media(
        changed,
        "image/svg+xml",
        b"<svg><script>bad()</script></svg>".to_vec(),
    );
    let path = format!("/music/podcasts/episodes/{episode}/image");
    assert_eq!(
        request(&app, "GET", &path, &[], &[]).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(stub.media_fetches.lock().unwrap().last().unwrap(), changed);
    sqlx::query("DELETE FROM music_podcast_episodes WHERE id=$1")
        .bind(episode)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, &[], &[]).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn new_listener_refreshes_shared_catalogue_before_subscription_and_failures_preserve_it(
    pool: PgPool,
) {
    let (state, stub, owner, viewer) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    serve(&stub, URL, Some(b"<broken>".to_vec()), "\"broken\"");
    assert!(
        music_podcasts::subscribe(&state, viewer.id, URL)
            .await
            .is_err()
    );
    assert!(
        !music_podcasts::following(&state, show.id, viewer.id)
            .await
            .unwrap()
    );
    assert_eq!(
        music_podcasts::get(&state, show.id, owner.id)
            .await
            .unwrap()
            .title,
        "Show"
    );
    serve(
        &stub,
        URL,
        Some(feed("Current show", "Current episode")),
        "\"v2\"",
    );
    let current = music_podcasts::subscribe(&state, viewer.id, URL)
        .await
        .unwrap();
    assert_eq!(current.0.id, show.id);
    assert!(current.1);
    assert_eq!(current.0.title, "Current show");
    let fetches = stub.podcast_fetches.lock().unwrap().len();
    assert!(
        !music_podcasts::subscribe(&state, viewer.id, URL)
            .await
            .unwrap()
            .1
    );
    assert_eq!(stub.podcast_fetches.lock().unwrap().len(), fetches);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn selected_episodes_join_owned_collections_atomically_with_revision_and_library_rules(
    pool: PgPool,
) {
    use plamenu::music_collections::{self, New};
    let (state, _, owner, viewer) = fixture(pool).await;
    let show = music_podcasts::subscribe(&state, owner.id, URL)
        .await
        .unwrap()
        .0;
    let episode = music_podcasts::episodes(&state, show.id, owner.id, false, None)
        .await
        .unwrap()
        .items[0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    for kind in ["library", "playlist"] {
        let collection = music_collections::create(
            &state,
            owner.id,
            &New {
                name: "Podcast selection".into(),
                kind: kind.into(),
                description: String::new(),
                visibility: "private".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            music_collections::add_podcast_episodes(
                &state,
                viewer.id,
                collection.id,
                collection.revision,
                show.id,
                &[episode]
            )
            .await
            .is_err()
        );
        assert!(
            music_collections::add_podcast_episodes(
                &state,
                owner.id,
                collection.id,
                collection.revision,
                show.id,
                &[episode, i64::MAX]
            )
            .await
            .is_err()
        );
        assert!(
            music_collections::entries(&state, collection.id)
                .await
                .unwrap()
                .is_empty()
        );
        music_collections::add_podcast_episodes(
            &state,
            owner.id,
            collection.id,
            collection.revision,
            show.id,
            &[episode, episode],
        )
        .await
        .unwrap();
        let rows = music_collections::entries(&state, collection.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), if kind == "library" { 1 } else { 2 });
        assert!(
            music_collections::add_podcast_episodes(
                &state,
                owner.id,
                collection.id,
                collection.revision,
                show.id,
                &[episode]
            )
            .await
            .is_err()
        );
        let current = music_collections::get(&state, collection.id, Some(owner.id))
            .await
            .unwrap();
        music_collections::add_podcast_episodes(
            &state,
            owner.id,
            collection.id,
            current.revision,
            show.id,
            &[episode],
        )
        .await
        .unwrap();
        assert_eq!(
            music_collections::entries(&state, collection.id)
                .await
                .unwrap()
                .len(),
            if kind == "library" { 1 } else { 3 }
        );
    }
}
