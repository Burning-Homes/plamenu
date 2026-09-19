//! Integration tests: the full router against a real (per-test) Postgres
//! database, driven through tower without binding a socket.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TEST_DOMAIN, create_local_account, test_app, test_app_split_domain};
use http_body_util::BodyExt;
use plamenu_db::PgPool;
use serde_json::Value;
use tower::ServiceExt;

async fn create_alice(pool: &PgPool) {
    create_local_account(pool, "alice", "Alice").await;
}

/// GETs `uri` and returns status, content-type and parsed JSON body (if any).
async fn get(app: Router, uri: &str, accept: Option<&str>) -> (StatusCode, String, Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, content_type, json)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn health_works(pool: PgPool) {
    let (status, _, _) = get(test_app(pool), "/health", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn readiness_reflects_database_availability(pool: PgPool) {
    let app = test_app(pool.clone());
    // Liveness is a constant OK regardless of the datastore.
    assert_eq!(get(app.clone(), "/health", None).await.0, StatusCode::OK);
    // Readiness is 200 while the pool can serve a query.
    assert_eq!(get(app.clone(), "/ready", None).await.0, StatusCode::OK);
    // Take the datastore away: readiness must now report 503 instead of a
    // false OK, while liveness stays 200.
    pool.close().await;
    assert_eq!(
        get(app.clone(), "/ready", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(get(app, "/health", None).await.0, StatusCode::OK);
}

/// Readiness also reflects supervisor state — a restart-looping
/// background worker turns the instance unready (naming the subsystem), while
/// an isolated crash that recovers does not.
#[sqlx::test(migrations = "../db/migrations")]
async fn readiness_reflects_worker_restart_loops(pool: PgPool) {
    let state = common::test_state_with(pool, std::sync::Arc::default());
    let workers = state.workers.clone();
    let app = plamenu::build_router(state);

    // One recorded exit (a crash the respawn recovered from) stays ready.
    workers.record_exit("delivery");
    assert_eq!(get(app.clone(), "/ready", None).await.0, StatusCode::OK);

    // A crash loop crosses the threshold: unready, with the worker named.
    workers.record_exit("delivery");
    workers.record_exit("delivery");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("delivery"),
        "the failing subsystem is named: {body}"
    );
    // Liveness never flips for worker state.
    assert_eq!(get(app, "/health", None).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_lists_avatar_link_when_account_has_one(pool: PgPool) {
    create_alice(&pool).await;
    let uri = "/.well-known/webfinger?resource=acct:alice@plamenu.test";
    let (_, _, without) = get(test_app(pool.clone()), uri, None).await;
    assert!(
        !without["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "http://webfinger.net/rel/avatar"),
        "no avatar link without an avatar on file"
    );
    sqlx::query!("UPDATE accounts SET avatar_file_name = 'ava-9.jpg' WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, body) = get(test_app(pool), uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let avatar = body["links"]
        .as_array()
        .unwrap()
        .last()
        .expect("links present");
    assert_eq!(avatar["rel"], "http://webfinger.net/rel/avatar");
    assert_eq!(avatar["type"], "image/jpeg");
    assert_eq!(avatar["href"], "https://plamenu.test/media/ava-9.jpg");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_is_case_insensitive_and_canonicalizes(pool: PgPool) {
    create_alice(&pool).await;
    let uri = "/.well-known/webfinger?resource=acct:ALICE@PLAMENU.TEST";
    let (status, _, body) = get(test_app(pool), uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["subject"], "acct:alice@plamenu.test");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn split_domain_webfinger_maps_short_handles_to_the_host_domain(pool: PgPool) {
    create_alice(&pool).await;
    let app = || test_app_split_domain(pool.clone(), "social.example.test", "example.test");

    // The short handle is canonical, but profile, actor and interaction URLs
    // remain on the server origin. The apex only needs to redirect/proxy this
    // well-known request here while preserving its query string.
    let (status, _, body) = get(
        app(),
        "/.well-known/webfinger?resource=acct:alice%40example.test",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["subject"], "acct:alice@example.test");
    assert_eq!(body["aliases"][0], "https://social.example.test/@alice");
    assert_eq!(
        body["aliases"][1],
        "https://social.example.test/users/alice"
    );
    assert_eq!(
        body["links"][0]["href"],
        "https://social.example.test/@alice"
    );
    assert_eq!(
        body["links"][1]["href"],
        "https://social.example.test/users/alice"
    );
    assert_eq!(
        body["links"][2]["template"],
        "https://social.example.test/interact?uri={uri}"
    );

    // Peers that discover the actor URL first commonly try its host as the
    // acct domain. Mastodon and GoToSocial accept that alias and still return
    // the canonical short-domain subject; Plamenu does the same.
    let (status, _, host_alias) = get(
        app(),
        "/.well-known/webfinger?resource=acct:ALICE%40SOCIAL.EXAMPLE.TEST",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(host_alias["subject"], "acct:alice@example.test");

    // Client-side account lookup treats both handle domains as local too;
    // neither spelling is mistaken for (or persisted as) a remote account.
    for acct in ["alice%40example.test", "alice%40social.example.test"] {
        let (status, _, account) =
            get(app(), &format!("/api/v1/accounts/lookup?acct={acct}"), None).await;
        assert_eq!(status, StatusCode::OK, "{acct}");
        assert_eq!(account["acct"], "alice");
        assert_eq!(account["url"], "https://social.example.test/@alice");
    }

    let (status, _, instance) = get(
        app(),
        "/.well-known/webfinger?resource=acct:example.test%40example.test",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(instance["subject"], "acct:example.test@example.test");
    assert_eq!(
        instance["links"][0]["href"],
        "https://social.example.test/actor"
    );

    let (_, _, v1) = get(app(), "/api/v1/instance", None).await;
    assert_eq!(v1["uri"], "social.example.test");
    assert_eq!(v1["account_domain"], "example.test");

    let (_, _, v2) = get(app(), "/api/v2/instance", None).await;
    assert_eq!(v2["domain"], "social.example.test");
    assert_eq!(v2["account_domain"], "example.test");
}

/// Mastodon 410s webfinger only for *permanently* unavailable (deleted)
/// accounts; a reversibly suspended account still answers so peers can
/// dereference the actor and read `suspended: true`. The suspended actor's
/// collections answer 403 (temporary) instead of 410.
#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_and_collections_follow_the_availability_ladder(pool: PgPool) {
    create_alice(&pool).await;
    let alice = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::account::suspend(&pool, alice.id, "local")
        .await
        .unwrap();

    let jrd_uri = "/.well-known/webfinger?resource=acct:alice@plamenu.test";
    let (status, _, _) = get(test_app(pool.clone()), jrd_uri, None).await;
    assert_eq!(status, StatusCode::OK, "suspended: webfinger still answers");
    let (status, _, body) = get(
        test_app(pool.clone()),
        "/users/alice",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "suspended: blanked actor answers");
    assert_eq!(body["suspended"], true);
    let (status, _, _) = get(
        test_app(pool.clone()),
        "/users/alice/collections/featured",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "suspended: collections 403");

    plamenu_db::account::mark_deleted(&pool, alice.id)
        .await
        .unwrap();
    let (status, _, _) = get(test_app(pool.clone()), jrd_uri, None).await;
    assert_eq!(status, StatusCode::GONE, "deleted: webfinger is gone");
    let (status, _, _) = get(
        test_app(pool.clone()),
        "/users/alice",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "deleted: actor is gone");
    let (status, _, _) = get(
        test_app(pool),
        "/users/alice/collections/featured",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "deleted: collections are gone");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_unknown_user_is_404(pool: PgPool) {
    let uri = "/.well-known/webfinger?resource=acct:ghost@plamenu.test";
    let (status, _, body) = get(test_app(pool), uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_foreign_domain_is_404(pool: PgPool) {
    create_alice(&pool).await;
    let uri = "/.well-known/webfinger?resource=acct:alice@elsewhere.example";
    let (status, _, _) = get(test_app(pool), uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_malformed_resource_is_400(pool: PgPool) {
    let uri = "/.well-known/webfinger?resource=acct:not-an-acct";
    let (status, _, _) = get(test_app(pool), uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn host_meta_serves_xrd_xml_by_default(pool: PgPool) {
    let response = test_app(pool)
        .oneshot(
            Request::builder()
                .uri("/.well-known/host-meta")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/xrd+xml; charset=utf-8"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <XRD xmlns=\"http://docs.oasis-open.org/ns/xri/xrd-1.0\">\n  \
         <Link rel=\"lrdd\" template=\"https://plamenu.test/.well-known/webfinger?resource={uri}\"/>\n\
         </XRD>"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn host_meta_answers_json_for_json_accept_and_extension(pool: PgPool) {
    let app = test_app(pool);
    for (uri, accept) in [
        ("/.well-known/host-meta", Some("application/json")),
        ("/.well-known/host-meta.json", None),
    ] {
        let (status, content_type, body) = get(app.clone(), uri, accept).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_type, "application/json", "{uri}");
        assert_eq!(body["links"][0]["rel"], "lrdd", "{uri}");
        assert_eq!(
            body["links"][0]["template"],
            "https://plamenu.test/.well-known/webfinger?resource={uri}"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn actor_serves_html_to_browsers_and_ap_to_clients(pool: PgPool) {
    create_alice(&pool).await;
    // A browser (HTML/no Accept) gets the human profile page, like Mastodon.
    let (status, content_type, body) =
        get(test_app(pool.clone()), "/users/alice", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert_eq!(body, Value::Null, "HTML page is not JSON");

    let (status, content_type, _) = get(test_app(pool.clone()), "/users/alice", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");

    // An ActivityPub client gets the full actor document.
    let (status, content_type, body) = get(
        test_app(pool),
        "/users/alice",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/activity+json; charset=utf-8");
    assert_eq!(body["type"], "Person");
    assert_eq!(body["id"], "https://plamenu.test/users/alice");
    assert_eq!(body["url"], "https://plamenu.test/@alice");
    assert_eq!(body["preferredUsername"], "alice");
    assert_eq!(body["name"], "Alice");
    assert_eq!(body["inbox"], "https://plamenu.test/users/alice/inbox");
    assert_eq!(
        body["endpoints"]["sharedInbox"],
        "https://plamenu.test/inbox"
    );
    assert_eq!(
        body["publicKey"]["id"],
        "https://plamenu.test/users/alice#main-key"
    );
    let pem = body["publicKey"]["publicKeyPem"].as_str().unwrap();
    assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----"));
    // `published` must be RFC 3339.
    let published = body["published"].as_str().unwrap();
    assert!(
        time::OffsetDateTime::parse(published, &time::format_description::well_known::Rfc3339)
            .is_ok(),
        "published not RFC 3339: {published}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unknown_actor_is_404(pool: PgPool) {
    let (status, _, _) = get(
        test_app(pool),
        "/users/ghost",
        Some("application/activity+json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn nodeinfo_is_discoverable_and_counts_users(pool: PgPool) {
    create_alice(&pool).await;
    let (status, _, index) = get(test_app(pool.clone()), "/.well-known/nodeinfo", None).await;
    assert_eq!(status, StatusCode::OK);
    let href = index["links"][1]["href"].as_str().unwrap();
    assert_eq!(href, "https://plamenu.test/nodeinfo/2.1");

    let (status, _, body) = get(test_app(pool.clone()), "/nodeinfo/2.1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["software"]["name"], "plamenu");
    // The 2.1 `repository` field stays omitted until a public source URL
    // exists — never a placeholder link.
    assert!(
        !body["software"]
            .as_object()
            .unwrap()
            .contains_key("repository")
    );
    assert_eq!(body["protocols"][0], "activitypub");
    assert_eq!(body["usage"]["users"]["total"], 1);
    assert_eq!(body["usage"]["users"]["activeMonth"], 0);
    assert_eq!(body["usage"]["users"]["activeHalfyear"], 0);
    assert_eq!(body["usage"]["localPosts"], 0);
    // Fresh installs default to closed registrations.
    assert_eq!(body["openRegistrations"], false);
    // FEP-0151-style instance identity keys crawlers read.
    assert!(body["metadata"]["nodeName"].is_string());
    assert!(body["metadata"]["nodeDescription"].is_string());

    let (status, _, body) = get(test_app(pool), "/nodeinfo/2.0", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], "2.0");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_v1_has_mastodon_shape(pool: PgPool) {
    create_alice(&pool).await;
    let (status, _, body) = get(test_app(pool), "/api/v1/instance", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["uri"], TEST_DOMAIN);
    assert_eq!(body["stats"]["user_count"], 1);
    let version = body["version"].as_str().unwrap();
    assert!(
        version.starts_with("4.7.1 (compatible; Plamenu "),
        "advertised Mastodon API level changed: {version}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_v2_has_mastodon_shape(pool: PgPool) {
    let (status, _, body) = get(test_app(pool), "/api/v2/instance", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["domain"], TEST_DOMAIN);
    assert_eq!(
        body["version"],
        format!("4.7.1 (compatible; Plamenu {})", plamenu::VERSION)
    );
    assert_eq!(body["registrations"]["enabled"], false);
    assert!(body["configuration"]["statuses"]["max_characters"].is_u64());
    // Clients feature-detect grouped notifications & co. from this marker.
    assert_eq!(body["api_versions"]["mastodon"], 11);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_config_endpoints_serve_empty_states(pool: PgPool) {
    let (status, _, body) = get(test_app(pool.clone()), "/api/v1/instance/rules", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Array(vec![]));

    for uri in [
        "/api/v1/instance/extended_description",
        "/api/v1/instance/privacy_policy",
    ] {
        let (status, _, body) = get(test_app(pool.clone()), uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["content"], "", "{uri}");
        assert!(body["updated_at"].is_null(), "{uri}");
    }

    // No terms of service published: 404 with Mastodon's wording, like its
    // own controller raising RecordNotFound.
    for uri in [
        "/api/v1/instance/terms_of_service",
        "/api/v1/instance/terms_of_service/2026-01-01",
    ] {
        let (status, _, body) = get(test_app(pool.clone()), uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["error"], "Record not found", "{uri}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_rules_serve_published_rules(pool: PgPool) {
    // Create out of priority order to prove display ordering.
    plamenu_db::rule::create(&pool, "Be kind", "", Some(1))
        .await
        .unwrap();
    plamenu_db::rule::create(&pool, "No spam", "Stay relevant", Some(0))
        .await
        .unwrap();

    let (status, _, rules) = get(test_app(pool.clone()), "/api/v1/instance/rules", None).await;
    assert_eq!(status, StatusCode::OK);
    let rules = rules.as_array().unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0]["text"], "No spam");
    assert_eq!(rules[0]["hint"], "Stay relevant");
    assert!(rules[0]["id"].is_string());
    assert_eq!(rules[0]["translations"], serde_json::json!({}));
    assert_eq!(rules[1]["text"], "Be kind");

    // The same list is embedded in both instance entities.
    for uri in ["/api/v1/instance", "/api/v2/instance"] {
        let (_, _, body) = get(test_app(pool.clone()), uri, None).await;
        assert_eq!(body["rules"].as_array().unwrap().len(), 2, "{uri}");
        assert_eq!(body["rules"][0]["text"], "No spam", "{uri}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_entities_reflect_operator_settings(pool: PgPool) {
    create_alice(&pool).await;
    let current = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            site_title: "Testburg",
            site_short_description: "A cozy test box",
            site_extended_description: "## Welcome\n\nBe kind.",
            site_contact_username: "alice",
            site_contact_email: "admin@testburg.example",
            custom_css: "",
            registrations_mode: plamenu_db::instance_settings::RegistrationsMode::None,
            ..current.as_update()
        },
    )
    .await
    .unwrap();

    let (status, _, body) = get(test_app(pool.clone()), "/api/v1/instance", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Testburg");
    assert_eq!(body["short_description"], "A cozy test box");
    assert_eq!(body["email"], "admin@testburg.example");
    assert_eq!(body["contact_account"]["username"], "alice");

    let (status, _, body) = get(test_app(pool.clone()), "/api/v2/instance", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Testburg");
    assert_eq!(body["description"], "A cozy test box");
    assert_eq!(body["contact"]["email"], "admin@testburg.example");
    assert_eq!(body["contact"]["account"]["username"], "alice");

    // The extended description is served as sanitized HTML rendered from the
    // operator's Markdown, stamped with the settings row's update time.
    let (status, _, body) = get(
        test_app(pool.clone()),
        "/api/v1/instance/extended_description",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let content = body["content"].as_str().unwrap();
    assert!(content.contains("<h2>Welcome</h2>"), "content: {content}");
    assert!(content.contains("<p>Be kind.</p>"), "content: {content}");
    assert!(body["updated_at"].is_string());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_serves_site_upload_thumbnail_and_icon(pool: PgPool) {
    // Without uploads: v1 thumbnail is null, v2 keeps the static fallback and
    // omits `icon` (Mastodon falls back to bundled frontend assets there).
    let app = test_app(pool.clone());
    let (_, _, body) = get(app.clone(), "/api/v1/instance", None).await;
    assert!(body["thumbnail"].is_null());
    let (_, _, body) = get(app.clone(), "/api/v2/instance", None).await;
    let fallback_url = body["thumbnail"]["url"].as_str().unwrap();
    assert_eq!(fallback_url, format!("https://{TEST_DOMAIN}/thumbnail.png"));
    let fallback_path = fallback_url
        .strip_prefix(&format!("https://{TEST_DOMAIN}"))
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(fallback_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!((image.width(), image.height()), (512, 512));
    assert!(body.get("icon").is_none());

    plamenu_db::site_upload::upsert(
        &pool,
        "thumbnail",
        "100.png",
        "image/png",
        10,
        1400,
        700,
        Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH"),
        &[
            plamenu_db::site_upload::NewVariant {
                style: "@1x".into(),
                file_name: "101.png".into(),
                width: 1200,
                height: 630,
            },
            plamenu_db::site_upload::NewVariant {
                style: "@2x".into(),
                file_name: "102.png".into(),
                width: 2400,
                height: 1260,
            },
        ],
    )
    .await
    .unwrap();
    plamenu_db::site_upload::set_description(&pool, "thumbnail", "Our island")
        .await
        .unwrap();
    plamenu_db::site_upload::upsert(
        &pool,
        "app_icon",
        "200.png",
        "image/png",
        10,
        512,
        512,
        None,
        &[plamenu_db::site_upload::NewVariant {
            style: "192".into(),
            file_name: "201.png".into(),
            width: 192,
            height: 192,
        }],
    )
    .await
    .unwrap();

    let (_, _, body) = get(test_app(pool.clone()), "/api/v1/instance", None).await;
    assert_eq!(
        body["thumbnail"],
        format!("https://{TEST_DOMAIN}/media/101.png")
    );

    let (_, _, body) = get(test_app(pool.clone()), "/api/v2/instance", None).await;
    assert_eq!(
        body["thumbnail"]["url"],
        format!("https://{TEST_DOMAIN}/media/101.png")
    );
    assert_eq!(
        body["thumbnail"]["versions"]["@2x"],
        format!("https://{TEST_DOMAIN}/media/102.png")
    );
    assert_eq!(
        body["thumbnail"]["blurhash"],
        "LKO2?U%2Tw=w]~RBVZRi};RPxuwH"
    );
    assert_eq!(body["thumbnail"]["description"], "Our island");
    assert_eq!(body["icon"][0]["size"], "192x192");
    assert_eq!(
        body["icon"][0]["src"],
        format!("https://{TEST_DOMAIN}/media/201.png")
    );
}

/// Sends `method uri` with no credentials and returns the status.
async fn method_probe(app: Router, method: &str, uri: &str) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Every endpoint Mastodon declares as a Rails `:update` answers PUT *and*
/// PATCH; Rails generates both, and clients are split on which they send —
/// masto.js (Phanpy, Elk) sends PUT for every `update` action except
/// `update_credentials`, which it special-cases to PATCH.
///
/// Registering only one of the pair is invisible until a client picks the
/// other and gets a bare 405 with no body: that is exactly how Phanpy's
/// "Edit profile" broke. Method routing is resolved before any extractor, so
/// an unauthenticated probe still distinguishes "method not registered" (405)
/// from "registered, needs a token" (401) — no fixtures required.
#[sqlx::test(migrations = "../db/migrations")]
async fn update_routes_answer_both_put_and_patch(pool: PgPool) {
    let shared = test_app(pool);
    // Rails `resource(s) ... :update` declarations in Mastodon's routes/api.rb.
    let updatable = [
        "/api/v1/profile",
        "/api/v1/statuses/1",
        "/api/v1/statuses/1/interaction_policy",
        "/api/v1/collections/1",
        "/api/v1/notifications/policy",
        "/api/v2/notifications/policy",
        "/api/v1/lists/1",
        "/api/v1/filters/1",
        "/api/v2/filters/1",
        "/api/v2/filters/keywords/1",
        "/api/v1/scheduled_statuses/1",
        "/api/v1/media/1",
        "/api/v1/push/subscription",
        "/api/v1/admin/domain_blocks/1",
        "/api/v1/admin/ip_blocks/1",
        "/api/v1/admin/reports/1",
        "/api/v1/admin/tags/1",
    ];
    for uri in updatable {
        for method in ["PUT", "PATCH"] {
            let status = method_probe(shared.clone(), method, uri).await;
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri} is not registered"
            );
        }
    }
    // `update_credentials` is the one exception: Mastodon declares it as a
    // bare `patch`, so PUT is a 405 there too and clients special-case it.
    assert_eq!(
        method_probe(
            shared.clone(),
            "PATCH",
            "/api/v1/accounts/update_credentials"
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

/// RFC 8414 authorization-server metadata. Clients read it before they hold a
/// token to decide how to authorize — Phanpy enables PKCE only when
/// `code_challenge_methods_supported` names S256, so an absent document
/// silently downgrades every login to the plain code flow.
#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_authorization_server_metadata(pool: PgPool) {
    let (status, _, body) = get(
        test_app(pool),
        "/.well-known/oauth-authorization-server",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["issuer"], format!("https://{TEST_DOMAIN}/"));
    assert_eq!(
        body["authorization_endpoint"],
        format!("https://{TEST_DOMAIN}/oauth/authorize")
    );
    assert_eq!(
        body["token_endpoint"],
        format!("https://{TEST_DOMAIN}/oauth/token")
    );
    assert_eq!(
        body["revocation_endpoint"],
        format!("https://{TEST_DOMAIN}/oauth/revoke")
    );
    assert_eq!(
        body["app_registration_endpoint"],
        format!("https://{TEST_DOMAIN}/api/v1/apps")
    );
    assert_eq!(
        body["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    // Only what this server implements: no refresh-token or implicit grant,
    // and client credentials travel in the request body.
    assert_eq!(
        body["grant_types_supported"],
        serde_json::json!(["authorization_code", "client_credentials"])
    );
    assert_eq!(
        body["token_endpoint_auth_methods_supported"],
        serde_json::json!(["client_secret_post"])
    );
    let scopes = body["scopes_supported"].as_array().unwrap();
    for expected in ["read", "write", "follow", "push", "profile"] {
        assert!(
            scopes.iter().any(|s| s == expected),
            "{expected} missing from scopes_supported"
        );
    }
}

/// `configuration` carries the limits clients size their composer and profile
/// editor from. `supported_mime_types` in particular must never be empty:
/// clients read an empty list as "nothing is allowed" rather than "unknown",
/// which made Phanpy reject every file before it reached the upload endpoint.
#[sqlx::test(migrations = "../db/migrations")]
async fn instance_configuration_advertises_client_limits(pool: PgPool) {
    for uri in ["/api/v1/instance", "/api/v2/instance"] {
        let (status, _, body) = get(test_app(pool.clone()), uri, None).await;
        assert_eq!(status, StatusCode::OK);
        let config = &body["configuration"];
        assert_eq!(config["statuses"]["max_characters"], 5000);

        let mime_types = config["media_attachments"]["supported_mime_types"]
            .as_array()
            .unwrap_or_else(|| panic!("{uri} has no supported_mime_types"));
        assert!(!mime_types.is_empty(), "{uri} advertises no media types");
        for expected in ["image/jpeg", "image/png", "image/gif", "video/mp4"] {
            assert!(
                mime_types.iter().any(|m| m == expected),
                "{uri} omits {expected}"
            );
        }
        assert!(config["media_attachments"]["description_limit"].is_u64());

        let accounts = &config["accounts"];
        for key in [
            "max_display_name_length",
            "max_note_length",
            "max_avatar_description_length",
            "max_header_description_length",
            "max_featured_tags",
            "max_pinned_statuses",
            "max_profile_fields",
            "profile_field_name_limit",
            "profile_field_value_limit",
        ] {
            assert!(accounts[key].is_u64(), "{uri} accounts.{key} missing");
        }
    }

    // `timelines_access` is a v2-only addition; it reports what an anonymous
    // request actually gets, so clients can stop offering a feed that 401s.
    let (_, _, body) = get(test_app(pool), "/api/v2/instance", None).await;
    let access = &body["configuration"]["timelines_access"];
    for feed in ["live_feeds", "hashtag_feeds", "trending_link_feeds"] {
        for scope in ["local", "remote"] {
            let value = access[feed][scope].as_str().unwrap_or_else(|| {
                panic!("timelines_access.{feed}.{scope} missing");
            });
            assert!(
                matches!(value, "public" | "authenticated" | "disabled"),
                "timelines_access.{feed}.{scope} = {value}"
            );
        }
    }
}
