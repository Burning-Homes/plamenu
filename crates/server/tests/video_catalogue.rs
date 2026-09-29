//! Bounded catalogue paging, current channel ownership and anonymous policy.
//! Media rows are authorization fixtures, not claims of codec verification.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu_db::{PgPool, id, video};
use tower::ServiceExt;

async fn films(pool: &PgPool, owner: i64, channel: i64, count: usize, title: &str) -> Vec<i64> {
    let statuses: Vec<i64> = (0..count).map(|_| id::next()).collect();
    let media: Vec<i64> = (0..count).map(|_| id::next()).collect();
    sqlx::query("INSERT INTO statuses(id,account_id,object_type,title,content) SELECT sid,$2,'Video',$3,'Public fixture' FROM unnest($1::bigint[]) AS sid")
        .bind(&statuses).bind(owner).bind(title).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO media_attachments(id,account_id,status_id,file_name,content_type,kind) SELECT mid,$3,sid,'fixture.mp4','video/mp4','video' FROM unnest($1::bigint[],$2::bigint[]) AS m(sid,mid)")
        .bind(&statuses).bind(&media).bind(owner).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO video_publications(status_id,channel_id,media_id) SELECT sid,$3,mid FROM unnest($1::bigint[],$2::bigint[]) AS m(sid,mid)")
        .bind(&statuses).bind(&media).bind(channel).execute(pool).await.unwrap();
    statuses
}
async fn cookie(state: &plamenu::AppState, account: i64) -> String {
    let user = plamenu_db::user::set_credentials(
        &state.pool,
        account,
        Some("viewer@example.test"),
        "unused",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(state).await.unwrap();
    let raw = format!("catalogue-{}", id::next());
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
async fn page(state: &plamenu::AppState, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        request = request.header("Cookie", format!("__Host-plamenu_session={cookie}"));
    }
    let response = plamenu::build_router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[sqlx::test(migrations = "../db/migrations")]
async fn filtered_five_thousand_match_pages_retain_private_safe_older_navigation(pool: PgPool) {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let spammer = common::create_local_account(&pool, "spammer", "Spammer").await;
    let viewer = common::create_local_account(&pool, "viewer", "Viewer").await;
    common::open_previews(&pool).await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let hidden_channel =
        plamenu::video::create_channel(&state, &spammer, "hiddenfilms", "Hidden films")
            .await
            .unwrap();
    let visible = films(&pool, owner.id, channel.id, 50, "Visible needle").await;
    films(&pool, spammer.id, hidden_channel.id, 5500, "Hidden needle").await;
    plamenu_db::block::create(&pool, viewer.id, spammer.id, None)
        .await
        .unwrap();
    let raw = cookie(&state, viewer.id).await;
    let started = std::time::Instant::now();
    let first = video::discovery_page(
        &pool,
        None,
        Some("needle"),
        Some(viewer.id),
        false,
        None,
        21,
    )
    .await
    .unwrap();
    assert!(first.hits.is_empty());
    let scan = first
        .scan_before
        .expect("bounded empty page must still advance");
    let (_, html) = page(&state, "/videos/explore?q=needle", Some(&raw)).await;
    assert!(!html.contains("Hidden needle") && !html.contains("Hidden films"));
    assert!(html.contains("No visible videos on this page"));
    assert!(
        html.contains(&format!("max_id={scan}")),
        "Older link must not disappear on an empty page"
    );
    let second = video::discovery_page(
        &pool,
        None,
        Some("needle"),
        Some(viewer.id),
        false,
        Some(scan),
        21,
    )
    .await
    .unwrap();
    assert_eq!(second.hits.len(), 21);
    assert!(
        second
            .hits
            .iter()
            .all(|hit| visible.contains(&hit.status_id))
    );
    let (_, html) = page(
        &state,
        &format!("/videos/explore?q=needle&max_id={scan}"),
        Some(&raw),
    )
    .await;
    assert_eq!(html.matches("class=\"video-explore__card\"").count(), 20);
    assert!(
        !html.contains("<video"),
        "catalogue must not allocate eager players"
    );
    assert!(!html.contains("Hidden needle"));
    let cursor = second.hits[19].status_id;
    let third = video::discovery_page(
        &pool,
        None,
        Some("needle"),
        Some(viewer.id),
        false,
        Some(cursor),
        21,
    )
    .await
    .unwrap();
    assert!(third.hits.iter().all(|hit| hit.status_id < cursor));
    eprintln!(
        "catalogue 5,550-match filtered paging incl. two HTML renders: {:?}",
        started.elapsed()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn transferred_channel_current_owner_is_authoritative_in_every_catalogue_view(pool: PgPool) {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let recipient = common::create_local_account(&pool, "bob", "Bob").await;
    let viewer = common::create_local_account(&pool, "viewer", "Viewer").await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    films(&pool, owner.id, channel.id, 1, "Stable author film").await;
    plamenu_db::follow::create(&pool, viewer.id, channel.id, None)
        .await
        .unwrap();
    sqlx::query("UPDATE video_channels SET owner_account_id=$2 WHERE account_id=$1")
        .bind(channel.id)
        .bind(recipient.id)
        .execute(&pool)
        .await
        .unwrap();
    for followed in [false, true] {
        assert_eq!(
            video::discover(&pool, None, Some(viewer.id), followed, None, 21)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    plamenu_db::block::create(&pool, viewer.id, recipient.id, None)
        .await
        .unwrap();
    for followed in [false, true] {
        assert!(
            video::discover(&pool, Some("Stable"), Some(viewer.id), followed, None, 21)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        !video::library_visible(&pool, channel.id, Some(viewer.id))
            .await
            .unwrap()
    );
    assert!(
        video::discover_channel(&pool, channel.id, None, Some(viewer.id), None, 21)
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(recipient.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        video::discover(&pool, None, None, false, None, 21)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        video::discover_channel(&pool, channel.id, None, None, None, 21)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_search_and_subscriptions_use_independent_instance_gates(pool: PgPool) {
    let actor = common::create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &actor, "films", "Films")
        .await
        .unwrap();
    films(&pool, actor.id, channel.id, 1, "Public film").await;
    for (preview, search) in [(false, false), (true, false), (false, true), (true, true)] {
        let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
        plamenu_db::instance_settings::save(
            &pool,
            plamenu_db::instance_settings::SettingsUpdate {
                timeline_preview_federated: preview,
                public_search: search,
                ..settings.as_update()
            },
        )
        .await
        .unwrap();
        let state = common::test_state_with(pool.clone(), common::StubFederation::with_actors([]));
        for (path, allowed) in [
            ("/videos/explore", preview),
            ("/videos/explore?q=Public", search),
            ("/videos/explore?following=1", false),
        ] {
            let (status, body) = page(&state, path, None).await;
            assert_eq!(
                status,
                if allowed {
                    StatusCode::OK
                } else {
                    StatusCode::SEE_OTHER
                }
            );
            if !allowed {
                assert!(!body.contains("Public film"));
            }
        }
    }
}
