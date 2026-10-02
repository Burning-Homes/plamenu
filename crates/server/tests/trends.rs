//! Trending hashtags: the ranking engine (`trends::refresh_tags`), the public
//! `GET /api/v1/trends/tags` gate, and the admin surface (`admin/tags`,
//! `admin/trends/tags` review).

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::{
    PgPool, favourite, follow, instance_settings, oauth, preview_card, preview_card_trend, role,
    status, tag, user,
};
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
use tower::ServiceExt;

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

/// A local user promoted to the Admin role with a full admin-scoped token.
async fn admin_token(pool: &PgPool, username: &str) -> (Account, String) {
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
    let admin = role::find_by_name(pool, "Admin").await.unwrap().unwrap();
    role::assign_to_account(pool, account.id, Some(admin.id))
        .await
        .unwrap();
    let scopes = "read write admin:read admin:write";
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "trends-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes,
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(row.id), scopes)
        .await
        .unwrap();
    (account, token)
}

/// Seeds `accounts` distinct authors each posting one public status carrying
/// `name` on `day`, then registering its usage. Returns the tag id.
async fn seed_trending_tag(pool: &PgPool, name: &str, accounts: usize, day: Date) -> i64 {
    let tag_id = tag::ensure(pool, name).await.unwrap();
    for i in 0..accounts {
        let author = create_local_account(pool, &format!("{name}fan{i}"), "fan").await;
        let post = status::create_local(
            pool,
            status::NewLocalStatus::new(author.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();
        tag::attach(pool, post.id, tag_id).await.unwrap();
        tag::record_uses(pool, post.id, day).await.unwrap();
    }
    tag_id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn trends_refresh_gated_on_review(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let today = OffsetDateTime::now_utc().date();
    // Six accounts push #rust over the threshold today (expected 0 → 1,
    // observed 6 → score (6-1)^2/1 = 25).
    let tag_id = seed_trending_tag(&pool, "rust", 6, today).await;
    plamenu::trends::refresh_tags(&state).await.unwrap();

    // Trendable-by-default is off, so the tag trends but isn't allowed yet —
    // the public endpoint hides it.
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/tags",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert!(
        public.as_array().unwrap().is_empty(),
        "pending review, not public: {public}"
    );

    // A moderator sees it in the review list, flagged for review.
    let (_admin, token) = admin_token(&pool, "moddy").await;
    let (status, review) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/trends/tags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review[0]["name"], "rust");
    assert_eq!(review[0]["id"], tag_id.to_string());
    assert_eq!(review[0]["trendable"], false);
    assert_eq!(review[0]["requires_review"], true);

    // Approving marks it trendable and reviewed.
    let (status, approved) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/trends/tags/{tag_id}/approve"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["trendable"], true);
    assert_eq!(approved["requires_review"], false);

    // After the next refresh the trend's allowed flag flips and it surfaces.
    plamenu::trends::refresh_tags(&state).await.unwrap();
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/tags",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public[0]["name"], "rust");
    assert_eq!(public[0]["id"], tag_id.to_string());
    // The public Tag entity carries the seven-day history (six uses today).
    assert_eq!(public[0]["history"][0]["accounts"], "6");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn trends_endpoint_honours_operator_switch(pool: PgPool) {
    // Turn trends off but let tags trend without review.
    let current = instance_settings::get(&pool).await.unwrap();
    let mut update = current.as_update();
    update.trends_enabled = false;
    update.trendable_by_default = true;
    instance_settings::save(&pool, update).await.unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    let today = OffsetDateTime::now_utc().date();
    seed_trending_tag(&pool, "rust", 6, today).await;
    plamenu::trends::refresh_tags(&state).await.unwrap();

    // The tag is allowed (trendable_by_default), but trends are disabled, so the
    // public endpoint still serves an empty list rather than 404ing.
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/tags",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert!(
        public.as_array().unwrap().is_empty(),
        "trends disabled: {public}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_trends_refresh_review_and_public(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    // An eligible author: discoverable, posting a public, language-tagged
    // original without a content warning.
    let author = create_local_account(&pool, "author", "Author").await;
    sqlx::query!(
        "UPDATE accounts SET discoverable = true WHERE id = $1",
        author.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let post = status::create_local(
        &pool,
        status::NewLocalStatus {
            language: Some("en"),
            ..status::NewLocalStatus::new(author.id, "<p>hot take</p>", "public", None)
        },
    )
    .await
    .unwrap();
    // Five accounts favourite it today (observed 5 → score (5-1)² = 16).
    for i in 0..5 {
        let fan = create_local_account(&pool, &format!("fan{i}"), "fan").await;
        favourite::create(&pool, fan.id, post.id, None)
            .await
            .unwrap();
    }
    plamenu::trends::refresh_statuses(&state).await.unwrap();

    // Pending review → the public endpoint hides it.
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/statuses",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert!(public.as_array().unwrap().is_empty(), "pending: {public}");

    // Moderator review list shows it, flagged for review.
    let (_admin, token) = admin_token(&pool, "moddy").await;
    let (status, review) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/trends/statuses",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review[0]["id"], post.id.to_string());
    assert_eq!(review[0]["requires_review"], true);

    // Approve → refresh → it surfaces publicly with its live favourite count.
    let (status, approved) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/trends/statuses/{}/approve", post.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["requires_review"], false);

    plamenu::trends::refresh_statuses(&state).await.unwrap();
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/statuses",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public[0]["id"], post.id.to_string());
    assert_eq!(public[0]["favourites_count"], 5);
}

/// `accounts.discoverable` is nullable and stays NULL for accounts that never
/// declared it (most remote actors). The eligibility expression must not
/// propagate that NULL — it made every refresh pass fail to decode, killing
/// the whole trends scheduler on staging.
#[sqlx::test(migrations = "../db/migrations")]
async fn status_refresh_survives_null_discoverable_author(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    // Fresh local accounts are discoverable by default now (migration 0127),
    // but this guards the NULL path (most remote actors) — pin it back to NULL.
    let author = create_local_account(&pool, "shy", "Shy").await;
    sqlx::query!(
        "UPDATE accounts SET discoverable = NULL WHERE id = $1",
        author.id
    )
    .execute(&pool)
    .await
    .unwrap();
    status::create_local(
        &pool,
        status::NewLocalStatus {
            language: Some("en"),
            ..status::NewLocalStatus::new(author.id, "<p>hello</p>", "public", None)
        },
    )
    .await
    .unwrap();

    plamenu::trends::refresh_statuses(&state)
        .await
        .expect("a NULL-discoverable author must not break the refresh pass");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn link_trends_refresh_review_and_public(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let today = OffsetDateTime::now_utc().date();

    // An article-like card (title + description + image + provider), the only
    // kind eligible for link trends.
    let card = preview_card::upsert(
        &pool,
        preview_card::NewPreviewCard {
            url: "https://news.example/story",
            title: "Big News",
            description: "Something happened",
            kind: "link",
            provider_name: "Example News",
            image_url: Some("https://news.example/img.png"),
            language: Some("en"),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Five accounts share the link today (observed 5 → score 16).
    for i in 0..5 {
        let author = create_local_account(&pool, &format!("sharer{i}"), "s").await;
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(author.id, "<p>look</p>", "public", None),
        )
        .await
        .unwrap();
        preview_card::attach(&pool, post.id, card.id, "https://news.example/story")
            .await
            .unwrap();
        preview_card_trend::record_use(&pool, card.id, post.id, today)
            .await
            .unwrap();
    }
    plamenu::trends::refresh_links(&state).await.unwrap();

    // Pending review → the public endpoint hides it.
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/links",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert!(public.as_array().unwrap().is_empty(), "pending: {public}");

    // Moderator review list shows the link and its publisher; approve the card.
    let (_admin, token) = admin_token(&pool, "moddy").await;
    let (status, review) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/trends/links",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review[0]["id"], card.id.to_string());
    assert_eq!(review[0]["requires_review"], true);

    // The publisher was registered and can be listed.
    let (status, publishers) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/trends/links/publishers",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{publishers}");
    assert_eq!(publishers[0]["domain"], "news.example");
    assert_eq!(publishers[0]["requires_review"], true);

    let (status, approved) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/admin/trends/links/{}/approve", card.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["requires_review"], false);

    // After the next refresh the link surfaces publicly with its history.
    plamenu::trends::refresh_links(&state).await.unwrap();
    let (status, public) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/trends/links",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public[0]["url"], "https://news.example/story");
    assert_eq!(public[0]["history"][0]["accounts"], "5");

    // The link timeline returns the sharing statuses.
    let (status, timeline) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/link?url=https://news.example/story",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{timeline}");
    assert_eq!(timeline.as_array().unwrap().len(), 5, "five sharers");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn follow_suggestions_sources_and_dismissal(pool: PgPool) {
    // The viewer follows alice; alice follows carol and dave; carol also
    // follows dave. So carol is a friend-of-friend and dave is both a
    // friend-of-friend and the most-followed (two followers).
    let (viewer, token) = admin_token(&pool, "viewer").await;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let dave = create_local_account(&pool, "dave", "Dave").await;
    for account in [&carol, &dave] {
        sqlx::query!(
            "UPDATE accounts SET discoverable = true WHERE id = $1",
            account.id
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    follow::create(&pool, viewer.id, alice.id, None)
        .await
        .unwrap();
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();
    follow::create(&pool, alice.id, dave.id, None)
        .await
        .unwrap();
    follow::create(&pool, carol.id, dave.id, None)
        .await
        .unwrap();

    let (status, suggestions) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/suggestions",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{suggestions}");
    let carol_id = carol.id.to_string();
    let dave_id = dave.id.to_string();
    let carol_entry = suggestions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["account"]["id"] == carol_id.as_str())
        .expect("carol is suggested");
    // Carol reached the viewer through a followee, so her source is the
    // past-interactions bucket and lists friends_of_friends.
    assert_eq!(carol_entry["source"], "past_interactions");
    assert!(
        carol_entry["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "friends_of_friends"),
        "carol via friends_of_friends: {carol_entry}"
    );
    assert!(
        suggestions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["account"]["id"] == dave_id.as_str()),
        "dave is suggested: {suggestions}"
    );
    // Alice (already followed) is never suggested.
    let alice_id = alice.id.to_string();
    assert!(
        !suggestions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["account"]["id"] == alice_id.as_str()),
        "already-followed alice is excluded"
    );

    // Dismissing carol removes her (and only her) from both API versions.
    let (status, body) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/suggestions/{}", carol.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({}));

    let (status, after) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/suggestions",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    // v1 is a flat account list.
    assert!(
        !after
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == carol_id.as_str()),
        "dismissed carol gone: {after}"
    );
    assert!(
        after
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == dave_id.as_str()),
        "dave still suggested: {after}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_tag_registry_show_update_list(pool: PgPool) {
    let (_admin, token) = admin_token(&pool, "moddy").await;
    let tag_id = tag::ensure(&pool, "rust").await.unwrap();

    // Show: unset registry fields read as their defaults, and the tag is
    // unreviewed.
    let (status, show) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/admin/tags/{tag_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{show}");
    assert_eq!(show["name"], "rust");
    assert_eq!(show["usable"], true);
    assert_eq!(show["listable"], true);
    assert_eq!(show["trendable"], false);
    assert_eq!(show["requires_review"], true);

    // Update: set a display casing, disable usable, allow trending. Reviewing
    // stamps `reviewed_at`, so `requires_review` clears.
    let (status, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/admin/tags/{tag_id}"),
        Some(&token),
        Some(json!({ "display_name": "Rust", "usable": false, "trendable": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["name"], "Rust", "display casing now surfaces");
    assert_eq!(updated["usable"], false);
    assert_eq!(updated["trendable"], true);
    assert_eq!(
        updated["listable"], true,
        "untouched field keeps its default"
    );
    assert_eq!(updated["requires_review"], false);

    // The index lists the tag with its new casing.
    let (status, list) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/tags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let tag_id_str = tag_id.to_string();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "Rust" && t["id"] == tag_id_str.as_str()),
        "updated tag appears in the admin index: {list}"
    );

    // Unauthenticated access is rejected.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/admin/tags",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn tag_viewer_flags_keep_both_reasons_and_exact_scope(pool: PgPool) {
    let alice = create_local_account(&pool, "flagalice", "Alice").await;
    let bob = create_local_account(&pool, "flagbob", "Bob").await;
    let both = tag::ensure(&pool, "both").await.unwrap();
    let followed = tag::ensure(&pool, "followed").await.unwrap();
    let featured = tag::ensure(&pool, "featured").await.unwrap();
    let neither = tag::ensure(&pool, "neither").await.unwrap();
    let outside = tag::ensure(&pool, "outside").await.unwrap();
    for id in [both, followed, outside] {
        tag::follow(&pool, alice.id, id).await.unwrap();
    }
    for id in [both, featured, outside] {
        plamenu_db::featured_tag::feature(&pool, alice.id, id)
            .await
            .unwrap();
    }
    tag::follow(&pool, bob.id, neither).await.unwrap();
    plamenu_db::featured_tag::feature(&pool, bob.id, followed)
        .await
        .unwrap();
    let ids = [both, followed, featured, neither, both];
    let flags = tag::viewer_flags(&pool, alice.id, &ids).await.unwrap();
    assert_eq!(flags.following, [both, followed].into_iter().collect());
    assert_eq!(flags.featuring, [both, featured].into_iter().collect());
    let flags = tag::viewer_flags(&pool, bob.id, &ids).await.unwrap();
    assert_eq!(flags.following, [neither].into_iter().collect());
    assert_eq!(flags.featuring, [followed].into_iter().collect());
    let empty = tag::viewer_flags(&pool, alice.id, &[]).await.unwrap();
    assert!(empty.following.is_empty() && empty.featuring.is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn trends_tag_viewer_page_preserves_order_history_and_anonymous_shape(pool: PgPool) {
    let today = OffsetDateTime::now_utc().date();
    let high = seed_trending_tag(&pool, "high", 1, today).await;
    let middle = seed_trending_tag(&pool, "middle", 1, today).await;
    let low = seed_trending_tag(&pool, "low", 1, today).await;
    let denied = seed_trending_tag(&pool, "denied", 1, today).await;
    for (id, score, allowed) in [
        (high, 60.0, true),
        (middle, 30.0, true),
        (low, 10.0, true),
        (denied, 1000.0, false),
    ] {
        plamenu_db::tag_trend::upsert(&pool, id, score, allowed)
            .await
            .unwrap();
    }
    let past_author = create_local_account(&pool, "pasthistory", "Past").await;
    let past = status::create_local(
        &pool,
        status::NewLocalStatus::new(past_author.id, "past", "public", None),
    )
    .await
    .unwrap();
    tag::attach(&pool, past.id, high).await.unwrap();
    for age in [2, 7] {
        tag::record_uses(&pool, past.id, today - time::Duration::days(age))
            .await
            .unwrap();
    }
    let private = status::create_local(
        &pool,
        status::NewLocalStatus::new(past_author.id, "private", "private", None),
    )
    .await
    .unwrap();
    tag::attach(&pool, private.id, high).await.unwrap();
    tag::record_uses(&pool, private.id, today).await.unwrap();
    let (alice, alice_token) = admin_token(&pool, "pagealice").await;
    let (bob, bob_token) = admin_token(&pool, "pagebob").await;
    tag::follow(&pool, alice.id, high).await.unwrap();
    plamenu_db::featured_tag::feature(&pool, alice.id, high)
        .await
        .unwrap();
    tag::follow(&pool, bob.id, middle).await.unwrap();
    let app = test_app(pool.clone());
    let (status, alice_page) = api(
        app.clone(),
        "GET",
        "/api/v1/trends/tags?limit=2",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(alice_page[0]["id"], high.to_string());
    assert_eq!(alice_page[1]["id"], middle.to_string());
    assert_eq!(alice_page.as_array().unwrap().len(), 2);
    assert_eq!(alice_page[0]["following"], true);
    assert_eq!(alice_page[0]["featuring"], true);
    assert_eq!(alice_page[1]["following"], false);
    assert_eq!(alice_page[1]["featuring"], false);
    let history = alice_page[0]["history"].as_array().unwrap();
    assert_eq!(history.len(), 7);
    for (age, entry) in history.iter().enumerate() {
        let day = today - time::Duration::days(i64::try_from(age).unwrap());
        assert_eq!(
            entry["day"],
            day.with_time(time::Time::MIDNIGHT)
                .assume_utc()
                .unix_timestamp()
                .to_string()
        );
        let count = if age == 0 || age == 2 { "1" } else { "0" };
        assert_eq!(entry["uses"], count);
        assert_eq!(entry["accounts"], count);
    }
    let (status, bob_page) = api(
        app.clone(),
        "GET",
        "/api/v1/trends/tags?limit=1&offset=1",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bob_page.as_array().unwrap().len(), 1);
    assert_eq!(bob_page[0]["id"], middle.to_string());
    assert_eq!(bob_page[0]["following"], true);
    assert_eq!(bob_page[0]["featuring"], false);
    let (status, anonymous) = api(app, "GET", "/api/v1/trends/tags?limit=2", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let mut expected = alice_page;
    for entity in expected.as_array_mut().unwrap() {
        entity.as_object_mut().unwrap().remove("following");
        entity.as_object_mut().unwrap().remove("featuring");
    }
    assert_eq!(
        anonymous, expected,
        "anonymous omits both flags without changing ranked/history rows"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn trends_disabled_hides_populated_authenticated_flags(pool: PgPool) {
    let (viewer, token) = admin_token(&pool, "disabledviewer").await;
    let id = tag::ensure(&pool, "hidden").await.unwrap();
    tag::follow(&pool, viewer.id, id).await.unwrap();
    plamenu_db::featured_tag::feature(&pool, viewer.id, id)
        .await
        .unwrap();
    plamenu_db::tag_trend::upsert(&pool, id, 100.0, true)
        .await
        .unwrap();
    let settings = instance_settings::get(&pool).await.unwrap();
    let mut update = settings.as_update();
    update.trends_enabled = false;
    instance_settings::save(&pool, update).await.unwrap();
    let (status, page) = api(
        test_app(pool),
        "GET",
        "/api/v1/trends/tags",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page, json!([]));
}
