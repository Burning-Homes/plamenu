mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState, entities,
    error::ApiError,
    webmentions::{self, outgoing, worker},
};
use plamenu_db::{
    PgPool,
    account::Account,
    status::{self, NewLocalStatus, Status},
};
use plamenu_federation::MentionPage;
use std::sync::Arc;
use tower::ServiceExt;
const SOURCE: &str = "https://website.example/song-review";
async fn fixture(pool: PgPool) -> (AppState, Arc<StubFederation>, Account, Status, String) {
    let owner = create_local_account(&pool, "alice", "Alice").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool, stub.clone());
    let item = status::create_local(
        &state.pool,
        NewLocalStatus::new(owner.id, "<p>A song</p>", "public", None),
    )
    .await
    .unwrap();
    let target = entities::status_web_url(&state.config.domain, &item, &owner.username);
    (state, stub, owner, item, target)
}
fn page(target: &str, text: &str) -> MentionPage {
    MentionPage {
        final_url: SOURCE.into(),
        content_type: "text/html; charset=utf-8".into(),
        links: Vec::new(),
        body: format!(
            "<article class=h-entry><h1 class=p-name>Review</h1><span class='p-author h-card'><span class=p-name>Bob</span></span><div class=e-content>{text}</div><a class=u-in-reply-to href='{target}'>Song</a></article>"
        ),
    }
}
async fn notification_count(state: &AppState, owner: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id=$1 AND kind='webmention'",
    )
    .bind(owner)
    .fetch_one(&state.pool)
    .await
    .unwrap()
}
#[sqlx::test(migrations = "../db/migrations")]
async fn receiver_is_durable_idempotent_updates_one_card_and_retracts_removed_sources(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    stub.mention_pages.lock().unwrap().insert(
        SOURCE.into(),
        page(&target, "Hello &amp; thanks<script>alert(1)</script>"),
    );
    let router = plamenu::build_router(state.clone());
    let form =
        serde_urlencoded::to_string([("source", SOURCE), ("target", target.as_str())]).unwrap();
    let response = router
        .oneshot(
            Request::post("/webmentions")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(notification_count(&state, owner.id).await, 0);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let id = body["id"].as_str().unwrap().parse().unwrap();
    worker::run(&state, id).await.unwrap();
    let rows = webmentions::list(&state, Some(owner.id), None, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entry.as_ref().unwrap().summary, "Hello & thanks");
    assert_eq!(notification_count(&state, owner.id).await, 1);
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Edited"));
    assert_eq!(
        webmentions::receive(&state, SOURCE, &target).await.unwrap(),
        id
    );
    worker::run(&state, id).await.unwrap();
    assert_eq!(notification_count(&state, owner.id).await, 1);
    assert_eq!(
        webmentions::list(&state, None, Some(&target), None)
            .await
            .unwrap()[0]
            .entry
            .as_ref()
            .unwrap()
            .summary,
        "Edited"
    );
    stub.mention_pages
        .lock()
        .unwrap()
        .get_mut(SOURCE)
        .unwrap()
        .body = "<p>The link was removed</p>".into();
    webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    assert!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(notification_count(&state, owner.id).await, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn transient_verification_keeps_old_card_and_private_targets_and_capabilities_are_rejected(
    pool: PgPool,
) {
    let (state, stub, owner, item, target) = fixture(pool).await;
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Original"));
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    stub.mention_failures
        .lock()
        .unwrap()
        .insert(SOURCE.into(), 503);
    webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    assert_eq!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()
            .len(),
        1
    );
    let queued: String = sqlx::query_scalar("SELECT state FROM webmentions WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(queued, "queued");
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(item.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        webmentions::receive(&state, SOURCE, &target).await,
        Err(ApiError::NotFound)
    ));
    assert!(
        webmentions::receive(&state, SOURCE, &format!("{target}?token=secret"))
            .await
            .is_err()
    );
    assert!(
        webmentions::receive(&state, "https://user:pass@website.example/post", &target)
            .await
            .is_err()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn redelivery_during_fetch_fences_old_content_and_404_retracts_without_duplicate_notifications(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Old"));
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *stub.mention_gate.lock().unwrap() = Some(gate.clone());
    let work = tokio::spawn({
        let state = state.clone();
        async move { worker::run(&state, id).await }
    });
    stub.mention_started.notified().await;
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "New"));
    webmentions::receive(&state, SOURCE, &target).await.unwrap();
    gate.add_permits(1);
    work.await.unwrap().unwrap();
    assert_eq!(notification_count(&state, owner.id).await, 0);
    *stub.mention_gate.lock().unwrap() = None;
    worker::run(&state, id).await.unwrap();
    assert_eq!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()[0]
            .entry
            .as_ref()
            .unwrap()
            .summary,
        "New"
    );
    stub.mention_failures
        .lock()
        .unwrap()
        .insert(SOURCE.into(), 404);
    webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    assert_eq!(notification_count(&state, owner.id).await, 0);
}
#[sqlx::test(migrations = "../db/migrations")]
async fn outgoing_discovers_endpoint_retains_removed_targets_and_public_source_obeys_current_privacy(
    pool: PgPool,
) {
    let (state, stub, _, item, _) = fixture(pool).await;
    let target = "https://website.example/track";
    stub.mention_pages.lock().unwrap().insert(
        target.into(),
        MentionPage {
            final_url: target.into(),
            content_type: "text/html".into(),
            body: "<link rel=webmention href='/incoming'>".into(),
            links: vec![],
        },
    );
    sqlx::query("UPDATE statuses SET content=$2 WHERE id=$1")
        .bind(item.id)
        .bind(format!("<p><a href='{target}'>Hello</a></p>"))
        .execute(&state.pool)
        .await
        .unwrap();
    outgoing::pass(&state).await.unwrap();
    let sent = stub.mention_posts.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "https://website.example/incoming");
    assert_eq!(sent[0].2, target);
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::get(format!("/webmentions/posts/{}", item.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["link"]
            .to_str()
            .unwrap()
            .contains("webmention")
    );
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
    assert!(html.contains("h-entry"));
    assert!(html.contains(target));
    sqlx::query("UPDATE statuses SET content='',visibility='private' WHERE id=$1")
        .bind(item.id)
        .execute(&state.pool)
        .await
        .unwrap();
    outgoing::pass(&state).await.unwrap();
    assert_eq!(stub.mention_posts.lock().unwrap().len(), 2);
    assert!(matches!(
        outgoing::public_status(&state, item.id).await,
        Err(ApiError::NotFound)
    ));
    sqlx::query("DELETE FROM statuses WHERE id=$1")
        .bind(item.id)
        .execute(&state.pool)
        .await
        .unwrap();
    outgoing::pass(&state).await.unwrap();
    assert_eq!(stub.mention_posts.lock().unwrap().len(), 3);
    assert!(matches!(
        outgoing::public_status(&state, item.id).await,
        Err(ApiError::Gone)
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_website_reactions_are_native_public_notes_with_atomic_edges_and_microformats(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Review"));
    let mention = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, mention).await.unwrap();
    for (kind, marker) in [
        ("reply", "u-in-reply-to"),
        ("like", "u-like-of"),
        ("repost", "u-repost-of"),
        ("quote", "u-quotation-of"),
    ] {
        let item = webmentions::interactions::publish(&state, &owner, mention, kind, "My response")
            .await
            .unwrap();
        assert_eq!(item.account_id, owner.id);
        assert_eq!(item.visibility, "public");
        assert!(item.uri.is_none());
        assert!(item.in_reply_to_id.is_none());
        assert_eq!(
            webmentions::interactions::get(&state, item.id)
                .await
                .unwrap(),
            Some((SOURCE.into(), kind.into()))
        );
        let response = plamenu::build_router(state.clone())
            .oneshot(
                Request::get(format!("/webmentions/posts/{}", item.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
        assert!(html.contains(marker));
        let parsed = webmentions::parse::entry(
            &html,
            &url::Url::parse(&format!(
                "https://plamenu.test/webmentions/posts/{}",
                item.id
            ))
            .unwrap(),
            SOURCE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.kind, kind);
    }
    let fake: i64 =
        sqlx::query_scalar("SELECT count(*) FROM accounts WHERE domain='website.example'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(fake, 0);
    assert!(
        webmentions::interactions::publish(&state, &owner, mention, "delete", "invalid")
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn photo_proxy_decodes_raster_hides_origin_and_obeys_current_target_and_source_policy(
    pool: PgPool,
) {
    let (state, stub, owner, item, target) = fixture(pool).await;
    let photo = "https://website.example/avatar.png";
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8, 8)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    stub.media.lock().unwrap().insert(
        photo.into(),
        plamenu_federation::FetchedMedia {
            bytes: png.into_inner(),
            content_type: "image/png".into(),
            final_url: photo.into(),
        },
    );
    let mut html = page(&target, "Review");
    html.body = html.body.replace(
        "<span class=p-name>Bob</span>",
        &format!("<span class=p-name>Bob</span><img class=u-photo src='{photo}'>"),
    );
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), html);
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    let row = webmentions::list(&state, Some(owner.id), None, None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        row.entity()["entry"]["photo"],
        format!("/webmentions/{id}/photo")
    );
    let (bytes, content_type) = webmentions::photo::get(&state, id).await.unwrap();
    assert_eq!(content_type, "image/jpeg");
    assert!(bytes.starts_with(&[0xff, 0xd8]));
    assert!(bytes.len() < 131_072);
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(item.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(matches!(
        webmentions::photo::get(&state, id).await,
        Err(ApiError::NotFound)
    ));
}
#[sqlx::test(migrations = "../db/migrations")]
async fn archive_paging_advances_past_many_currently_hidden_targets_without_starving_older_cards(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Older readable card"));
    let readable = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, readable).await.unwrap();
    let hidden = status::create_local(
        &state.pool,
        NewLocalStatus::new(owner.id, "<p>Will become private</p>", "public", None),
    )
    .await
    .unwrap();
    let hidden_target = entities::status_web_url(&state.config.domain, &hidden, &owner.username);
    for i in 0..55 {
        let source = format!("https://website.example/hidden/{i}");
        stub.mention_pages
            .lock()
            .unwrap()
            .insert(source.clone(), page(&hidden_target, "Hidden"));
        let id = webmentions::receive(&state, &source, &hidden_target)
            .await
            .unwrap();
        worker::run(&state, id).await.unwrap();
    }
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(hidden.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let page = webmentions::page(&state, Some(owner.id), None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, readable);
    assert!(page.before.is_none());
}
#[sqlx::test(migrations = "../db/migrations")]
async fn canonical_library_mentions_appear_on_native_collection_pages_and_disappear_on_privacy_change(
    pool: PgPool,
) {
    let (state, stub, owner, _, _) = fixture(pool).await;
    let collection = plamenu::music_collections::create(
        &state,
        owner.id,
        &plamenu::music_collections::New {
            name: "Library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let canonical = plamenu::music_library_federation::uri(&state.config.domain, collection.id);
    let native = format!(
        "https://{}/music/collections/{}",
        state.config.domain, collection.id
    );
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&canonical, "Library review"));
    let id = webmentions::receive(&state, SOURCE, &canonical)
        .await
        .unwrap();
    worker::run(&state, id).await.unwrap();
    assert_eq!(
        webmentions::list(&state, None, Some(&native), None)
            .await
            .unwrap()
            .len(),
        1
    );
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::get(format!("/music/collections/{}", collection.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
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
    assert!(html.contains("webmention-card"));
    assert!(html.contains("Library review"));
    sqlx::query("UPDATE music_collections SET visibility='private' WHERE id=$1")
        .bind(collection.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn self_mentions_stay_archived_while_a_different_local_website_author_notifies_the_target_owner(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    let owner_url = entities::account_web_url(&state.config.domain, &owner);
    let mut own = page(&target, "My own page");
    own.body = own.body.replace(
        "<span class=p-name>Bob</span>",
        &format!("<a class='p-name u-url' href='{owner_url}'>Alice</a>"),
    );
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), own);
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    assert_eq!(notification_count(&state, owner.id).await, 0);
    assert_eq!(
        webmentions::list(&state, Some(owner.id), None, None)
            .await
            .unwrap()
            .len(),
        1
    );
    let bob = create_local_account(&state.pool, "bob", "Bob").await;
    let bob_url = entities::account_web_url(&state.config.domain, &bob);
    let source = "https://plamenu.test/webmentions/posts/123";
    let mut other = page(&target, "Bob mentioned Alice");
    other.final_url = source.into();
    other.body = other.body.replace(
        "<span class=p-name>Bob</span>",
        &format!("<a class='p-name u-url' href='{bob_url}'>Bob</a>"),
    );
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(source.into(), other);
    let id = webmentions::receive(&state, source, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    assert_eq!(notification_count(&state, owner.id).await, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_document_excludes_incoming_card_links_from_its_authored_microformat_entry(
    pool: PgPool,
) {
    let (state, stub, _, item, _) = fixture(pool).await;
    let target = format!("https://plamenu.test/webmentions/posts/{}", item.id);
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Incoming review"));
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::get(format!("/webmentions/posts/{}", item.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
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
    assert!(html.contains("Incoming review"));
    assert!(
        webmentions::parse::entry(&html, &url::Url::parse(&target).unwrap(), SOURCE)
            .unwrap()
            .is_none()
    );
}
#[sqlx::test(migrations = "../db/migrations")]
async fn native_archive_forms_use_current_csrf_and_publish_only_the_explicit_owner_action(
    pool: PgPool,
) {
    let (state, stub, owner, _, target) = fixture(pool).await;
    plamenu_db::user::create(
        &state.pool,
        owner.id,
        Some("alice@example.test"),
        &plamenu::auth::hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    stub.mention_pages
        .lock()
        .unwrap()
        .insert(SOURCE.into(), page(&target, "Original review"));
    let id = webmentions::receive(&state, SOURCE, &target).await.unwrap();
    worker::run(&state, id).await.unwrap();
    let app = plamenu::build_router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=alice%40example.test&password=fixture-password",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
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
    let response = app
        .clone()
        .oneshot(
            Request::get("/webmentions/archive")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
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
    assert!(html.contains("Publish public like"));
    assert!(html.contains(&csrf));
    let path = format!("/web/webmentions/{id}/interact");
    let response = app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("csrf=wrong&kind=like"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM webmention_interactions")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let form = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("kind", "like"),
        ("text", "A good review"),
    ])
    .unwrap();
    let response = app
        .oneshot(
            Request::post(path)
                .header("cookie", cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let edges:Vec<(i64,String)>=sqlx::query_as("SELECT s.account_id,i.kind FROM webmention_interactions i JOIN statuses s ON s.id=i.status_id WHERE s.visibility='public'").fetch_all(&state.pool).await.unwrap();
    assert_eq!(edges, vec![(owner.id, "like".into())]);
}
