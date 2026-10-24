//! Channel profiles use verified publications, independently of Announce delivery.
mod common;

use axum::{body::Body, http::Request};
use common::{RemoteUser, StubFederation, create_local_account};
use http_body_util::BodyExt;
use plamenu_db::{PgPool, account, status, user::TimelineOrder};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn library(
    pool: &PgPool,
) -> (
    plamenu::AppState,
    account::Account,
    account::Account,
    Vec<status::Status>,
) {
    let uploader = RemoteUser::new("remote.test", "uploader");
    let mut channel = RemoteUser::channel("remote.test", "films");
    channel.actor.attributed_to = Some(json!([uploader.actor.id]));
    let stub = StubFederation::with_users(&[&uploader, &channel]);
    let state = common::test_state_with(pool.clone(), stub.clone());
    let mut films = Vec::new();
    for n in 0..7 {
        let uri = format!("https://remote.test/videos/{n}");
        // Fetch order deliberately differs from origin publish order.
        let day = 1 + (n * 3) % 7;
        stub.objects.lock().unwrap().insert(uri.clone(), json!({
            "id":uri, "type":"Video", "name":format!("Film {n}"),
            "attributedTo":[{"type":"Person","id":uploader.actor.id},{"type":"Group","id":channel.actor.id}],
            "to":[plamenu_ap::activity::PUBLIC], "published":format!("2026-10-{day:02}T00:00:00Z"),
            "url":[{"type":"Link","mediaType":"video/mp4","href":format!("https://remote.test/media/{n}.mp4")}]
        }));
        films.push(
            plamenu::ingest::resolve_or_fetch_status(&state, &uri)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let author = account::find_by_uri(pool, &uploader.actor.id)
        .await
        .unwrap()
        .unwrap();
    let channel = account::find_by_uri(pool, &channel.actor.id)
        .await
        .unwrap()
        .unwrap();
    (state, author, channel, films)
}

async fn page(state: &plamenu::AppState, path: &str) -> String {
    let response = plamenu::build_router(state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{path}");
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

async fn feed(
    pool: &PgPool,
    channel: i64,
    viewer: Option<i64>,
    order: TimelineOrder,
    filter: &status::AccountStatusesFilter,
    limit: i64,
) -> Vec<i64> {
    status::by_account(pool, channel, viewer, filter, order, limit)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_channel_profile_and_api_include_all_publications_once(pool: PgPool) {
    let (state, author, channel, films) = library(&pool).await;
    let wrapper = status::upsert_remote_reblog(
        &pool,
        "https://remote.test/announces/film",
        channel.id,
        films[3].id,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let author_json: Value = serde_json::from_str(
        &page(&state, &format!("/api/v1/accounts/{}/statuses", author.id)).await,
    )
    .unwrap();
    assert_eq!(author_json.as_array().unwrap().len(), 7);
    for suffix in [
        "",
        "?only_media=true&exclude_reblogs=true",
        "?sort=hot",
        "?sort=top",
    ] {
        let entities: Value = serde_json::from_str(
            &page(
                &state,
                &format!("/api/v1/accounts/{}/statuses{suffix}", channel.id),
            )
            .await,
        )
        .unwrap();
        assert_eq!(entities.as_array().unwrap().len(), 7, "{suffix}");
        for entity in entities.as_array().unwrap() {
            assert_eq!(entity["account"]["id"], author.id.to_string());
            assert_eq!(entity["reblog"], Value::Null);
            assert_eq!(entity["video_channels"][0]["id"], channel.id.to_string());
            assert_ne!(entity["id"], wrapper.id.to_string());
        }
    }
    for suffix in ["", "/media"] {
        let html = page(&state, &format!("/!films@remote.test{suffix}")).await;
        for film in &films {
            let marker = if suffix.is_empty() {
                format!("id=\"post-{}\"", film.id)
            } else {
                format!(
                    "href=\"/@uploader@remote.test/{}#post-{}\"",
                    film.id, film.id
                )
            };
            assert_eq!(html.matches(&marker).count(), 1, "{suffix}: {}", film.id);
        }
        assert!(!html.contains(&format!("id=\"post-{}\"", wrapper.id)));
    }
    // Publications also survive client-side pagination in both supported orders.
    let filter = status::AccountStatusesFilter::default();
    for order in [TimelineOrder::Published, TimelineOrder::Received] {
        let all = feed(&pool, channel.id, None, order, &filter, 50).await;
        let author_all = feed(&pool, author.id, None, order, &filter, 50).await;
        assert_eq!(all, author_all);
        let first = feed(&pool, channel.id, None, order, &filter, 2).await;
        let older = status::AccountStatusesFilter {
            max_id: first.last().copied(),
            ..Default::default()
        };
        let rest = feed(&pool, channel.id, None, order, &older, 50).await;
        assert_eq!([first, rest].concat(), all);
        let newer = status::AccountStatusesFilter {
            since_id: Some(all[2]),
            ..Default::default()
        };
        assert_eq!(
            feed(&pool, channel.id, None, order, &newer, 50).await,
            all[..2]
        );
    }
    let tag = plamenu_db::tag::ensure(&pool, "films").await.unwrap();
    plamenu_db::tag::attach(&pool, films[0].id, tag)
        .await
        .unwrap();
    let tagged = status::AccountStatusesFilter {
        tagged: Some("FILMS".into()),
        ..Default::default()
    };
    assert_eq!(
        feed(
            &pool,
            channel.id,
            None,
            TimelineOrder::Published,
            &tagged,
            50
        )
        .await,
        [films[0].id]
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn linked_publications_obey_author_visibility_and_history_boundaries(pool: PgPool) {
    let (_, author, channel, films) = library(&pool).await;
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let filter = status::AccountStatusesFilter::default();
    // The channel link cannot make private/direct/deleted content public.
    for (film, visibility) in films.iter().zip(["private", "direct", "unlisted"]) {
        sqlx::query("UPDATE statuses SET visibility=$2 WHERE id=$1")
            .bind(film.id)
            .bind(visibility)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(films[3].id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET ingest_provenance='history' WHERE id=$1")
        .bind(films[4].id)
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::remote_history::set_actor_metadata(
        &pool,
        author.id,
        Some("https://remote.test/users/uploader/outbox"),
        Some(true),
    )
    .await
    .unwrap();
    for order in [TimelineOrder::Published, TimelineOrder::Received] {
        let anonymous = feed(&pool, channel.id, None, order, &filter, 50).await;
        assert_eq!(anonymous.len(), 3);
        assert!(anonymous.contains(&films[2].id));
        let signed_in = feed(&pool, channel.id, Some(viewer.id), order, &filter, 50).await;
        assert_eq!(signed_in.len(), 4);
        assert!(signed_in.contains(&films[4].id));
        // Reading a profile must not promote cold history into global discovery.
        assert!(
            !plamenu_db::video::discover_channel(
                &pool,
                channel.id,
                None,
                Some(viewer.id),
                None,
                50
            )
            .await
            .unwrap()
            .iter()
            .any(|hit| hit.status_id == films[4].id)
        );
    }
    plamenu_db::block::create(&pool, viewer.id, author.id, None)
        .await
        .unwrap();
    assert!(
        feed(
            &pool,
            channel.id,
            Some(viewer.id),
            TimelineOrder::Published,
            &filter,
            50
        )
        .await
        .is_empty()
    );
    plamenu_db::block::delete(&pool, viewer.id, author.id)
        .await
        .unwrap();
    plamenu_db::block::create(&pool, viewer.id, channel.id, None)
        .await
        .unwrap();
    assert!(
        feed(
            &pool,
            channel.id,
            Some(viewer.id),
            TimelineOrder::Received,
            &filter,
            50
        )
        .await
        .is_empty()
    );
    account::suspend(&pool, author.id, "local").await.unwrap();
    assert!(
        feed(
            &pool,
            channel.id,
            None,
            TimelineOrder::Published,
            &filter,
            50
        )
        .await
        .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_channel_profile_keeps_uploader_authorship_and_owner_moderation(pool: PgPool) {
    let owner = create_local_account(&pool, "owner", "Owner").await;
    let editor = create_local_account(&pool, "editor", "Editor").await;
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let state = common::test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    let film = status::create_local(
        &pool,
        status::NewLocalStatus {
            object_type: Some("Video"),
            ..status::NewLocalStatus::new(editor.id, "Film", "public", None)
        },
    )
    .await
    .unwrap();
    let mid = plamenu_db::id::next();
    sqlx::query("INSERT INTO media_attachments(id,account_id,status_id,file_name,content_type,kind) VALUES($1,$2,$3,'film.mp4','video/mp4','video')").bind(mid).bind(editor.id).bind(film.id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO video_publications(status_id,channel_id,media_id) VALUES($1,$2,$3)")
        .bind(film.id)
        .bind(channel.id)
        .bind(mid)
        .execute(&pool)
        .await
        .unwrap();
    let wrapper = status::create_local_reblog(&pool, channel.id, film.id)
        .await
        .unwrap();
    let filter = status::AccountStatusesFilter::default();
    assert_eq!(
        feed(
            &pool,
            channel.id,
            None,
            TimelineOrder::Published,
            &filter,
            50
        )
        .await,
        [film.id]
    );
    assert_ne!(film.id, wrapper.id);
    let entity: Value = serde_json::from_str(
        &page(&state, &format!("/api/v1/accounts/{}/statuses", channel.id)).await,
    )
    .unwrap();
    assert_eq!(entity[0]["account"]["id"], editor.id.to_string());
    plamenu_db::block::create(&pool, viewer.id, owner.id, None)
        .await
        .unwrap();
    assert!(
        feed(
            &pool,
            channel.id,
            Some(viewer.id),
            TimelineOrder::Published,
            &filter,
            50
        )
        .await
        .is_empty()
    );
    account::suspend(&pool, owner.id, "local").await.unwrap();
    assert!(
        feed(
            &pool,
            channel.id,
            None,
            TimelineOrder::Received,
            &filter,
            50
        )
        .await
        .is_empty()
    );
}
