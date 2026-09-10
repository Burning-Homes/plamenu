//! Long-form authoring: publishing an `Article`, what the kind refuses, and
//! what the wire carries.
//!
//! The point of the kind is that a long-form post federates its **whole body**,
//! which only holds if three representations agree: the row's `object_type`, the
//! `Create` we deliver, and the object served at the post's own URL. Two of those
//! are built by different code paths (`actions::post_status` and
//! `note::note_for_status`), so most of what follows asserts they say the same
//! thing — that is the failure mode this track could plausibly ship.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, create_local_account, test_app, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::hash_password;
use plamenu::delivery;
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, follow, user};
use serde_json::{Value, json};
use tower::ServiceExt;

const BODY: &str = "A body long enough to be worth reading, and far past what a title-plus-link \
                    stub would carry.";

fn article_params<'a>(username: &'a str, title: &'a str, text: &'a str) -> PostParams<'a> {
    PostParams {
        username,
        text,
        visibility: "public",
        title: Some(title),
        kind: PostKind::Article,
        ..Default::default()
    }
}

async fn get_ap(app: Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A bearer token through the real OAuth flow, like `edits.rs` mints one.
async fn user_with_token(pool: &PgPool, username: &str) -> String {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let (_, app_json) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "long-form",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    let client_id = app_json["client_id"].as_str().unwrap().to_owned();
    let client_secret = app_json["client_secret"].as_str().unwrap().to_owned();
    let request = Request::builder()
        .method("POST")
        .uri("/oauth/authorize")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            serde_urlencoded::to_string([
                ("client_id", client_id.as_str()),
                ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                ("scope", "read write"),
                ("email", email.as_str()),
                ("password", "pw"),
            ])
            .unwrap(),
        ))
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page = String::from_utf8_lossy(&bytes).into_owned();
    let code = page
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let (_, token) = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        })),
    )
    .await;
    token["access_token"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn article_is_stored_typed_and_served_whole(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (stored, _) = actions::post_status(
        &state,
        article_params("alice", "Pimping my board games", BODY),
    )
    .await
    .unwrap();

    assert_eq!(stored.object_type.as_deref(), Some("Article"));
    assert_eq!(stored.title.as_deref(), Some("Pimping my board games"));

    let (status, object) = get_ap(
        test_app_with(pool.clone(), StubFederation::with_actors([])),
        &format!("/users/alice/statuses/{}", stored.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(object["type"], "Article");
    assert_eq!(object["name"], "Pimping my board games");
    let content = object["content"].as_str().unwrap();
    assert!(
        content.starts_with("<h1>Pimping my board games</h1>"),
        "the headline rides the body for peers that ignore `name`: {content}"
    );
    assert!(
        content.contains("far past what a title-plus-link"),
        "the full body federates: {content}"
    );
    // The stored body keeps what the author typed — the heading exists only on
    // the wire, so an edit never accumulates headings.
    assert!(!stored.content.contains("<h1>"), "{}", stored.content);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_create_and_the_served_object_agree(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("remote.test", "bob");
    let stub = StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), stub.clone());
    add_follower(&pool, &remote, alice.id).await;

    let (stored, _) = actions::post_status(&state, article_params("alice", "On typography", BODY))
        .await
        .unwrap();
    delivery::run_due(&state).await;

    let sent = stub.deliveries();
    let create = sent
        .iter()
        .map(|d| &d.activity)
        .find(|activity| activity["type"] == "Create")
        .expect("the Create was delivered");
    assert_eq!(create["object"]["type"], "Article");

    let (_, served) = get_ap(
        test_app_with(pool.clone(), StubFederation::with_actors([])),
        &format!("/users/alice/statuses/{}", stored.id),
    )
    .await;
    // Both builders, one shape: a receiver that stores the Create and later
    // re-fetches the object must not see two different kinds of post.
    for field in ["type", "name", "content"] {
        assert_eq!(
            create["object"][field], served[field],
            "the Create and the served object disagree on `{field}`"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_client_entity_reports_the_kind_and_folds_the_title(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (stored, _) = actions::post_status(&state, article_params("alice", "Kerning", BODY))
        .await
        .unwrap();

    let entity =
        plamenu::entities::render_status(&pool, common::TEST_DOMAIN, &stored, Some(alice.id))
            .await
            .unwrap();
    assert_eq!(entity["object_type"], "Article");
    assert_eq!(entity["title"], "Kerning");
    // A stock Mastodon client reads only `content`, so the title is folded in
    // there too — the same treatment a group `Page` gets.
    assert!(
        entity["content"]
            .as_str()
            .unwrap()
            .contains("<p class=\"status__title\"><strong>Kerning</strong></p>"),
        "{}",
        entity["content"]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn long_form_refuses_the_shapes_other_kinds_own(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));

    // A parent to reply to.
    let (parent, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "a thread starts",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cases: Vec<(&str, PostParams<'_>)> = vec![
        (
            "needs a title",
            PostParams {
                title: None,
                ..article_params("alice", "", BODY)
            },
        ),
        (
            "can't be a reply",
            PostParams {
                in_reply_to_id: Some(parent.id),
                ..article_params("alice", "A titled reply", BODY)
            },
        ),
        (
            "can't be a link post",
            PostParams {
                external_url: Some("https://example.com/elsewhere"),
                ..article_params("alice", "A link", BODY)
            },
        ),
        (
            "can't carry a poll",
            PostParams {
                poll: Some(actions::PollParams {
                    options: vec!["yes".to_owned(), "no".to_owned()],
                    expires_in: Some(3600),
                    multiple: false,
                    hide_totals: false,
                }),
                ..article_params("alice", "A poll", BODY)
            },
        ),
    ];
    for (name, params) in cases {
        let refused = actions::post_status(&state, params).await;
        assert!(
            matches!(refused, Err(plamenu::error::ApiError::Unprocessable(_))),
            "long-form {name}: expected a 422, got {:?}",
            refused.map(|(item, _)| item.id)
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn long_form_measures_against_its_own_character_limit(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    // Well past the 5000-character default for an ordinary post, well inside the
    // 50 000 a long-form post gets.
    let long = "word ".repeat(1200);

    let refused = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: &long,
            visibility: "public",
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(refused, Err(plamenu::error::ApiError::Unprocessable(_))),
        "an ordinary post is still capped"
    );

    let (stored, _) = actions::post_status(&state, article_params("alice", "Long", &long))
        .await
        .expect("the same text is fine as long-form");
    assert_eq!(stored.object_type.as_deref(), Some("Article"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_headline_can_be_fixed_and_federates_as_an_update(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let remote = RemoteUser::new("remote.test", "bob");
    let stub = StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), stub.clone());
    add_follower(&pool, &remote, alice.id).await;

    let (stored, _) = actions::post_status(&state, article_params("alice", "Teh headline", BODY))
        .await
        .unwrap();
    delivery::run_due(&state).await;

    let edited = actions::edit_status(
        &state,
        &alice,
        stored.id,
        actions::EditParams {
            title: Some("The headline"),
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(edited.title.as_deref(), Some("The headline"));
    assert_eq!(
        edited.object_type.as_deref(),
        Some("Article"),
        "an edit never changes the kind"
    );

    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let update = sent
        .iter()
        .map(|d| &d.activity)
        .find(|activity| activity["type"] == "Update")
        .expect("the corrected headline federates");
    assert_eq!(update["object"]["name"], "The headline");
    assert!(
        update["object"]["content"]
            .as_str()
            .unwrap()
            .starts_with("<h1>The headline</h1>"),
        "the baked heading is rebuilt from the new title"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_untitled_post_refuses_a_title(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (note, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "an ordinary note",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let refused = actions::edit_status(
        &state,
        &alice,
        note.id,
        actions::EditParams {
            title: Some("Suddenly a headline"),
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: None,
        },
    )
    .await;
    assert!(
        matches!(refused, Err(plamenu::error::ApiError::Unprocessable(_))),
        "gaining a name would change the kind the audience received"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_api_takes_the_kind_and_refuses_an_unknown_one(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;

    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": BODY,
            "post_kind": "article",
            "title": "Published through the API",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    assert_eq!(entity["object_type"], "Article");
    assert_eq!(entity["title"], "Published through the API");

    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "hello", "post_kind": "artickle"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a misspelled kind must not publish a plain post"
    );

    // Absent keeps the historical behaviour: an ordinary post, untyped column.
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entity["object_type"], Value::Null);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn articles_can_be_scheduled_but_events_cannot(pool: PgPool) {
    let token = user_with_token(&pool, "alice").await;
    let far_future = "2099-01-01T12:00:00Z";

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": BODY,
            "post_kind": "article",
            "title": "Later",
            "scheduled_at": far_future,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["params"]["post_kind"], "article");
    assert_eq!(body["params"]["title"], "Later");

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "our meetup",
            "title": "Meetup",
            "scheduled_at": far_future,
            "event": {"start_time": "2099-02-01T18:00:00Z"},
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the same hole existed for events: {body}"
    );

    // An ordinary post still schedules.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "later", "scheduled_at": far_future})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Lets a remote user follow the author, so a post has somewhere to go.
async fn add_follower(pool: &PgPool, user: &RemoteUser, author_id: i64) {
    let remote = plamenu::remote::store_remote_actor(pool, &user.actor)
        .await
        .unwrap();
    follow::create(pool, remote.id, author_id, None)
        .await
        .unwrap();
}
