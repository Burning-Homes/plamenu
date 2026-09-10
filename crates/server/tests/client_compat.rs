//! Client-bootstrap surface: the endpoints Mastodon apps call on launch
//! (custom emojis, announcements, filters, trends, followed tags,
//! preferences, app credential verification), notification type filtering,
//! and the favourites listings.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, status, user};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let (client_id, client_secret) = register_app(pool).await;
    let auth = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", "read write"),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await;
    let code = auth
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let token = api(
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
    (
        account,
        token.1["access_token"].as_str().unwrap().to_owned(),
    )
}

/// Registers the test OAuth app; returns (`client_id`, `client_secret`).
async fn register_app(pool: &PgPool) -> (String, String) {
    let (status, app) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "client-compat",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    (
        app["client_id"].as_str().unwrap().to_owned(),
        app["client_secret"].as_str().unwrap().to_owned(),
    )
}

async fn post_form(app: Router, uri: &str, fields: &[(&str, &str)]) -> String {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Generic JSON API call; returns (status, body).
async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, value) = api_with_headers(app, method, uri, bearer, body).await;
    (status, value)
}

async fn api_with_headers(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
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
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// Posts a status through the API and returns its entity.
async fn post_status(pool: &PgPool, token: &str, body: Value) -> Value {
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    entity
}

// ---------------------------------------------------------------------------
// Bootstrap stubs

#[sqlx::test(migrations = "../db/migrations")]
async fn public_stub_endpoints_serve_empty_arrays(pool: PgPool) {
    for uri in [
        "/api/v1/custom_emojis",
        "/api/v1/trends",
        "/api/v1/trends/tags",
        "/api/v1/trends/statuses",
        "/api/v1/trends/links",
    ] {
        let (status, body) = api(test_app(pool.clone()), "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, json!([]), "{uri}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_endpoints_accept_mastodon_py_trailing_slashes(pool: PgPool) {
    // Mastodon.py requests `/api/v{1,2}/instance/` with a trailing slash;
    // Rails collapses it, axum routes it separately. Without the aliases the
    // library's version probe 404s (and, through 2.2.1, recurses to death).
    for uri in ["/api/v1/instance/", "/api/v2/instance/"] {
        let (status, body) = api(test_app(pool.clone()), "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.get("version").is_some(), "{uri} serves instance data");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authenticated_stub_endpoints_require_a_token(pool: PgPool) {
    // `followed_tags` is a real endpoint now (covered in `tags.rs`); only
    // `announcements` remains an always-empty stub.
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let uri = "/api/v1/announcements";
    let (status, _) = api(test_app(pool.clone()), "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} without token");
    let (status, body) = api(test_app(pool.clone()), "GET", uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(body, json!([]), "{uri}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preferences_serve_mastodon_defaults(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/preferences",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, prefs) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/preferences",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(prefs["posting:default:visibility"], "public");
    assert_eq!(prefs["posting:default:sensitive"], false);
    assert_eq!(prefs["posting:default:quote_policy"], "public");
    assert_eq!(prefs["reading:expand:media"], "default");
    assert_eq!(prefs["reading:expand:spoilers"], false);
    assert_eq!(prefs["reading:autoplay:gifs"], false);

    // A locked account defaults to private posting, like Mastodon's
    // `setting_default_privacy` fallback.
    let (status, _) = api(
        test_app(pool.clone()),
        "PATCH",
        "/api/v1/accounts/update_credentials",
        Some(&token),
        Some(json!({ "locked": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, prefs) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/preferences",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(prefs["posting:default:visibility"], "private");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stored_preferences_feed_api_and_compose_defaults(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let user_row = user::find_by_email(&pool, "alice@plamenu.test")
        .await
        .unwrap()
        .unwrap();
    user::update_settings(
        &pool,
        user_row.id,
        user::UserSettings {
            posting_default_visibility: user::PostingDefaultVisibility::Unlisted,
            posting_default_sensitive: true,
            posting_default_language: "de".to_owned(),
            posting_default_quote_policy: user::DefaultQuotePolicy::Followers,
            posting_default_content_type: user::PostingDefaultFormat::Markdown,
            reading_expand_media: user::ReadingExpandMedia::HideAll,
            reading_expand_spoilers: true,
            reading_autoplay_gifs: true,
            reading_allow_direct_remote_media: false,
            reading_collapse_boosts: true,
            reading_translate_language: None,
            timeline_order: user::TimelineOrder::Received,
            thread_order: user::ThreadOrder::Flat,
            noindex: false,
            show_application: true,
            time_zone: None,
        },
    )
    .await
    .unwrap();

    let (status_code, prefs) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/preferences",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(prefs["posting:default:visibility"], "unlisted");
    assert_eq!(prefs["posting:default:sensitive"], true);
    assert_eq!(prefs["posting:default:language"], "de");
    assert_eq!(prefs["posting:default:quote_policy"], "followers");
    assert_eq!(prefs["reading:expand:media"], "hide_all");
    assert_eq!(prefs["reading:expand:spoilers"], true);
    assert_eq!(prefs["reading:autoplay:gifs"], true);
    assert_eq!(prefs["reading:timeline:order"], "received");
    assert_eq!(prefs["reading:thread:order"], "flat");

    let (status_code, credentials) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(credentials["source"]["privacy"], "unlisted");
    assert_eq!(credentials["source"]["sensitive"], true);
    assert_eq!(credentials["source"]["language"], "de");
    assert_eq!(credentials["source"]["quote_policy"], "followers");

    let entity = post_status(
        &pool,
        &token,
        json!({
            "status": "api preference defaults",
        }),
    )
    .await;
    let status_id = entity["id"].as_str().unwrap().parse::<i64>().unwrap();
    let stored = status::find_by_id(&pool, status_id).await.unwrap().unwrap();
    assert_eq!(stored.account_id, alice.id);
    assert_eq!(stored.visibility, "unlisted");
    assert!(stored.sensitive);
    assert_eq!(stored.language.as_deref(), Some("de"));
    // The default quote policy preference feeds a post without the param.
    assert_eq!(
        stored.quote_approval_policy,
        plamenu_ap::quote_policy::AUTOMATIC_FOLLOWERS
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn apps_verify_credentials_returns_the_tokens_app(pool: PgPool) {
    // Without (or with a bogus) token: Mastodon's 401.
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/apps/verify_credentials",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "The access token is invalid");
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/apps/verify_credentials",
        Some("bogus"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A user token resolves to its application.
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (status, app) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/apps/verify_credentials",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app["name"], "client-compat");
    assert_eq!(app["scopes"], json!(["read", "write"]));
    assert_eq!(app["redirect_uris"], json!(["urn:ietf:wg:oauth:2.0:oob"]));
    assert!(app.get("client_secret").is_none(), "secret never echoed");

    // An app-level (client_credentials) token works too — Mastodon only
    // checks token validity here, not the presence of a user.
    let (client_id, client_secret) = register_app(&pool).await;
    let (status, granted) = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "client_credentials",
            "client_id": client_id,
            "client_secret": client_secret,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let app_token = granted["access_token"].as_str().unwrap();
    let (status, app) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/apps/verify_credentials",
        Some(app_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app["name"], "client-compat");
}

// ---------------------------------------------------------------------------
// Notification type filtering

/// Seeds alice with three notification kinds from bob: `follow`,
/// `favourite` and `mention`, all through real API flows.
async fn seed_notifications(pool: &PgPool) -> (Account, String, Account, String) {
    let (alice, alice_token) = user_with_token(pool, "alice").await;
    let (bob, bob_token) = user_with_token(pool, "bob").await;
    let post = post_status(pool, &alice_token, json!({ "status": "hello world" })).await;
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/accounts/{}/follow", alice.id),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!(
            "/api/v1/statuses/{}/favourite",
            post["id"].as_str().unwrap()
        ),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    post_status(pool, &bob_token, json!({ "status": "@alice hi!" })).await;
    (alice, alice_token, bob, bob_token)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn notifications_filter_by_types_and_exclude_types(pool: PgPool) {
    let (_alice, alice_token, bob, _bob_token) = seed_notifications(&pool).await;

    let kinds = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_owned())
            .collect()
    };

    // Unfiltered: all three, newest first.
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kinds(&body), ["mention", "favourite", "follow"]);

    // types[] narrows to the requested kinds.
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?types%5B%5D=follow&types%5B%5D=favourite",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(kinds(&body), ["favourite", "follow"]);

    // exclude_types[] removes kinds; the filter survives in the Link header.
    let (status, headers, body) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?exclude_types%5B%5D=mention",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kinds(&body), ["favourite", "follow"]);
    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    assert!(
        link.contains("exclude_types%5B%5D=mention"),
        "filter echoed in pagination links: {link}"
    );

    // A type Mastodon knows but Plamenu never emits matches nothing —
    // and an unknown type matches nothing rather than everything.
    for query in ["types[]=poll", "types[]=bogus"] {
        let (_, body) = api(
            test_app(pool.clone()),
            "GET",
            &format!("/api/v1/notifications?{query}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(body, json!([]), "{query}");
    }

    // account_id narrows to one sender (bob sent everything here).
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/notifications?account_id={}", bob.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(kinds(&body).len(), 3);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications?account_id=1",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!([]));

    // unread_count honours the same narrowing.
    let (_, count) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/notifications/unread_count?exclude_types%5B%5D=mention",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(count["count"], 2);
}

// ---------------------------------------------------------------------------
// Favourites listings

#[sqlx::test(migrations = "../db/migrations")]
async fn favourites_index_lists_favourited_statuses(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let first = post_status(&pool, &alice_token, json!({ "status": "first" })).await;
    let second = post_status(&pool, &alice_token, json!({ "status": "second" })).await;
    for post in [&first, &second] {
        let (status, _) = api(
            test_app(pool.clone()),
            "POST",
            &format!(
                "/api/v1/statuses/{}/favourite",
                post["id"].as_str().unwrap()
            ),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/favourites",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Newest favourite first, with the favourite flag set.
    let (status, headers, body) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v1/favourites",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], second["id"]);
    assert_eq!(items[1]["id"], first["id"]);
    assert_eq!(items[0]["favourited"], true);
    assert!(headers.get(header::LINK).is_some());

    // Pagination by favourite row id: a limit-1 page links onward and the
    // next page holds the older favourite.
    let (_, headers, body) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        "/api/v1/favourites?limit=1",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    let next = link
        .split("; rel=\"next\"")
        .next()
        .unwrap()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let next_path = next.split("plamenu.test").nth(1).unwrap();
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        next_path,
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap()[0]["id"], first["id"]);

    // Alice favourited nothing.
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/favourites",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn favourited_by_and_reblogged_by_list_accounts(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;
    let post = post_status(&pool, &alice_token, json!({ "status": "popular" })).await;
    let post_id = post["id"].as_str().unwrap();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/favourite"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/statuses/{post_id}/reblog"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Both listings are public for a public status, like Mastodon.
    for which in ["favourited_by", "reblogged_by"] {
        let (status, body) = api(
            test_app(pool.clone()),
            "GET",
            &format!("/api/v1/statuses/{post_id}/{which}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{which}");
        let accounts = body.as_array().unwrap();
        assert_eq!(accounts.len(), 1, "{which}");
        assert_eq!(accounts[0]["username"], "bob", "{which}");
    }

    // An invisible status 404s for the listings just like for the status.
    let dm = post_status(
        &pool,
        &alice_token,
        json!({ "status": "private note to self", "visibility": "direct" }),
    )
    .await;
    for which in ["favourited_by", "reblogged_by"] {
        let (status, _) = api(
            test_app(pool.clone()),
            "GET",
            &format!("/api/v1/statuses/{}/{which}", dm["id"].as_str().unwrap()),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{which}");
    }

    // Blocking hides the blocked account from the lists, like Mastodon's
    // `not_excluded_by_account`.
    let bob_id = {
        let (_, accounts) = api(
            test_app(pool.clone()),
            "GET",
            &format!("/api/v1/statuses/{post_id}/favourited_by"),
            None,
            None,
        )
        .await;
        accounts.as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/accounts/{bob_id}/block"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{post_id}/favourited_by"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(body, json!([]), "blocked account hidden from the viewer");
    // Anonymous viewers still see everyone.
    let (_, body) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{post_id}/favourited_by"),
        None,
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}
