//! The public discovery surface: `/api/v1/instance/{peers,activity,
//! domain_blocks,languages,translation_languages}`, `/api/v1/peers/search`
//! and `GET /api/oembed`, including the operator gating each one mirrors
//! from Mastodon.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TEST_DOMAIN, create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu_db::account::{Account, RemoteAccountData};
use plamenu_db::instance_policy::NewDomainBlock;
use plamenu_db::instance_settings::{DomainBlocksDisclosure, SettingsUpdate};
use plamenu_db::{PgPool, account, instance_policy, instance_settings, user};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A stored remote account on `domain`, without real key generation.
async fn remote_account(pool: &PgPool, domain: &str, username: &str) -> Account {
    let uri = format!("https://{domain}/users/{username}");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: &format!("{uri}#main-key"),
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

async fn save_settings(
    pool: &PgPool,
    patch: impl FnOnce(SettingsUpdate<'_>) -> SettingsUpdate<'_>,
) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(pool, patch(current.as_update()))
        .await
        .unwrap();
}

async fn block_domain(pool: &PgPool, domain: &str, severity: &str) {
    instance_policy::create_domain_block(
        pool,
        NewDomainBlock {
            domain,
            severity,
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
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

async fn get(pool: &PgPool, uri: &str) -> (StatusCode, Value) {
    api(test_app(pool.clone()), "GET", uri, None, None).await
}

/// A local account + user with an OAuth token (`read write`); the password
/// grant flow also records a `login_activities` row, which the activity
/// endpoint counts.
async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let (status, app) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "instance-meta",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": "read write",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = Request::builder()
        .method("POST")
        .uri("/oauth/authorize")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            serde_urlencoded::to_string([
                ("client_id", app["client_id"].as_str().unwrap()),
                ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                ("scope", "read write"),
                ("email", &email),
                ("password", "pw"),
            ])
            .unwrap(),
        ))
        .unwrap();
    let response = test_app(pool.clone()).oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page = String::from_utf8_lossy(&bytes);
    let code = page
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let (status, token) = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": app["client_id"],
            "client_secret": app["client_secret"],
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    (account, token["access_token"].as_str().unwrap().to_owned())
}

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
// Peers
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn peers_lists_known_domains_minus_blocked(pool: PgPool) {
    create_local_account(&pool, "local", "local").await;
    remote_account(&pool, "friendly.example", "a").await;
    remote_account(&pool, "blocked.example", "b").await;
    block_domain(&pool, "blocked.example", "suspend").await;

    let (status, body) = get(&pool, "/api/v1/instance/peers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!(["friendly.example"]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn peers_api_disabled_or_allow_list_is_404(pool: PgPool) {
    remote_account(&pool, "friendly.example", "a").await;

    save_settings(&pool, |update| SettingsUpdate {
        peers_api_enabled: false,
        ..update
    })
    .await;
    let (status, body) = get(&pool, "/api/v1/instance/peers").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
    let (status, _) = get(&pool, "/api/v1/peers/search?q=friendly").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Re-enabled but allow-list federation (Mastodon's limited federation
    // mode) also hides the API.
    save_settings(&pool, |update| SettingsUpdate {
        peers_api_enabled: true,
        ..update
    })
    .await;
    instance_policy::create_domain_allow(&pool, "friendly.example")
        .await
        .unwrap();
    let (status, _) = get(&pool, "/api/v1/instance/peers").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn peers_search_prefix_matches_most_populated_first(pool: PgPool) {
    remote_account(&pool, "mastodon.example", "a").await;
    remote_account(&pool, "mastodon.example", "b").await;
    remote_account(&pool, "mas.example", "c").await;
    remote_account(&pool, "other.example", "d").await;

    let (status, body) = get(&pool, "/api/v1/peers/search?q=mas").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!(["mastodon.example", "mas.example"]));

    // Blank query renders null, like Mastodon.
    let (status, body) = get(&pool, "/api/v1/peers/search").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);

    let (status, body) = get(&pool, "/api/v1/peers/search?q=nowhere").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    // LIKE metacharacters match literally, not as wildcards.
    let (status, body) = get(&pool, "/api/v1/peers/search?q=%25").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn activity_reports_twelve_weeks_of_strings(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    post_status(&pool, &token, json!({"status": "hello"})).await;

    let (status, body) = get(&pool, "/api/v1/instance/activity").await;
    assert_eq!(status, StatusCode::OK);
    let weeks = body.as_array().unwrap();
    assert_eq!(weeks.len(), 12);
    // Newest week first: the registration, the sign-in from the OAuth flow
    // and the status all land in it — every value a string.
    assert_eq!(weeks[0]["statuses"], "1");
    assert_eq!(weeks[0]["logins"], "1");
    assert_eq!(weeks[0]["registrations"], "1");
    assert!(weeks[0]["week"].as_str().unwrap().parse::<i64>().is_ok());
    assert_eq!(weeks[11]["statuses"], "0");
    let first: i64 = weeks[0]["week"].as_str().unwrap().parse().unwrap();
    let second: i64 = weeks[1]["week"].as_str().unwrap().parse().unwrap();
    assert_eq!(first - second, 7 * 24 * 3600);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn activity_api_disabled_is_404(pool: PgPool) {
    save_settings(&pool, |update| SettingsUpdate {
        activity_api_enabled: false,
        ..update
    })
    .await;
    let (status, body) = get(&pool, "/api/v1/instance/activity").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

// ---------------------------------------------------------------------------
// Domain blocks disclosure
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn domain_blocks_disclosure_ladder(pool: PgPool) {
    instance_policy::create_domain_block(
        &pool,
        NewDomainBlock {
            domain: "suspended.example",
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: Some("secret"),
            public_comment: Some("spam wave"),
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    block_domain(&pool, "silenced.example", "silence").await;
    block_domain(&pool, "noop.example", "noop").await;

    // Default: disclosed to nobody.
    let (status, _) = get(&pool, "/api/v1/instance/domain_blocks").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Users-only: anonymous 404, signed-in 200.
    save_settings(&pool, |update| SettingsUpdate {
        show_domain_blocks: DomainBlocksDisclosure::Users,
        ..update
    })
    .await;
    let (status, _) = get(&pool, "/api/v1/instance/domain_blocks").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, token) = user_with_token(&pool, "alice").await;
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/instance/domain_blocks",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Silence sorts before suspend; the noop block is not user-facing.
    assert_eq!(body[0]["domain"], "silenced.example");
    assert_eq!(body[0]["severity"], "silence");
    assert_eq!(body[1]["domain"], "suspended.example");
    assert_eq!(body[1]["severity"], "suspend");
    assert_eq!(body.as_array().unwrap().len(), 2);
    // SHA-256 of the domain, hex-encoded.
    assert_eq!(
        body[1]["digest"],
        "e59c8bb674a866f5bcfcef1406937ef735c1ac2f6b9af6dac4069e8aaeb00ca6"
    );
    // Rationale still disabled: comment withheld.
    assert_eq!(body[1]["comment"], Value::Null);

    // Everyone, with rationale.
    save_settings(&pool, |update| SettingsUpdate {
        show_domain_blocks: DomainBlocksDisclosure::All,
        show_domain_blocks_rationale: DomainBlocksDisclosure::All,
        ..update
    })
    .await;
    let (status, body) = get(&pool, "/api/v1/instance/domain_blocks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[1]["comment"], "spam wave");
    assert_eq!(body[0]["comment"], "");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn domain_blocks_obfuscation(pool: PgPool) {
    instance_policy::create_domain_block(
        &pool,
        NewDomainBlock {
            domain: "example.com",
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: true,
        },
    )
    .await
    .unwrap();
    save_settings(&pool, |update| SettingsUpdate {
        show_domain_blocks: DomainBlocksDisclosure::All,
        ..update
    })
    .await;
    let (status, body) = get(&pool, "/api/v1/instance/domain_blocks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["domain"], "exa****.*om");
    // The digest is of the real domain, so clients can still match it.
    assert_eq!(
        body[0]["digest"],
        "a379a6f6eeafb9a55e378c118034e2751e682fab9f2d30ab13d2125586ce1947"
    );
}

// ---------------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn languages_lists_the_posting_locales(pool: PgPool) {
    let (status, body) = get(&pool, "/api/v1/instance/languages").await;
    assert_eq!(status, StatusCode::OK);
    let languages = body.as_array().unwrap();
    assert_eq!(languages.len(), 214);
    assert!(
        languages
            .iter()
            .any(|l| l["code"] == "en" && l["name"] == "English")
    );
    // Sorted by code.
    assert_eq!(languages[0]["code"], "aa");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn translation_languages_empty_without_backend(pool: PgPool) {
    let (status, body) = get(&pool, "/api/v1/instance/translation_languages").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({}));
}

// ---------------------------------------------------------------------------
// oEmbed
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn oembed_serves_public_local_statuses(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let entity = post_status(&pool, &token, json!({"status": "hello world"})).await;
    let id = entity["id"].as_str().unwrap();

    let url = format!("https://{TEST_DOMAIN}/@alice/{id}");
    let (status, body) = get(&pool, &format!("/api/oembed?url={url}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "rich");
    assert_eq!(body["version"], "1.0");
    assert_eq!(body["author_name"], "alice");
    assert_eq!(body["provider_name"], TEST_DOMAIN);
    assert_eq!(body["width"], 400);
    assert_eq!(body["height"], Value::Null);
    assert!(body["html"].as_str().unwrap().contains(&url));

    // The AP id form resolves too, and maxwidth/maxheight are honored.
    let ap_url = format!("https://{TEST_DOMAIN}/users/alice/statuses/{id}");
    let (status, body) = get(
        &pool,
        &format!("/api/oembed?url={ap_url}&maxwidth=550&maxheight=300"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["width"], 550);
    assert_eq!(body["height"], 300);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oembed_hides_non_public_and_foreign_urls(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "alice").await;
    let entity = post_status(
        &pool,
        &token,
        json!({"status": "just us", "visibility": "private"}),
    )
    .await;
    let id = entity["id"].as_str().unwrap();

    let (status, body) = get(
        &pool,
        &format!("/api/oembed?url=https://{TEST_DOMAIN}/@alice/{id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");

    let (status, _) = get(
        &pool,
        &format!("/api/oembed?url=https://elsewhere.example/@alice/{id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get(&pool, "/api/oembed?url=not-a-url").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get(&pool, "/api/oembed").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
