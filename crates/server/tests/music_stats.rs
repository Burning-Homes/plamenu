mod common;
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::{
    AppState,
    error::ApiError,
    music_assets, music_listening,
    music_stats::{self, Group, ReleaseGroup, Request},
};
use plamenu_db::{PgPool, id};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let artist = id::next();
    let album = id::next();
    let asset = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Chart performer')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,release_year) VALUES($1,$2,$3,'Chart album',2021)").bind(album).bind(owner).bind(artist).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,genre,release_year,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Chart recording',$3,$4,'Rock; rock|Ambient',2021,'private',$5,$6,100,100,$7,10,'mp3')").bind(asset).bind(owner).bind(artist).bind(album).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    (state, owner, viewer, asset)
}
async fn listen(
    state: &AppState,
    owner: i64,
    asset: Option<i64>,
    status: Option<i64>,
    at: &str,
    counted: bool,
) {
    let event = id::next();
    let at = OffsetDateTime::parse(at, &Rfc3339).unwrap();
    sqlx::query("INSERT INTO music_listen_sessions(id,token,account_id,request_key,asset_id,status_id,duration,threshold_seconds,played_seconds,counted_at,history_id) VALUES($1,$2,$3,$4,$5,$6,10,5,6,$7,$8)").bind(event).bind(format!("{event:064x}")).bind(owner).bind(format!("listen_{event}")).bind(asset).bind(status).bind(counted.then_some(at)).bind(counted.then(id::next)).execute(&state.pool).await.unwrap();
}
fn request(from: &str, to: &str, zone: &str) -> Request {
    Request {
        from: Some(from.into()),
        to: Some(to.into()),
        tz: Some(zone.into()),
        ..Default::default()
    }
}
#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_counts_metadata_genres_artwork_and_publication_aliases_remain_canonical(
    pool: PgPool,
) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2026-03-08T06:30:00Z",
        true,
    )
    .await;
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2026-03-08T07:30:00Z",
        true,
    )
    .await;
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2026-03-08T07:40:00Z",
        false,
    )
    .await;
    let args = request("2026-03-08", "2026-03-08", "America/New_York");
    let report = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(report.play_count, 2);
    assert_eq!(report.top.tracks.len(), 1);
    assert_eq!(report.top.tracks[0].play_count, 2);
    assert_eq!(report.top.artists[0].name, "Chart performer");
    assert_eq!(report.top.albums[0].name, "Chart album");
    assert_eq!(report.top.genres.len(), 2);
    assert!(report.top.genres.iter().all(|g| g.play_count == 2));
    assert_eq!(report.releases[0].bucket, "2020s");
    assert_eq!(report.releases[0].count, 2);
    assert_eq!(report.plays.len(), 1);
    assert_eq!(report.plays[0].count, 2);
    assert_eq!(report.genres_timeline[0].genres.len(), 2);
    assert_eq!(report.clock.len(), 24);
    assert_eq!(report.clock[1].count, 1);
    assert_eq!(report.clock[2].count, 0);
    assert_eq!(report.clock[3].count, 1);
    assert_eq!(
        music_stats::report(&state, viewer, &args)
            .await
            .unwrap()
            .play_count,
        0
    );
    let album: i64 = sqlx::query_scalar("SELECT album_id FROM music_assets WHERE id=$1")
        .bind(asset)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let art = id::next();
    sqlx::query("INSERT INTO music_artworks(id,owner_id,album_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,'cover',$4,'image/png',100,20,20,$5)").bind(art).bind(owner).bind(album).bind(format!("musicart{art}.png")).bind(id::next()).execute(&state.pool).await.unwrap();
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let actor = plamenu_db::account::find_by_id(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    let publication = plamenu::music_publications::publish(&state, &actor, asset)
        .await
        .unwrap();
    listen(
        &state,
        owner,
        None,
        Some(publication.id),
        "2026-03-08T08:30:00Z",
        true,
    )
    .await;
    let report = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(report.play_count, 3);
    assert_eq!(
        report.top.tracks.len(),
        1,
        "A real publication edge folds onto its retained source"
    );
    assert_eq!(
        report.top.tracks[0].url,
        Some(format!("/music/assets/{asset}"))
    );
    assert_eq!(report.top.tracks[0].play_count, 3);
    assert_eq!(
        report.top.albums[0].image_url,
        Some(format!("/music/artwork/{art}"))
    );
    assert_eq!(
        report.top.tracks[0].image_url,
        Some(format!("/music/artwork/{art}"))
    );
    assert_eq!(
        report.plays.iter().map(|p| p.count).sum::<i64>(),
        report.play_count
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn calendar_zero_fills_week_starts_leap_months_and_fall_dst_hours(pool: PgPool) {
    let (state, owner, _, asset) = fixture(pool).await;
    for at in ["2026-11-01T05:30:00Z", "2026-11-01T06:30:00Z"] {
        listen(&state, owner, Some(asset), None, at, true).await;
    }
    let day = music_stats::report(
        &state,
        owner,
        &request("2026-11-01", "2026-11-02", "America/New_York"),
    )
    .await
    .unwrap();
    assert_eq!(day.clock[1].count, 2);
    assert_eq!(day.plays.len(), 2);
    assert_eq!(day.plays[0].count, 2);
    assert_eq!(day.plays[1].count, 0);
    let mut args = request("2026-11-01", "2026-11-02", "America/New_York");
    args.group_by = Group::Week;
    args.week_start = Some(6);
    let sunday = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(sunday.plays.len(), 1);
    assert_eq!(sunday.plays[0].bucket, "2026-11-01");
    assert_eq!(sunday.genres_timeline[0].bucket, "2026-11-01");
    assert!(
        sunday.genres_timeline[0]
            .genres
            .iter()
            .all(|g| g.count == 2)
    );
    args.week_start = Some(0);
    let monday = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(monday.plays.len(), 2);
    assert_eq!(monday.plays[0].bucket, "2026-10-26");
    assert_eq!(monday.plays[0].count, 2);
    assert_eq!(monday.plays[1].count, 0);
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2024-02-29T23:59:59Z",
        true,
    )
    .await;
    let mut args = request("2024-02-01", "2024-03-31", "UTC");
    args.group_by = Group::Month;
    let month = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(month.plays.len(), 2);
    assert_eq!(month.plays[0].bucket, "2024-02");
    assert_eq!(month.plays[0].count, 1);
    assert_eq!(month.plays[1].count, 0);
    args.group_by = Group::Year;
    let year = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(year.plays.len(), 1);
    assert_eq!(year.plays[0].bucket, "2024");
    assert_eq!(year.plays[0].count, 1);
    let kathmandu = music_stats::report(
        &state,
        owner,
        &request("2024-03-01", "2024-03-01", "Asia/Kathmandu"),
    )
    .await
    .unwrap();
    assert_eq!(kathmandu.play_count, 1);
    assert_eq!(kathmandu.clock[5].count, 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn every_chart_respects_current_shares_blocks_deletion_retirement_and_erase(pool: PgPool) {
    let (state, owner, viewer, asset) = fixture(pool).await;
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    listen(
        &state,
        viewer,
        Some(asset),
        None,
        "2026-03-08T12:30:00Z",
        true,
    )
    .await;
    let args = request("2026-03-01", "2026-03-31", "UTC");
    assert_eq!(
        music_stats::report(&state, viewer, &args)
            .await
            .unwrap()
            .play_count,
        1
    );
    music_assets::grant(&state, owner, asset, viewer, false)
        .await
        .unwrap();
    let hidden = music_stats::report(&state, viewer, &args).await.unwrap();
    assert_eq!(hidden.play_count, 0);
    assert!(hidden.top.tracks.is_empty());
    assert!(hidden.top.artists.is_empty());
    assert!(hidden.top.albums.is_empty());
    assert!(hidden.top.genres.is_empty());
    assert!(hidden.releases.is_empty());
    assert!(hidden.plays.iter().all(|p| p.count == 0));
    assert!(hidden.clock.iter().all(|h| h.count == 0));
    assert!(hidden.genres_timeline.iter().all(|p| p.genres.is_empty()));
    assert!(
        !serde_json::to_string(&hidden)
            .unwrap()
            .contains("Chart recording")
    );
    music_assets::grant(&state, owner, asset, viewer, true)
        .await
        .unwrap();
    plamenu_db::block::create(&state.pool, viewer, owner, None)
        .await
        .unwrap();
    assert_eq!(
        music_stats::report(&state, viewer, &args)
            .await
            .unwrap()
            .play_count,
        0
    );
    sqlx::query("DELETE FROM blocks")
        .execute(&state.pool)
        .await
        .unwrap();
    music_listening::erase(&state, viewer).await.unwrap();
    assert_eq!(
        music_stats::report(&state, viewer, &args)
            .await
            .unwrap()
            .play_count,
        0
    );
    listen(
        &state,
        viewer,
        Some(asset),
        None,
        "2026-03-08T12:30:00Z",
        true,
    )
    .await;
    music_assets::delete(&state, owner, asset).await.unwrap();
    assert_eq!(
        music_stats::report(&state, viewer, &args)
            .await
            .unwrap()
            .play_count,
        0
    );
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(viewer)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        music_stats::report(&state, viewer, &args).await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn remote_metadata_keeps_origin_and_never_merges_into_same_named_local_entities(
    pool: PgPool,
) {
    let (state, owner, _, asset) = fixture(pool).await;
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2026-03-08T12:30:00Z",
        true,
    )
    .await;
    let remote = RemoteUser::new("songhive.test", "sylvia");
    let author = plamenu::remote::store_remote_actor(&state.pool, &remote.actor)
        .await
        .unwrap();
    let document = serde_json::json!({"id":"https://songhive.test/ap/audio/chart-track","type":"Audio","attributedTo":remote.actor.id,"name":"Chart recording","to":["https://www.w3.org/ns/activitystreams#Public"],"url":[{"type":"Link","mediaType":"audio/mpeg","href":"https://songhive.test/track.mp3"}],"track":{"name":"Chart recording","artists":[{"name":"Chart performer"}],"album":{"name":"Chart album","released":"1999-01-01"}}});
    let audio = plamenu::ingest::ingest_remote_note(&state, &author, &document)
        .await
        .unwrap();
    listen(
        &state,
        owner,
        None,
        Some(audio.id),
        "2026-03-08T13:30:00Z",
        true,
    )
    .await;
    let mut args = request("2026-03-01", "2026-03-31", "UTC");
    args.release_group = ReleaseGroup::Year;
    let report = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(report.play_count, 2);
    assert_eq!(report.top.tracks.len(), 2);
    assert_eq!(report.top.artists.len(), 2);
    assert_eq!(report.top.albums.len(), 2);
    assert!(
        report
            .top
            .tracks
            .iter()
            .any(|t| t.url.as_deref() == Some("https://songhive.test/ap/audio/chart-track"))
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_assets")
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        1,
        "Remote statistics never copy a track into local ownership"
    );
    let mut second = document.clone();
    second["id"] = serde_json::json!("https://songhive.test/ap/audio/chart-second");
    let second_audio = plamenu::ingest::ingest_remote_note(&state, &author, &second)
        .await
        .unwrap();
    listen(
        &state,
        owner,
        None,
        Some(second_audio.id),
        "2026-03-08T14:30:00Z",
        true,
    )
    .await;
    // Different AP objects can link to one human-facing player page. That web
    // URL is a display link and cannot establish a shared music identity.
    sqlx::query("UPDATE statuses SET url='https://songhive.test/player' WHERE id=ANY($1)")
        .bind([audio.id, second_audio.id].as_slice())
        .execute(&state.pool)
        .await
        .unwrap();
    let separate = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(separate.play_count, 3);
    assert_eq!(separate.top.tracks.len(), 3);
    assert_eq!(
        separate
            .top
            .tracks
            .iter()
            .map(|t| &t.key)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        separate
            .top
            .tracks
            .iter()
            .filter(|t| t.url.as_deref() == Some("https://songhive.test/player"))
            .count(),
        2
    );
    plamenu_db::status::delete_by_uri(&state.pool, second_audio.uri.as_deref().unwrap(), author.id)
        .await
        .unwrap();
    plamenu_db::status::delete_by_uri(&state.pool, audio.uri.as_deref().unwrap(), author.id)
        .await
        .unwrap();
    let report = music_stats::report(&state, owner, &args).await.unwrap();
    assert_eq!(report.play_count, 1);
    assert_eq!(report.top.tracks.len(), 1);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn unsupported_inputs_fail_before_large_scans_and_full_iana_aliases_work(pool: PgPool) {
    let (state, owner, _, _) = fixture(pool).await;
    for args in [
        request("2026-04-01", "2026-03-01", "UTC"),
        request("bad", "2026-03-01", "UTC"),
        request("2026-03-01", "2026-03-02", "No/Such_Zone"),
        Request {
            week_start: Some(7),
            ..Default::default()
        },
        Request {
            limit: Some(101),
            ..Default::default()
        },
        request("1900-01-01", "2026-03-02", "UTC"),
    ] {
        assert!(matches!(
            music_stats::report(&state, owner, &args).await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    let report = music_stats::report(
        &state,
        owner,
        &request("2026-03-01", "2026-03-02", "Europe/Vienna"),
    )
    .await
    .unwrap();
    assert_eq!(report.period.tz, "Europe/Vienna");
    assert_eq!(report.clock.len(), 24);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn stats_read_scope_and_private_cache_policy_match_the_music_api(pool: PgPool) {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use plamenu::auth::{generate_secret, hash_password, hash_secret};
    use plamenu_db::{oauth, user};
    use tower::ServiceExt;
    let (state, owner, _, _) = fixture(pool).await;
    let u = user::create(
        &state.pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let client = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "chart-tests",
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
    for (token, scope) in [(&read, "read:statuses"), (&write, "write:statuses")] {
        oauth::create_token(
            &state.pool,
            &hash_secret(token),
            client.id,
            Some(u.id),
            scope,
        )
        .await
        .unwrap();
    }
    let app = plamenu::build_router(state);
    for (token, status) in [(&read, StatusCode::OK), (&write, StatusCode::FORBIDDEN)] {
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/music/stats")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
        }
    }
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri("/api/v1/music/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn aggregation_query_count_does_not_grow_per_listen(pool: PgPool) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::{Layer, layer::Context, prelude::*};
    struct Counter(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> Layer<S> for Counter {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target().starts_with("sqlx::query") {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let (state, owner, _, asset) = fixture(pool).await;
    let args = request("2026-03-01", "2026-03-31", "UTC");
    listen(
        &state,
        owner,
        Some(asset),
        None,
        "2026-03-08T12:30:00Z",
        true,
    )
    .await;
    // Warm ordinary rendering state before measuring each identical source set.
    music_stats::report(&state, owner, &args).await.unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Counter(counter.clone())),
    );
    counter.store(0, Ordering::Relaxed);
    let first = music_stats::report(&state, owner, &args).await.unwrap();
    let single = counter.load(Ordering::Relaxed);
    assert_eq!(first.play_count, 1);
    assert!(single > 0, "The counter must actually observe SQL");
    for _ in 0..40 {
        listen(
            &state,
            owner,
            Some(asset),
            None,
            "2026-03-08T12:30:00Z",
            true,
        )
        .await;
    }
    counter.store(0, Ordering::Relaxed);
    let many = music_stats::report(&state, owner, &args).await.unwrap();
    let repeated = counter.load(Ordering::Relaxed);
    assert_eq!(many.play_count, 41);
    assert_eq!(many.top.tracks[0].play_count, 41);
    assert!(
        repeated <= single + 1,
        "Query count grew per event: {single} -> {repeated}"
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn all_source_batches_count_without_collapsing_identical_titles_or_hidden_sources(
    pool: PgPool,
) {
    let (state, _, viewer, template) = fixture(pool).await;
    for index in 0..121 {
        let asset = id::next();
        let visibility = if index < 105 { "public" } else { "private" };
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,genre,release_year,visibility,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) SELECT $1,owner_id,title,artist_id,album_id,genre,release_year,$2,$3,$4,100,100,$5,10,'mp3' FROM music_assets WHERE id=$6").bind(asset).bind(visibility).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(format!("{asset:064x}")).bind(template).execute(&state.pool).await.unwrap();
        listen(
            &state,
            viewer,
            Some(asset),
            None,
            "2026-03-08T12:30:00Z",
            true,
        )
        .await;
    }
    let mut args = request("2026-03-01", "2026-03-31", "UTC");
    args.limit = Some(100);
    let report = music_stats::report(&state, viewer, &args).await.unwrap();
    assert_eq!(report.play_count, 105);
    assert_eq!(report.top.tracks.len(), 100);
    assert_eq!(report.top.artists[0].play_count, 105);
    assert_eq!(report.top.albums[0].play_count, 105);
    let unique = report
        .top
        .tracks
        .iter()
        .filter_map(|t| t.url.as_ref())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique.len(),
        100,
        "Equal titles never make unrelated tracks the same identity"
    );
    assert_eq!(report.clock[12].count, 105);
    assert_eq!(report.plays.iter().map(|p| p.count).sum::<i64>(), 105);
}
