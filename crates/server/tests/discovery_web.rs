//! A zero-follow member's discovery journey through the real web router.
mod common;
#[path = "discovery_web/topics.rs"]
mod topics;
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{AppState, discovery::browse};
use plamenu_db::{PgPool, discovery_store as db};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    stub: Arc<StubFederation>,
    viewer: i64,
    user: i64,
    token: String,
    source: i64,
    entry: i64,
}
impl Fixture {
    fn csrf(&self) -> String {
        plamenu::auth::hash_secret(&format!("plamenu-csrf:{}", self.token))
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        fields: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let response = plamenu::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", format!("__Host-plamenu_session={}", self.token))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        (status, headers, body)
    }
    async fn get(&self, path: &str) -> String {
        let (code, _, body) = self.request("GET", path, &[]).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        body
    }
}
async fn source(pool: &PgPool, domain: &str) -> i64 {
    let db::Admission::Added(id) =
        db::add_source(pool, &format!("https://{domain}/api/v1/directory"), None)
            .await
            .unwrap()
    else {
        panic!("source");
    };
    sqlx::query("UPDATE discovery_sources SET canonical_uri=endpoint,capabilities=ARRAY['directory'],verified_at=now(),valid_until=now()+interval '1 day',last_success_at=now() WHERE id=$1").bind(id).execute(pool).await.unwrap();
    id
}
async fn entry(pool: &PgPool, source: i64, domain: &str, name: &str) -> i64 {
    let id = plamenu_db::id::next();
    let uri = format!("https://{domain}/users/{name}");
    sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,actor_uri,source_id,kind,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until) VALUES($1,$2,$2,$3,'profile',1,'missing','deny','public',$4,$5,now(),now()+interval '1 day')")
        .bind(id).bind(uri).bind(source).bind(format!("{name} <name>"))
        .bind("Hello <img src=https://evil.example/track> & friends")
        .execute(pool).await.unwrap();
    id
}
async fn fixture(pool: &PgPool) -> Fixture {
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());
    let account = create_local_account(pool, "member", "Member").await;
    let user =
        plamenu_db::user::set_credentials(pool, account.id, Some("member@example.test"), "unused")
            .await
            .unwrap();
    let app = plamenu::web::session::ensure_web_app(&state).await.unwrap();
    let token = format!("discovery-test-{}", plamenu_db::id::next());
    plamenu_db::oauth::create_token(
        pool,
        &plamenu::auth::hash_secret(&token),
        app.id,
        Some(user.id),
        "read write follow push",
    )
    .await
    .unwrap();
    let mut settings = db::settings(pool).await.unwrap();
    settings.enabled = true;
    db::save_settings(pool, &settings).await.unwrap();
    let source = source(pool, "one.example").await;
    let entry = entry(pool, source, "one.example", "alice").await;
    let remote = RemoteUser::new("one.example", "alice");
    stub.actors
        .lock()
        .unwrap()
        .insert(remote.actor.id.clone(), remote.actor.clone());
    stub.objects.lock().unwrap().insert(
        remote.actor.id.clone(),
        serde_json::json!({"id":remote.actor.id,"type":"Person","preferredUsername":"alice","inbox":remote.actor.inbox,"publicKey":{"id":remote.actor.public_key.id,"owner":remote.actor.public_key.owner,"publicKeyPem":remote.keys.public_pem}}),
    );
    Fixture {
        state,
        stub,
        viewer: account.id,
        user: user.id,
        token,
        source,
        entry,
    }
}
async fn effects(pool: &PgPool) -> Vec<i64> {
    let mut counts = Vec::new();
    // Table names are a fixed fixture-owned allowlist, never request data.
    for table in [
        "accounts",
        "statuses",
        "follows",
        "notifications",
        "delivery_jobs",
        "account_media_jobs",
        "media_processing_jobs",
        "link_crawl_jobs",
        "link_verification_jobs",
        "remote_history_jobs",
        "discovery_jobs",
        "discovery_requests",
    ] {
        counts.push(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table}"
            )))
            .fetch_one(pool)
            .await
            .unwrap(),
        );
    }
    counts
}

#[sqlx::test(migrations = "../db/migrations")]
async fn browsing_is_private_and_has_no_acquisition_or_import_side_effects(pool: PgPool) {
    let f = fixture(&pool).await;
    let before = effects(&pool).await;
    let html = f.get("/discover").await;
    assert!(html.contains("alice &lt;name&gt;"));
    assert!(html.contains("&lt;img src=https://evil.example/track&gt;"));
    assert!(!html.contains("<img src=\"https://evil.example"));
    assert!(!html.contains("has no discovery preference"));
    assert!(!html.contains("Last verified:"));
    assert!(html.contains("discovery-avatar"));
    assert!(html.contains("data-discovery-results=\"people\""));
    assert!(html.contains(&format!("/web/discover/{}/open", f.entry)));
    assert!(!html.contains(&format!("/web/accounts/{}/follow", f.entry)));
    assert!(
        f.get("/discover?sources=true")
            .await
            .contains("data-discovery-server=")
    );
    assert_eq!(effects(&pool).await, before);
    assert!(f.stub.fetches.lock().unwrap().is_empty());
    assert!(f.stub.deliveries.lock().unwrap().is_empty());
    assert!(f.stub.media_fetches.lock().unwrap().is_empty());
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri("/discover")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_redirection());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_open_requires_csrf_resolves_one_profile_and_keeps_follow_separate(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/web/discover/{}/open", f.entry);
    assert_eq!(
        f.request("POST", &path, &[("csrf", "bad")]).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(f.stub.fetches.lock().unwrap().is_empty());
    let (code, headers, body) = f.request("POST", &path, &[("csrf", &f.csrf())]).await;
    assert_eq!(code, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(headers["location"], "/@alice@one.example");
    let account = plamenu_db::account::find_by_uri(&pool, "https://one.example/users/alice")
        .await
        .unwrap()
        .unwrap();
    assert!(!f.stub.fetches.lock().unwrap().is_empty());
    let html = f.get("/@alice@one.example").await;
    assert!(html.contains(&format!("/web/accounts/{}/follow", account.id)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM follows")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(f.stub.deliveries.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn exclusions_apply_after_account_import_and_without_imported_accounts(pool: PgPool) {
    let f = fixture(&pool).await;
    let uri = "https://one.example/users/alice";
    let remote = RemoteUser::new("one.example", "alice");
    let account = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    assert!(
        db::entry(&pool, uri)
            .await
            .unwrap()
            .unwrap()
            .account_id
            .is_none()
    );
    plamenu_db::block::create(&pool, f.viewer, account.id, None)
        .await
        .unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    let before = f.stub.fetches.lock().unwrap().len();
    let result = f
        .request(
            "POST",
            &format!("/web/discover/{}/open", f.entry),
            &[("csrf", &f.csrf())],
        )
        .await;
    assert_eq!(result.1["location"], "/discover?notice=unavailable");
    assert_eq!(f.stub.fetches.lock().unwrap().len(), before);
    plamenu_db::block::delete(&pool, f.viewer, account.id)
        .await
        .unwrap();
    plamenu_db::mute::upsert(&pool, f.viewer, account.id, true, None)
        .await
        .unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    plamenu_db::mute::delete(&pool, f.viewer, account.id)
        .await
        .unwrap();
    plamenu_db::suggestion::suppress(&pool, f.viewer, account.id)
        .await
        .unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    let second = entry(&pool, f.source, "one.example", "unknown").await;
    assert!(
        browse::eligible(&f.state, f.viewer, second)
            .await
            .unwrap()
            .is_some()
    );
    plamenu_db::account_domain_block::create(&pool, f.viewer, "one.example")
        .await
        .unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    assert!(
        !f.get("/discover?sources=true")
            .await
            .contains("data-discovery-server=")
    );
    assert!(
        browse::eligible(&f.state, f.viewer, second)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dismissals_are_personal_and_do_not_import_the_hidden_profile(pool: PgPool) {
    let f = fixture(&pool).await;
    let before = effects(&pool).await;
    let path = format!("/web/discover/{}/dismiss", f.entry);
    assert_eq!(
        f.request("POST", &path, &[("csrf", "bad")]).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(
        browse::eligible(&f.state, f.viewer, f.entry)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        f.request("POST", &path, &[("csrf", &f.csrf())]).await.0,
        StatusCode::SEE_OTHER
    );
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    let other = create_local_account(&pool, "other", "Other").await;
    assert!(
        browse::eligible(&f.state, other.id, f.entry)
            .await
            .unwrap()
            .is_some()
    );
    let after = effects(&pool).await;
    assert_eq!(&after[1..], &before[1..]);
    assert!(f.stub.fetches.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pagination_is_stable_diverse_and_ignores_new_arrivals_until_restart(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = source(&pool, "two.example").await;
    for n in 0..8 {
        entry(&pool, f.source, "one.example", &format!("a{n}")).await;
        entry(&pool, second, "two.example", &format!("b{n}")).await;
    }
    let first = browse::page(&f.state, f.viewer, None, None).await.unwrap();
    assert_eq!(first.entries.len(), 8);
    let first_ids: Vec<_> = first.entries.iter().map(|e| e.id).collect();
    let cursor = first.next.unwrap();
    let late = entry(&pool, second, "two.example", "late").await;
    db::dismiss(&pool, f.viewer, first_ids[0]).await.unwrap();
    let second = browse::page(&f.state, f.viewer, Some(cursor), None)
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 8);
    assert!(
        second
            .entries
            .iter()
            .all(|e| !first_ids.contains(&e.id) && e.id != late)
    );
    let third = browse::page(&f.state, f.viewer, second.next, None)
        .await
        .unwrap();
    assert_eq!(third.entries.len(), 1);
    assert!(third.next.is_none());
    assert_eq!(
        f.request("GET", "/discover?watermark=1", &[]).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn disabled_paused_expired_and_tightened_policy_are_applied_on_each_read(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut s = db::settings(&pool).await.unwrap();
    s.paused = true;
    db::save_settings(&pool, &s).await.unwrap();
    let page = f.get("/discover").await;
    assert!(page.contains("discovery-paused"));
    assert!(page.contains("data-discovery-card="));
    s = db::settings(&pool).await.unwrap();
    s.paused = false;
    s.disabled_mechanisms.push("metadata_cache".into());
    db::save_settings(&pool, &s).await.unwrap();
    let page = f.get("/discover").await;
    assert!(page.contains("discovery-methods-disabled"));
    assert!(page.contains("data-discovery-card="));
    s = db::settings(&pool).await.unwrap();
    s.allow_missing_discoverable = false;
    db::save_settings(&pool, &s).await.unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
    s = db::settings(&pool).await.unwrap();
    s.allow_missing_discoverable = true;
    s.enabled = false;
    db::save_settings(&pool, &s).await.unwrap();
    let page = f.get("/discover").await;
    assert!(page.contains("discovery-disabled"));
    assert!(!page.contains("data-discovery-card="));
    s = db::settings(&pool).await.unwrap();
    s.enabled = true;
    db::save_settings(&pool, &s).await.unwrap();
    sqlx::query("UPDATE discovery_entries SET verified_at=now()-interval '2 days',valid_until=now()-interval '1 day'").execute(&pool).await.unwrap();
    assert!(!f.get("/discover").await.contains("data-discovery-card="));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn operator_controls_require_permission_csrf_and_current_revision(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        f.request("GET", "/admin/discovery", &[]).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET role_id=2 WHERE id=$1")
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    let before = effects(&pool).await;
    let html = f.get("/admin/discovery").await;
    assert!(html.contains("Add source"));
    assert_eq!(effects(&pool).await, before);
    assert_eq!(
        f.request(
            "POST",
            "/web/admin/discovery/sources",
            &[("csrf", "bad"), ("url", "https://new.example")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let result = f
        .request(
            "POST",
            "/web/admin/discovery/sources",
            &[("csrf", &f.csrf()), ("url", "https://new.example")],
        )
        .await;
    assert_eq!(result.1["location"], "/admin/discovery?notice=queued");
    assert!(f.stub.fetches.lock().unwrap().is_empty());
    let revision = db::settings(&pool).await.unwrap().revision.to_string();
    let result = f
        .request(
            "POST",
            "/web/admin/discovery/settings",
            &[
                ("csrf", &f.csrf()),
                ("revision", &revision),
                ("enabled", "1"),
                ("paused", "1"),
                ("missing_indexable", "1"),
                ("profile_listing", "1"),
            ],
        )
        .await;
    assert_eq!(result.1["location"], "/admin/discovery?notice=saved");
    let s = db::settings(&pool).await.unwrap();
    assert!(s.enabled && s.paused);
    assert!(!s.allow_missing_discoverable);
    assert!(s.allow_missing_indexable);
    assert!(s.disabled_mechanisms.contains(&"directory".into()));
    let result = f
        .request(
            "POST",
            "/web/admin/discovery/settings",
            &[("csrf", &f.csrf()), ("revision", &revision)],
        )
        .await;
    assert_eq!(result.1["location"], "/admin/discovery?notice=conflict");
    assert!(db::settings(&pool).await.unwrap().enabled);
    f.request(
        "POST",
        &format!("/web/admin/discovery/sources/{}", f.source),
        &[("csrf", &f.csrf()), ("enabled", "false")],
    )
    .await;
    assert!(!db::source(&pool, f.source).await.unwrap().unwrap().enabled);
}

async fn seed_browser_posts(pool: &PgPool) {
    let actors:Vec<(i64,String)>=sqlx::query_as("SELECT source_id,canonical_uri FROM discovery_entries WHERE kind='profile' ORDER BY id LIMIT 3").fetch_all(pool).await.unwrap();
    for (source, actor) in actors {
        sqlx::query("UPDATE discovery_entries SET indexable='allow' WHERE canonical_uri=$1")
            .bind(&actor)
            .execute(pool)
            .await
            .unwrap();
        for n in 0..3 {
            sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,actor_uri,source_id,kind,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until,published_at,sensitive) VALUES($1,$2,$3,$4,'post',1,'missing','allow','public',$5,'Garden orchids and small creative experiments. A public post excerpt.',now(),now()+interval '1 day',now()-interval '1 hour',$6)")
                .bind(plamenu_db::id::next()).bind(format!("{actor}/posts/{n}")).bind(&actor).bind(source)
                .bind(if n==0 {"Content warning"} else {""}).bind(n==0).execute(pool).await.unwrap();
        }
    }
}

async fn seed_browser_communities(pool: &PgPool) {
    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,origin FROM discovery_sources ORDER BY id LIMIT 2")
            .fetch_all(pool)
            .await
            .unwrap();
    for n in 0..9 {
        let (source, origin) = &sources[n % 2];
        let uri = format!("{origin}/c/garden{n}");
        sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,actor_uri,source_id,kind,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until,sensitive) VALUES($1,$2,$2,$3,'community',1,'missing','deny','public',$4,'A community for growing things and sharing ideas.',now(),now()+interval '1 day',$5)")
            .bind(plamenu_db::id::next()).bind(uri).bind(source).bind(format!("Garden community {n}")).bind(n==0).execute(pool).await.unwrap();
    }
}

async fn seed_browser_videos(pool: &PgPool) {
    let sources: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,origin FROM discovery_sources ORDER BY id LIMIT 2")
            .fetch_all(pool)
            .await
            .unwrap();
    for n in 0..9 {
        let (source, origin) = &sources[n % 2];
        let id = plamenu_db::id::next();
        let uuid = format!("00000000-0000-4000-8000-{n:012x}");
        let owner = format!("{origin}/accounts/camera");
        let channel = format!("{origin}/video-channels/garden{}", n / 4);
        sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,actor_uri,source_id,kind,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until,published_at,sensitive) VALUES($1,$2,$3,$4,'video',1,'missing','missing','public',$5,'Growing orchids, a short film about a small garden.',now(),now()+interval '1 day',now()-interval '1 hour',$6)").bind(id).bind(format!("{origin}/videos/watch/{uuid}")).bind(&owner).bind(source).bind(format!("Garden film {n}")).bind(n==0).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO discovery_video_details(entry_id,uuid,owner_uri,channel_uri,channel_title,duration) VALUES($1,$2,$3,$4,'Garden films',125)").bind(id).bind(uuid).bind(owner).bind(channel).execute(pool).await.unwrap();
    }
}

#[sqlx::test(migrations = "../db/migrations")]
#[ignore = "opt-in local Chromium, keyboard/mobile/no-JavaScript and accessibility journey"]
async fn browser_discovery_journey(pool: PgPool) {
    use std::io::Write;
    let f = fixture(&pool).await;
    let second = source(&pool, "two.example").await;
    for n in 0..5 {
        entry(&pool, f.source, "one.example", &format!("a{n}")).await;
        entry(&pool, second, "two.example", &format!("b{n}")).await;
    }
    sqlx::query("UPDATE users SET role_id=2 WHERE id=$1")
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    seed_browser_posts(&pool).await;
    seed_browser_communities(&pool).await;
    seed_browser_videos(&pool).await;
    // Render the populated operator cost table, including its narrow layout.
    for family in ["profile", "community", "video"] {
        sqlx::query("INSERT INTO discovery_requests(id,origin,family,operation,bytes,completed,active_until) VALUES($1,'https://one.example',$2,'refresh_entry',123456,true,now())")
            .bind(plamenu_db::id::next()).bind(family).execute(&pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO discovery_topics(entry_id,topic) SELECT id,'garden' FROM discovery_entries",
    )
    .execute(&pool)
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let inspector_pool = pool.clone();
    let app = plamenu::build_router(f.state.clone()).route(
        "/_fixture/discovery-effects",
        axum::routing::get(move || {
            let pool = inspector_pool.clone();
            async move { axum::Json(effects(&pool).await) }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let before = effects(&pool).await;
    let config = serde_json::json!({"url":format!("http://{address}"),"token":f.token,"entry":f.entry.to_string()});
    let result = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("node")
            .arg("../../accessibility/discovery-journey.mjs")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config.to_string().as_bytes())
            .unwrap();
        child.wait().unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(result.success());
    let after = effects(&pool).await;
    assert_eq!(after[0], before[0] + 1);
    assert_eq!(&after[1..5], &before[1..5]);
    assert!(!f.stub.fetches.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pending_budget_failure_and_localized_states_are_reported_without_fetching(pool: PgPool) {
    let f = fixture(&pool).await;
    db::enqueue(
        &pool,
        f.source,
        "https://one.example/api/v1/directory",
        db::Operation::InspectSource,
        None,
    )
    .await
    .unwrap();
    assert!(
        f.get("/discover")
            .await
            .contains("data-discovery-state=\"discovery-loading\"")
    );
    sqlx::query("UPDATE discovery_jobs SET last_error='budget_exhausted'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.get("/discover")
            .await
            .contains("data-discovery-state=\"discovery-budget\"")
    );
    sqlx::query("DELETE FROM discovery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE discovery_sources SET last_failure_at=now(),last_success_at=NULL,last_error='access_denied'").execute(&pool).await.unwrap();
    assert!(
        f.get("/discover")
            .await
            .contains("data-discovery-state=\"discovery-unsupported\"")
    );
    plamenu_db::user::update_locale(&pool, f.user, Some("ru"))
        .await
        .unwrap();
    let (code, headers, html) = f.request("GET", "/discover", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert!(html.contains("Открытия"));
    assert!(html.contains("Открыть профиль"));
    assert_eq!(headers["cache-control"], "private, no-store");
    assert!(f.stub.fetches.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_transfers_are_operator_only_and_import_no_trust(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        f.request("GET", "/admin/discovery/export", &[]).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.request(
            "POST",
            "/web/admin/discovery/bootstrap",
            &[("csrf", &f.csrf())]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET role_id=2 WHERE id=$1")
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    let raw = r#"{"version":1,"sources":[{"origin":"https://import.example","verified":true,"capabilities":["directory"]}]}"#;
    assert_eq!(
        f.request(
            "POST",
            "/web/admin/discovery/import",
            &[("csrf", "bad"), ("manifest", raw)]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let result = f
        .request(
            "POST",
            "/web/admin/discovery/import",
            &[("csrf", &f.csrf()), ("manifest", raw)],
        )
        .await;
    assert_eq!(result.0, StatusCode::SEE_OTHER);
    assert!(
        result.1["location"]
            .to_str()
            .unwrap()
            .contains("admitted=1")
    );
    let source = db::source_diagnostics(&pool)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.origin == "https://import.example")
        .unwrap();
    assert!(source.verified_at.is_none() && source.capabilities.is_empty());
    assert_eq!(source.provenance, "import");
    assert!(f.stub.fetches.lock().unwrap().is_empty());
    let exported = f.get("/admin/discovery/export").await;
    assert!(
        plamenu::discovery::seeds::parse(&exported)
            .unwrap()
            .iter()
            .any(|s| s.starts_with("https://import.example/"))
    );
    for private in ["enabled", "introduced_by", "last_error", "policy_revision"] {
        assert!(!exported.contains(private));
    }
    assert!(
        f.get("/admin/discovery")
            .await
            .contains("Import source list")
    );
}
