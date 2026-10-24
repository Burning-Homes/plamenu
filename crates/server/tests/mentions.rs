mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::{generate_secret, hash_password, hash_secret},
    mentions,
};
use plamenu_db::{
    PgPool, notification, oauth,
    status::{self, NewLocalStatus, Status},
    user,
};
use serde_json::Value;
use tower::ServiceExt;
async fn post(
    state: &AppState,
    author: i64,
    recipient: i64,
    text: &str,
    visibility: &str,
) -> Status {
    let post = status::create_local(
        &state.pool,
        NewLocalStatus::new(author, text, visibility, None),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO status_mentions(status_id,account_id) VALUES($1,$2)")
        .bind(post.id)
        .bind(recipient)
        .execute(&state.pool)
        .await
        .unwrap();
    post
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn permanent_mentions_survive_notification_clear_but_reflect_private_audiences_edits_and_deletes(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let author = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let public = post(&state, author, owner, "<p>Public mention</p>", "public").await;
    let private = post(&state, author, owner, "<p>Private mention</p>", "direct").await;
    for post in [&public, &private] {
        notification::create(&state.pool, owner, author, "mention", Some(post.id))
            .await
            .unwrap();
    }
    notification::clear(&state.pool, owner).await.unwrap();
    let page = mentions::page(&state, owner, &mentions::Query::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0]["source"], "local");
    let page = mentions::page(
        &state,
        owner,
        &mentions::Query {
            private: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0]["status"]["id"], private.id.to_string());
    sqlx::query("UPDATE statuses SET content='<p>Edited mention</p>' WHERE id=$1")
        .bind(private.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let page = mentions::page(&state, owner, &mentions::Query::default())
        .await
        .unwrap();
    assert_eq!(page.items[0]["status"]["content"], "<p>Edited mention</p>");
    sqlx::query("DELETE FROM status_mentions WHERE status_id=$1")
        .bind(public.id)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(private.id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        mentions::page(&state, owner, &mentions::Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        mentions::page(&state, author, &mentions::Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn archive_paging_skips_hidden_senders_and_deduplicates_current_edges_and_sources(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let good = create_local_account(&pool, "carol", "Carol").await.id;
    let hidden = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let oldest = post(
        &state,
        good,
        owner,
        "<p>Accessible old mention</p>",
        "public",
    )
    .await;
    for _ in 0..55 {
        post(&state, hidden, owner, "<p>Hidden mention</p>", "public").await;
    }
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(hidden)
        .execute(&state.pool)
        .await
        .unwrap();
    let page = mentions::page(&state, owner, &mentions::Query::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0]["id"], oldest.id.to_string());
    assert!(page.before.is_none());
    sqlx::query("UPDATE accounts SET domain='remote.example',uri='https://remote.example/users/carol' WHERE id=$1").bind(good).execute(&state.pool).await.unwrap();
    assert!(
        mentions::page(
            &state,
            owner,
            &mentions::Query {
                source: Some("local".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    assert_eq!(
        mentions::page(
            &state,
            owner,
            &mentions::Query {
                source: Some("activitypub".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .len(),
        1
    );
    assert!(
        mentions::page(
            &state,
            owner,
            &mentions::Query {
                source: Some("unknown".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_and_api_archives_require_the_current_recipient_and_show_private_cards(
    pool: PgPool,
) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let author = create_local_account(&pool, "bob", "Bob").await.id;
    let uid = user::create(
        &pool,
        owner,
        Some("alice@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap()
    .id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    post(
        &state,
        author,
        owner,
        "<p>Private archive card</p>",
        "direct",
    )
    .await;
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Archive tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&token),
        app.id,
        Some(uid),
        "read:notifications",
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let r = app
        .clone()
        .oneshot(
            Request::get("/api/v1/mentions/archive?private=true")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "private, no-store");
    let body: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let r = app
        .clone()
        .oneshot(
            Request::get("/api/v1/mentions/archive")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let login = app
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
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let r = app
        .oneshot(
            Request::get("/mentions?private=true")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let text =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(text.contains("Private archive card"));
    assert!(text.contains("Only non-public mentions"));
    assert!(text.contains("/notifications/inbox"));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn website_and_activity_mentions_share_the_archive_without_creating_an_actor(pool: PgPool) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let author = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    post(&state, author, owner, "<p>Native mention</p>", "public").await;
    let target = status::create_local(
        &state.pool,
        NewLocalStatus::new(owner, "<p>Website target</p>", "public", None),
    )
    .await
    .unwrap();
    let target = format!("https://{}/@alice/{}", state.config.domain, target.id);
    let source = "https://website.example/mention";
    let entry=plamenu::webmentions::parse::entry(&format!("<article class=h-entry><p class=p-summary>Website response</p><a class=u-in-reply-to href='{target}'>Reply</a></article>"),&url::Url::parse(source).unwrap(),&target).unwrap().unwrap();
    let id = plamenu_db::id::next();
    sqlx::query("INSERT INTO webmentions(id,source,target,owner_id,entry,active,state) VALUES($1,$2,$3,$4,$5,true,'done')").bind(id).bind(source).bind(target).bind(owner).bind(sqlx::types::Json(entry)).execute(&state.pool).await.unwrap();
    notification::clear(&state.pool, owner).await.unwrap();
    let page = mentions::page(&state, owner, &mentions::Query::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0]["source"], "webmention");
    assert!(page.websites.contains_key(&id));
    assert!(
        mentions::page(
            &state,
            author,
            &mentions::Query {
                source: Some("webmention".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    assert!(
        mentions::page(
            &state,
            owner,
            &mentions::Query {
                source: Some("webmention".into()),
                private: Some(true),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty()
    );
    sqlx::query("UPDATE webmentions SET active=false WHERE id=$1")
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        mentions::page(&state, owner, &mentions::Query::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 2);
}
