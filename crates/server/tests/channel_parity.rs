//! Channel identity reuses account profiles without inheriting community powers.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{RemoteUser, StubFederation, create_local_account};
use http_body_util::BodyExt;
use plamenu_db::{PgPool, account, id, status};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn film(pool: &PgPool, author: i64, channel: i64) -> status::Status {
    let stored = status::create_local(
        pool,
        status::NewLocalStatus::new(author, "<p>A channel film</p>", "public", None),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE statuses SET object_type='Video', title='A channel film' WHERE id=$1")
        .bind(stored.id)
        .execute(pool)
        .await
        .unwrap();
    let mid = id::next();
    sqlx::query("INSERT INTO media_attachments(id,account_id,status_id,file_name,small_file_name,content_type,kind) VALUES($1,$2,$3,'fixture.mp4','poster.png','video/mp4','video')").bind(mid).bind(author).bind(stored.id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO video_publications(status_id,channel_id,media_id) VALUES($1,$2,$3)")
        .bind(stored.id)
        .bind(channel)
        .bind(mid)
        .execute(pool)
        .await
        .unwrap();
    status::find_by_id(pool, stored.id).await.unwrap().unwrap()
}

async fn page(state: &plamenu::AppState, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        request = request.header("Cookie", format!("__Host-plamenu_session={cookie}"));
    }
    let response = plamenu::build_router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn cookie(state: &plamenu::AppState, person: i64) -> String {
    let user = plamenu_db::user::set_credentials(
        &state.pool,
        person,
        Some(&format!("fixture-{person}@example.test")),
        "unused",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(state).await.unwrap();
    let raw = format!("channel-fixture-{}", id::next());
    plamenu_db::oauth::create_token(
        &state.pool,
        &plamenu::auth::hash_secret(&raw),
        app.id,
        Some(user.id),
        "read write",
    )
    .await
    .unwrap();
    raw
}

async fn context(state: &plamenu::AppState, stored: &status::Status, viewer: Option<i64>) -> Value {
    plamenu::entities::render_status(&state.pool, &state.config.domain, stored, viewer)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "one batch-to-retirement attribution lifecycle keeps all privacy assertions on the same publications"
)]
async fn channel_publication_context_is_batched_truthful_and_visibility_guarded(pool: PgPool) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::prelude::*;
    let author = create_local_account(&pool, "alice", "Alice").await;
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let mut films = Vec::new();
    let mut channels = Vec::new();
    for n in 0..8 {
        let channel = plamenu::video::create_channel(
            &state,
            &author,
            &format!("films{n}"),
            &format!("Films {n}"),
        )
        .await
        .unwrap();
        films.push(film(&pool, author.id, channel.id).await);
        channels.push(channel);
    }
    // Warm connections/query preparation before measuring either batch.
    plamenu::entities::render_statuses(&pool, &state.config.domain, &films, Some(viewer.id))
        .await
        .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(common::QueryCounter(counter.clone()));
    let (small_count, big_count) = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut counts = Vec::new();
        for batch in [&films[..1], &films[..]] {
            counter.store(0, Ordering::Relaxed);
            let rendered = plamenu::entities::render_statuses(
                &pool,
                &state.config.domain,
                batch,
                Some(viewer.id),
            )
            .await
            .unwrap();
            counts.push(counter.load(Ordering::Relaxed));
            for (index, entity) in rendered.iter().enumerate() {
                assert_eq!(entity["account"]["id"], author.id.to_string());
                assert_eq!(
                    entity["video_channels"][0]["id"],
                    channels[index].id.to_string()
                );
                assert_eq!(entity["video_channels"][0]["video_channel"], true);
                assert_eq!(entity["groups"], json!([]));
                assert_eq!(entity["group_post"], false);
            }
        }
        (counts[0], counts[1])
    };
    assert!(small_count > 0, "query counter must observe SQL");
    assert_eq!(
        small_count, big_count,
        "channel contexts cannot add per-card SQL"
    );
    eprintln!("channel contexts: 1={small_count} SQL reads, 8={big_count}");
    let streamed = plamenu::entities::render_status_for_viewers(
        &pool,
        &state.config.domain,
        &films[0],
        &[viewer.id, author.id],
    )
    .await
    .unwrap();
    for who in [viewer.id, author.id] {
        assert_eq!(streamed[&who], context(&state, &films[0], Some(who)).await);
    }
    let stored = &films[0];
    let boost = status::create_local_reblog(&pool, viewer.id, stored.id)
        .await
        .unwrap();
    let boosted = context(&state, &boost, Some(viewer.id)).await;
    assert_eq!(boosted["video_channels"], json!([]));
    assert_eq!(
        boosted["reblog"]["video_channels"],
        context(&state, stored, Some(viewer.id)).await["video_channels"],
        "publication context belongs to the original, not its booster"
    );
    for target in [channels[0].id, author.id] {
        plamenu_db::block::create(&pool, viewer.id, target, None)
            .await
            .unwrap();
        assert_eq!(
            context(&state, stored, Some(viewer.id)).await["video_channels"],
            json!([])
        );
        plamenu_db::block::delete(&pool, viewer.id, target)
            .await
            .unwrap();
    }
    for visibility in ["private", "direct"] {
        sqlx::query("UPDATE statuses SET visibility=$2 WHERE id=$1")
            .bind(stored.id)
            .bind(visibility)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            context(&state, stored, None).await["video_channels"],
            json!([])
        );
    }
    sqlx::query("UPDATE statuses SET visibility='public',deleted_at=now() WHERE id=$1")
        .bind(stored.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        context(&state, stored, None).await["video_channels"],
        json!([]),
        "tombstone has the Status extension shape without exposing publication context"
    );
    sqlx::query("UPDATE statuses SET deleted_at=NULL WHERE id=$1")
        .bind(stored.id)
        .execute(&pool)
        .await
        .unwrap();
    for target in [channels[0].id, author.id] {
        sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
            .bind(target)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            context(&state, stored, None).await["video_channels"],
            json!([])
        );
        sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
            .bind(target)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM video_publications WHERE status_id=$1")
        .bind(stored.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        context(&state, stored, None).await["video_channels"],
        json!([])
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_context_survives_bare_video_surfaces_without_community_controls(pool: PgPool) {
    let author = create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    common::open_previews(&pool).await;
    let channel = plamenu::video::create_channel(&state, &author, "films", "Films")
        .await
        .unwrap();
    let stored = film(&pool, author.id, channel.id).await;
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    plamenu_db::follow::create(&pool, viewer.id, author.id, None)
        .await
        .unwrap();
    plamenu_db::notification::create(&pool, viewer.id, author.id, "status", Some(stored.id))
        .await
        .unwrap();
    let viewer_cookie = cookie(&state, viewer.id).await;
    for path in [
        format!("/@alice/{}", stored.id),
        "/@alice".into(),
        "/public".into(),
        "/public?local=true".into(),
        "/".into(),
        "/notifications".into(),
        "/search?q=channel".into(),
    ] {
        let (code, html) = page(&state, &path, Some(&viewer_cookie)).await;
        assert_eq!(code, StatusCode::OK, "{path}");
        assert!(
            html.contains("published on") && html.contains("href=\"/@films\""),
            "{path}: context"
        );
        assert!(
            html.contains("/@alice") && html.contains("Alice"),
            "{path}: actual author"
        );
        assert!(
            !html.contains("status__vote") && !html.contains("/web/groups/"),
            "{path}: no community powers"
        );
        if let Ok(dir) = std::env::var("PLAMENU_CHANNEL_PARITY_EVIDENCE") {
            let label = if path.contains(&stored.id.to_string()) {
                "detail"
            } else if path == "/@alice" {
                "profile"
            } else {
                "public"
            };
            std::fs::write(format!("{dir}/{label}.html"), html).unwrap();
        }
    }
    status::create_local_reblog(&pool, viewer.id, stored.id)
        .await
        .unwrap();
    let (_, boosted) = page(&state, "/@viewer", Some(&viewer_cookie)).await;
    assert!(boosted.contains("published on") && boosted.contains("href=\"/@films\""));
    let reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(author.id, "<p>Reply</p>", "public", Some(stored.id)),
    )
    .await
    .unwrap();
    let (_, html) = page(
        &state,
        &format!("/@alice/{}", reply.id),
        Some(&viewer_cookie),
    )
    .await;
    assert!(html.contains("published on") && html.contains("href=\"/@films\""));
    let (_, catalogue) = page(&state, "/videos/explore", Some(&viewer_cookie)).await;
    assert!(catalogue.contains(&format!("/videos/explore?channel={}", channel.id)));
    browser_proof(&state, &author, channel.id, stored.id).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn verified_remote_video_channel_is_not_a_community_announce(pool: PgPool) {
    let author = RemoteUser::new("remote.test", "uploader");
    let mut channel = RemoteUser::new("remote.test", "films");
    channel.actor.kind = "Group".into();
    channel.actor.attributed_to = Some(json!([author.actor.id]));
    let mut stranger = RemoteUser::new("remote.test", "community");
    stranger.actor.kind = "Group".into();
    let stub = StubFederation::with_users(&[&author, &channel, &stranger]);
    let state = common::test_state_with(pool.clone(), stub.clone());
    let url = "https://remote.test/videos/42";
    stub.objects.lock().unwrap().insert(url.into(),json!({"id":url,"type":"Video","name":"Remote film","attributedTo":[{"type":"Person","id":author.actor.id},{"type":"Group","id":channel.actor.id}],"to":[plamenu_ap::activity::PUBLIC],"published":"2026-10-01T00:00:00Z","url":[{"type":"Link","mediaType":"video/mp4","href":"https://remote.test/media/42.mp4"}]}));
    let stored = plamenu::ingest::resolve_or_fetch_status(&state, url)
        .await
        .unwrap()
        .unwrap();
    let channel = account::find_by_uri(&pool, &channel.actor.id)
        .await
        .unwrap()
        .unwrap();
    let uploader = account::find_by_uri(&pool, &author.actor.id)
        .await
        .unwrap()
        .unwrap();
    let group = plamenu::remote::store_remote_actor(&pool, &stranger.actor)
        .await
        .unwrap();
    let wrapper = status::upsert_remote_reblog(
        &pool,
        "https://remote.test/announces/42",
        channel.id,
        stored.id,
        None,
    )
    .await
    .unwrap();
    let rendered = context(&state, &wrapper.unwrap(), None).await;
    assert_eq!(rendered["account"]["video_channel"], true);
    assert_eq!(rendered["reblog"]["account"]["id"], uploader.id.to_string());
    assert_eq!(
        rendered["reblog"]["video_channels"][0]["id"],
        channel.id.to_string()
    );
    assert_eq!(rendered["reblog"]["group_post"], false);
    assert!(
        plamenu_db::group::boosting_group_ids(&pool, stored.id)
            .await
            .unwrap()
            .is_empty()
    );
    let single = plamenu::entities::account_json(&pool, &state.config.domain, &channel, None)
        .await
        .unwrap();
    let batch = plamenu::entities::render_accounts(
        &pool,
        &state.config.domain,
        &[channel.clone(), group.clone()],
        None,
    )
    .await
    .unwrap();
    assert_eq!(single, batch[0]);
    assert_eq!(single["video_channel"], true);
    assert_eq!(batch[1]["video_channel"], false);
    status::upsert_remote_reblog(
        &pool,
        "https://remote.test/community/42",
        group.id,
        stored.id,
        None,
    )
    .await
    .unwrap();
    let bare = context(&state, &stored, None).await;
    assert_eq!(
        bare["group_post"], true,
        "unverified unrelated Group Announce keeps community semantics"
    );
    assert_eq!(bare["groups"][0]["id"], group.id.to_string());
    assert_eq!(
        plamenu_db::group::boosting_group_ids(&pool, stored.id)
            .await
            .unwrap(),
        vec![group.id]
    );
    plamenu_db::video::set_remote_channel(&pool, stored.id, None)
        .await
        .unwrap();
    assert_eq!(
        context(&state, &stored, None).await["video_channels"],
        json!([])
    );
    assert_eq!(
        plamenu::entities::account_json(&pool, &state.config.domain, &channel, None)
            .await
            .unwrap()["video_channel"],
        false
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_known_remote_channel_cannot_claim_another_uploaders_video(pool: PgPool) {
    let owner = RemoteUser::new("remote.test", "owner");
    let uploader = RemoteUser::new("remote.test", "uploader");
    let mut channel = RemoteUser::new("remote.test", "films");
    channel.actor.kind = "Group".into();
    channel.actor.attributed_to = Some(json!([owner.actor.id]));
    let stub = StubFederation::with_users(&[&owner, &uploader, &channel]);
    let state = common::test_state_with(pool.clone(), stub.clone());
    let mut stored = Vec::new();
    for (number, author) in [(1, &owner), (2, &uploader)] {
        let url = format!("https://remote.test/videos/{number}");
        stub.objects.lock().unwrap().insert(url.clone(), json!({
            "id":url,"type":"Video","name":"Remote film",
            "attributedTo":[{"type":"Person","id":author.actor.id},{"type":"Group","id":channel.actor.id}],
            "to":[plamenu_ap::activity::PUBLIC],"published":"2026-10-01T00:00:00Z",
            "url":[{"type":"Link","mediaType":"video/mp4","href":"https://remote.test/media/film.mp4"}]
        }));
        stored.push(
            plamenu::ingest::resolve_or_fetch_status(&state, &url)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let channel = account::find_by_uri(&pool, &channel.actor.id)
        .await
        .unwrap()
        .unwrap();
    let channel_entity =
        plamenu::entities::account_json(&pool, &state.config.domain, &channel, None)
            .await
            .unwrap();
    assert_eq!(
        channel_entity["video_channel"], true,
        "the first publication verifies this actor"
    );
    assert!(
        channel_entity["avatar"]
            .as_str()
            .unwrap()
            .ends_with("/missing.png")
    );
    assert_eq!(
        context(&state, &stored[0], None).await["video_channels"][0]["id"],
        channel.id.to_string()
    );
    assert_eq!(
        context(&state, &stored[1], None).await["video_channels"],
        json!([]),
        "channel ownership does not name this uploader"
    );
    let wrapper = status::upsert_remote_reblog(
        &pool,
        "https://remote.test/announces/other",
        channel.id,
        stored[1].id,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let rendered = context(&state, &wrapper, None).await;
    assert_eq!(rendered["account"]["video_channel"], true);
    assert_eq!(rendered["reblog"]["video_channels"], json!([]));
    assert_eq!(
        rendered["reblog"]["groups"][0]["id"],
        channel.id.to_string()
    );
    assert_eq!(
        rendered["reblog"]["group_post"], true,
        "unverified Announce retains community semantics"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn channel_profile_outbox_failure_rolls_back_images_and_cleans_strays(pool: PgPool) {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let remote = RemoteUser::new("remote.test", "viewer");
    let remote = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, remote.id, channel.id, None)
        .await
        .unwrap();
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32,
        32,
        image::Rgb([160, 90, 220]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .unwrap();
    let edits = || plamenu::video_channels::ProfileEdit {
        display_name: Some("Updated films".into()),
        avatar: Some(png.clone()),
        avatar_description: Some("Purple square".into()),
        ..Default::default()
    };
    let signing = plamenu::key_store::account_signing_key(&state, channel.id, "rsa")
        .await
        .unwrap()
        .record;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION reject_channel_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected channel outbox failure'; END; $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_channel_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_channel_delivery()").execute(&pool).await.unwrap();
    assert!(
        plamenu::video_channels::update_profile(&state, &owner, channel.id, edits())
            .await
            .is_err()
    );
    let current = account::find_by_id(&pool, channel.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.display_name, channel.display_name);
    assert!(current.avatar_file_name.is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        count
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_cleanup_jobs")
            .fetch_one(&pool)
            .await
            .unwrap()
            > 0,
        "orphan image has durable cleanup"
    );
    sqlx::query("DROP TRIGGER reject_channel_delivery ON delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        account::rename_local(&pool, channel.id, "films_renamed")
            .await
            .unwrap()
    );
    plamenu::video_channels::update_profile(&state, &owner, channel.id, edits())
        .await
        .unwrap();
    let activity: Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE account_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(channel.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(activity["type"], "Update");
    assert_eq!(activity["actor"], channel.uri.as_deref().unwrap());
    assert_eq!(activity["object"]["icon"]["summary"], "Purple square");
    assert_eq!(activity["object"]["id"], channel.uri.as_deref().unwrap());
    assert_eq!(activity["object"]["preferredUsername"], "films_renamed");
    assert_eq!(activity["object"]["publicKey"]["id"], signing.key_uri);
    let current_key = plamenu::key_store::account_signing_key(&state, channel.id, "rsa")
        .await
        .unwrap()
        .record;
    assert_eq!(current_key.key_uri, signing.key_uri);
    assert_eq!(current_key.controller_uri, channel.uri.as_deref().unwrap());
    assert_eq!(
        activity["object"]["publicKey"]["owner"],
        channel.uri.as_deref().unwrap()
    );
}

/// Optional visual proof against this test's disposable HTTP fixture.
async fn browser_proof(
    state: &plamenu::AppState,
    owner: &account::Account,
    channel: i64,
    stored: i64,
) {
    // Optional local browser proof runs on this same disposable fixture. Ordinary
    // regression execution never requires Docker or an installed browser.
    if let (Ok(driver), Ok(dir)) = (
        std::env::var("PLAMENU_CHANNEL_PARITY_BROWSER"),
        std::env::var("PLAMENU_CHANNEL_PARITY_EVIDENCE"),
    ) {
        let owner_cookie = cookie(state, owner.id).await;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32,
            32,
            image::Rgb([160, 90, 220]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
        state.media.put("poster.png", png.clone()).await.unwrap();
        std::fs::write(format!("{dir}/browser-upload.png"), png).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = plamenu::build_router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let configuration = format!("{dir}/browser-fixture.json");
        std::fs::write(
            &configuration,
            serde_json::to_vec(
                &json!({"port":port,"cookie":owner_cookie,"channel":channel.to_string(),"status":stored.to_string()}),
            )
            .unwrap(),
        )
        .unwrap();
        let output = tokio::process::Command::new(driver)
            .arg(configuration)
            .output()
            .await
            .unwrap();
        server.abort();
        assert!(
            output.status.success(),
            "browser proof: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
