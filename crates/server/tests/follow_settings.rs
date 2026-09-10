//! Per-follow relationship settings (M32): `reblogs` / `notify` /
//! `languages` on `POST /api/v1/accounts/{id}/follow`, the real
//! `showing_reblogs`/`notifying`/`languages` Relationship fields, the home
//! timeline's hide-boosts and language filters, and the `status`
//! notification for notify subscribers.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "follow-settings-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write follow",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write follow",
    )
    .await
    .unwrap();
    (account, token)
}

/// Generic JSON API call; returns (status, body).
async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// The same endpoint form-encoded, the way Mastodon clients send arrays
/// (`languages[]`).
async fn api_form(app: Router, uri: &str, bearer: &str, form_body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form_body.to_owned()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post_status_in(pool: &PgPool, username: &str, text: &str, language: &str) -> i64 {
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username,
            text,
            visibility: "public",
            language: Some(language),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    stored.id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_params_set_update_and_keep_settings(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());
    let follow_uri = format!("/api/v1/accounts/{}/follow", carol.id);

    // A fresh follow with options applies them and reports them back.
    let (status, rel) = api(
        app(),
        "POST",
        &follow_uri,
        Some(&alice_token),
        Some(json!({"reblogs": false, "notify": true, "languages": ["en"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["following"], true);
    assert_eq!(rel["showing_reblogs"], false);
    assert_eq!(rel["notifying"], true);
    assert_eq!(rel["languages"], json!(["en"]));
    // `replies` is our extension; absent, a person follow reports the
    // target-derived default.
    assert_eq!(rel["showing_replies"], true);

    // Re-following without params keeps the stored settings (Mastodon's
    // absent-option semantics), and `/relationships` agrees.
    let (_, rel) = api(app(), "POST", &follow_uri, Some(&alice_token), None).await;
    assert_eq!(rel["showing_reblogs"], false);
    assert_eq!(rel["notifying"], true);
    assert_eq!(rel["languages"], json!(["en"]));
    assert_eq!(rel["showing_replies"], true);

    // The extension param sets it, and a later absent one keeps it.
    let (_, rel) = api(
        app(),
        "POST",
        &follow_uri,
        Some(&alice_token),
        Some(json!({"replies": false})),
    )
    .await;
    assert_eq!(rel["showing_replies"], false);
    let (_, rel) = api(app(), "POST", &follow_uri, Some(&alice_token), None).await;
    assert_eq!(rel["showing_replies"], false);
    let (_, rel) = api(
        app(),
        "POST",
        &follow_uri,
        Some(&alice_token),
        Some(json!({"replies": true})),
    )
    .await;
    assert_eq!(rel["showing_replies"], true);
    let (_, rels) = api(
        app(),
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={}", carol.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(rels[0]["showing_reblogs"], false);
    assert_eq!(rels[0]["notifying"], true);
    assert_eq!(rels[0]["languages"], json!(["en"]));

    // An empty languages list clears the filter; other settings stay.
    let (_, rel) = api(
        app(),
        "POST",
        &follow_uri,
        Some(&alice_token),
        Some(json!({"languages": []})),
    )
    .await;
    assert_eq!(rel["languages"], Value::Null);
    assert_eq!(rel["showing_reblogs"], false);
    assert_eq!(rel["notifying"], true);

    // Form encoding: bare flags plus a repeated `languages[]` array.
    let (status, rel) = api_form(
        app(),
        &follow_uri,
        &alice_token,
        "reblogs=true&notify=false&languages[]=de&languages[]=fr",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rel:?}");
    assert_eq!(rel["showing_reblogs"], true);
    assert_eq!(rel["notifying"], false);
    assert_eq!(rel["languages"], json!(["de", "fr"]));

    // An unknown locale is Mastodon's validation 422.
    let (status, body) = api(
        app(),
        "POST",
        &follow_uri,
        Some(&alice_token),
        Some(json!({"languages": ["klingon"]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "Validation failed: Languages is invalid");

    // The invalid request changed nothing; the stored edge still carries
    // the form-submitted values.
    let edge = follow::find(&pool, alice.id, carol.id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge.show_reblogs);
    assert!(!edge.notify);
    assert_eq!(
        edge.languages.as_deref(),
        Some(["de".to_owned(), "fr".to_owned()].as_slice())
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hide_boosts_and_language_filter_apply_to_home(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let (dave, _) = user_with_token(&pool, "dave").await;
    let app = || test_app(pool.clone());

    // Alice follows carol (all settings default) and dave.
    for id in [carol.id, dave.id] {
        let (status, _) = api(
            app(),
            "POST",
            &format!("/api/v1/accounts/{id}/follow"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let english = post_status_in(&pool, "dave", "hello world", "en").await;
    let boost_source = post_status_in(&pool, "carol", "boost me", "en").await;
    let (status, boost) = api(
        app(),
        "POST",
        &format!("/api/v1/statuses/{boost_source}/reblog"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{boost:?}");
    // Dave boosts carol's post so alice sees a boost row by dave.
    let state = test_state_with(pool.clone(), std::sync::Arc::default());
    let dave_boost = actions::reblog_status(&state, &dave, boost_source)
        .await
        .unwrap();

    let home_ids = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    assert!(home_ids(&home).contains(&dave_boost.id.to_string()));

    // Hiding dave's boosts removes the boost but keeps his own posts —
    // and alice's own boost of the same status stays.
    let (_, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", dave.id),
        Some(&alice_token),
        Some(json!({"reblogs": false})),
    )
    .await;
    assert_eq!(rel["showing_reblogs"], false);
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    let ids = home_ids(&home);
    assert!(!ids.contains(&dave_boost.id.to_string()), "boost hidden");
    assert!(ids.contains(&english.to_string()), "own posts stay");
    assert!(
        ids.contains(&boost_source.to_string()),
        "carol's post stays"
    );

    // A language filter on the carol follow hides her off-language posts.
    let german = post_status_in(&pool, "carol", "hallo welt", "de").await;
    let (_, rel) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        Some(json!({"languages": ["en"]})),
    )
    .await;
    assert_eq!(rel["languages"], json!(["en"]));
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    let ids = home_ids(&home);
    assert!(!ids.contains(&german.to_string()), "german post filtered");
    assert!(
        ids.contains(&boost_source.to_string()),
        "english post stays"
    );

    // Carol's own home timeline is untouched by alice's settings.
    let (_, home) = api(
        app(),
        "GET",
        "/api/v1/timelines/home",
        Some(&carol_token),
        None,
    )
    .await;
    assert!(home_ids(&home).contains(&german.to_string()));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notify_follow_gets_status_notifications(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, _) = user_with_token(&pool, "carol").await;
    let (_dave, _) = user_with_token(&pool, "dave").await;
    let app = || test_app(pool.clone());
    let state = || test_state_with(pool.clone(), std::sync::Arc::default());

    let (status, _) = api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        Some(json!({"notify": true, "languages": ["en"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A plain post notifies; its notification carries the status.
    let post = post_status_in(&pool, "carol", "big news", "en").await;
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let items = notifications.as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["type"], "status");
    assert_eq!(items[0]["account"]["id"], carol.id.to_string());
    assert_eq!(items[0]["status"]["id"], post.to_string());

    // Off-language posts, replies to others, boosts and DMs stay silent;
    // a self-reply notifies (Mastodon's FeedInsertWorker#notify? rules).
    post_status_in(&pool, "carol", "hallo welt", "de").await;
    let dave_post = post_status_in(&pool, "dave", "root", "en").await;
    actions::post_status(
        &state(),
        PostParams {
            username: "carol",
            text: "a reply",
            visibility: "public",
            language: Some("en"),
            in_reply_to_id: Some(dave_post),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    actions::reblog_status(&state(), &carol, dave_post)
        .await
        .unwrap();
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 1);

    let (self_reply, _) = actions::post_status(
        &state(),
        PostParams {
            username: "carol",
            text: "more on that",
            visibility: "public",
            language: Some("en"),
            in_reply_to_id: Some(post),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    let items = notifications.as_array().unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["type"], "status");
    assert_eq!(items[0]["status"]["id"], self_reply.id.to_string());

    // types[]=status narrows to exactly these.
    let (_, filtered) = api(
        app(),
        "GET",
        "/api/v1/notifications?types[]=status",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(filtered.as_array().unwrap().len(), 2);

    // Turning notify off stops future notifications.
    api(
        app(),
        "POST",
        &format!("/api/v1/accounts/{}/follow", carol.id),
        Some(&alice_token),
        Some(json!({"notify": false})),
    )
    .await;
    post_status_in(&pool, "carol", "quiet now", "en").await;
    let (_, notifications) = api(
        app(),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(notifications.as_array().unwrap().len(), 2);
}
