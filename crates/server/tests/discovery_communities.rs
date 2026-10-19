//! Lemmy 0.19.19 public community discovery through guarded HTTP and real routes.
mod common;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use plamenu::{
    AppState,
    discovery::{browse, communities, directory, profiles, worker},
    federation::HttpFederation,
};
use plamenu_db::{PgPool, discovery_store as db};
use plamenu_federation::{FederationClient, ProxyConfig};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

const ORIGIN: &str = "http://communities-test.onion";
fn actor(name: &str) -> Value {
    json!({"type":"Group","id":format!("{ORIGIN}/c/{name}"),"preferredUsername":name,
        "name":format!("{name} community"),"summary":"<p>Canonical gardening community</p>",
        "inbox":format!("{ORIGIN}/c/{name}/inbox"),"outbox":format!("{ORIGIN}/c/{name}/outbox"),
        "tag":[{"type":"Hashtag","name":"#Gardens"}],"sensitive":false,"indexable":false})
}
fn record(name: &str) -> Value {
    // Lemmy 0.19.19's Community and CommunityView wire fields; canonical AP
    // identity comes from actor_id, never a synthesized /c/<name> guess.
    json!({"community":{"id":1,"name":name,"title":"Untrusted directory title",
        "actor_id":format!("{ORIGIN}/c/{name}"),"local":true,"visibility":"Public",
        "deleted":false,"removed":false,"hidden":false,"nsfw":false}})
}
fn setup(f: &Fixture, name: &str) {
    f.set(&format!("/c/{name}"), actor(name));
    f.set(
        &format!("/api/v3/community?name={name}"),
        json!({"community_view":record(name)}),
    );
}
async fn community(f: &Fixture, name: &str) -> db::Entry {
    db::entry(&f.state.pool, &format!("{ORIGIN}/c/{name}"))
        .await
        .unwrap()
        .unwrap()
}
type Held = (String, Arc<Notify>, Arc<Semaphore>);
#[derive(Default)]
struct Peer {
    documents: Mutex<HashMap<String, (StatusCode, Value)>>,
    requests: Mutex<Vec<(String, bool)>>,
    hold: Mutex<Option<Held>>,
}
async fn respond(State(peer): State<Arc<Peer>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let target = request.uri().path_and_query().unwrap().as_str().to_owned();
    let signed = request.headers().contains_key("signature");
    assert!(!request.headers().contains_key("authorization"));
    assert!(!request.headers().contains_key("cookie"));
    peer.requests.lock().unwrap().push((target.clone(), signed));
    let (status, body) = peer
        .documents
        .lock()
        .unwrap()
        .get(&target)
        .cloned()
        .unwrap_or((StatusCode::NOT_FOUND, json!({})));
    let held = peer.hold.lock().unwrap().clone();
    if let Some((hold_path, started, release)) = held
        && path == hold_path
    {
        started.notify_one();
        let permit = release.acquire().await.unwrap();
        permit.forget();
    }
    Response::builder()
        .status(status)
        .header(
            "content-type",
            if path.starts_with("/api/") {
                "application/json"
            } else {
                "application/activity+json"
            },
        )
        .header("link", "<http://foreign.onion/next>; rel=\"next\"")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}
struct Fixture {
    state: AppState,
    peer: Arc<Peer>,
    source: i64,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    fn set(&self, path: &str, value: Value) {
        self.peer
            .documents
            .lock()
            .unwrap()
            .insert(path.into(), (StatusCode::OK, value));
    }
    fn status(&self, path: &str, status: StatusCode) {
        self.peer
            .documents
            .lock()
            .unwrap()
            .insert(path.into(), (status, json!({})));
    }
    fn page(&self, second: bool, rows: Value) {
        let uri = communities::page_uri(ORIGIN, second).unwrap();
        let url = url::Url::parse(&uri).unwrap();
        let mut document = json!({});
        document["communities"] = rows;
        self.set(
            &format!("{}?{}", url.path(), url.query().unwrap()),
            document,
        );
    }
    async fn drain(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(100);
        loop {
            let count = worker::run_due(&self.state).await.unwrap();
            let pending:i64=sqlx::query_scalar("SELECT count(*) FROM discovery_jobs WHERE NOT withdrawn AND run_at<clock_timestamp()+interval '45 seconds'").fetch_one(&self.state.pool).await.unwrap();
            if pending == 0 && count == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "discovery did not drain"
            );
            if count == 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
async fn fixture(pool: PgPool) -> Fixture {
    fixture_at(pool, communities::PATH).await
}
async fn fixture_at(pool: PgPool, path: &str) -> Fixture {
    let peer = Arc::new(Peer::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(respond).with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = common::test_config();
    let keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config).unwrap());
    let signer = common::RemoteUser::new("plamenu.test", "instance").signer();
    let client = FederationClient::new("discovery-post-test", true)
        .unwrap()
        .with_proxies(&ProxyConfig {
            onion_proxy_url: Some(proxy),
            ..ProxyConfig::default()
        })
        .unwrap()
        .with_fetch_signer(signer);
    let federation = Arc::new(HttpFederation::new(
        client,
        pool.clone(),
        config.domain.clone(),
        keyring,
    ));
    let state = AppState::new(
        pool.clone(),
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap();
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = true;
    settings.disabled_mechanisms.push("peer_exploration".into());
    db::save_settings(&pool, &settings).await.unwrap();
    let db::Admission::Added(source) = db::add_source(&pool, &format!("{ORIGIN}{path}"), None)
        .await
        .unwrap()
    else {
        panic!("new source")
    };
    Fixture {
        state,
        peer,
        source,
        server,
    }
}
async fn cold_counts(pool: &PgPool) -> Vec<i64> {
    let mut counts = Vec::new();
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
struct Session {
    token: String,
    csrf: String,
    viewer: i64,
}
async fn session(state: &AppState) -> Session {
    let viewer = common::create_local_account(&state.pool, "member", "Member").await;
    let user = plamenu_db::user::set_credentials(
        &state.pool,
        viewer.id,
        Some("member@example.test"),
        "unused",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(state).await.unwrap();
    let token = format!("posts-test-{}", plamenu_db::id::next());
    plamenu_db::oauth::create_token(
        &state.pool,
        &plamenu::auth::hash_secret(&token),
        app.id,
        Some(user.id),
        "read write follow push",
    )
    .await
    .unwrap();
    let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{token}"));
    Session {
        token,
        csrf,
        viewer: viewer.id,
    }
}
async fn request(
    state: &AppState,
    session: &Session,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(
                    "cookie",
                    format!("__Host-plamenu_session={}", session.token),
                )
                .header("authorization", format!("Bearer {}", session.token))
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

#[sqlx::test(migrations = "../db/migrations")]
async fn canonical_community_cards_are_separate_and_have_no_import_side_effects(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    let before = cold_counts(&pool).await;
    f.drain().await;
    let entry = community(&f, "garden").await;
    assert_eq!(entry.kind, "community");
    assert_eq!(
        db::entry_topics(&pool, entry.id).await.unwrap(),
        ["gardens"]
    );
    assert_eq!(entry.title.as_deref(), Some("garden community"));
    assert_eq!(entry.indexable, "deny");
    assert!(entry.outbox_uri.is_none());
    assert_eq!(cold_counts(&pool).await, before);
    assert!(
        browse::page(&f.state, member.viewer, None, None)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        browse::page_kind(&f.state, member.viewer, None, None, "community")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    let page = request(&f.state, &member, "GET", "/discover/communities", &[]).await;
    assert_eq!(page.0, StatusCode::OK);
    assert_eq!(page.1["cache-control"], "private, no-store");
    assert!(page.2.contains("Canonical gardening community"));
    assert!(page.2.contains("Open community"));
    assert!(!page.2.contains("Untrusted directory title"));
    let sources = request(&f.state, &member, "GET", "/discover?sources=true", &[]).await;
    assert!(sources.2.contains("Browse communities"));
    assert!(sources.2.contains("/discover/communities?source="));
    assert_eq!(cold_counts(&pool).await, before);
    let requests = f.peer.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|(path, signed)| if path.starts_with("/api/") {
                !*signed
            } else {
                *signed
            })
    );
    assert!(!requests.iter().any(|(path, _)| path.contains("outbox")));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn origins_probe_communities_only_after_an_unsupported_directory(pool: PgPool) {
    let f = fixture_at(pool.clone(), directory::PATH).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    f.drain().await;
    assert_eq!(community(&f, "garden").await.state, "active");
    let source = db::source(&pool, f.source).await.unwrap().unwrap();
    assert!(
        source
            .capabilities
            .contains(&"community_directory".to_owned())
    );
    assert_eq!(
        source.canonical_uri.as_deref(),
        Some(format!("{ORIGIN}{}", communities::PATH).as_str())
    );
    assert_eq!(f.peer.requests.lock().unwrap().len(), 4);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authentication_failure_does_not_probe_another_directory(pool: PgPool) {
    let f = fixture_at(pool, directory::PATH).await;
    let uri = directory::page_uri(&format!("{ORIGIN}{}", directory::PATH), 0).unwrap();
    let url = url::Url::parse(&uri).unwrap();
    f.status(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        StatusCode::FORBIDDEN,
    );
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn deletion_during_first_verification_keeps_a_guard_and_allows_explicit_recheck(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    worker::run_due(&f.state).await.unwrap();
    let uri = format!("{ORIGIN}/c/garden");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() =
        Some(("/api/v3/community".into(), started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { worker::run_due(&state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    plamenu::discovery::posts::verified_delete(&f.state, Some(&uri), &uri)
        .await
        .unwrap();
    release.add_permits(1);
    task.await.unwrap();
    assert!(db::entry(&pool, &uri).await.unwrap().is_none());
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT withdrawn FROM discovery_jobs WHERE target_uri=$1")
            .bind(&uri)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    *f.peer.hold.lock().unwrap() = None;
    db::request_profile_recheck(&pool, &uri).await.unwrap();
    f.drain().await;
    assert_eq!(community(&f, "garden").await.state, "active");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn small_community_rounds_advance_and_all_work_spends_shared_limits(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let mut settings = db::settings(&pool).await.unwrap();
    settings.requests_per_origin = 200;
    db::save_settings(&pool, &settings).await.unwrap();
    for second in [false, true] {
        let start = if second { 20 } else { 0 };
        let rows: Vec<_> = (start..start + 20)
            .map(|n| {
                let name = format!("garden{n}");
                setup(&f, &name);
                record(&name)
            })
            .collect();
        f.page(second, json!(rows));
    }
    f.drain().await;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM discovery_entries WHERE kind='community'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 3);
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 7);
    sqlx::query("UPDATE discovery_sampling SET next_at=now() WHERE source_id=$1")
        .bind(f.source)
        .execute(&pool)
        .await
        .unwrap();
    f.drain().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM discovery_entries WHERE kind='community'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        6
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 14);
    let requests = f.peer.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(p, _)| p.starts_with(communities::PATH))
            .count(),
        2
    );
    assert!(!requests.iter().any(|(p, _)| p.contains("page=3")));
}

#[test]
fn established_communities_use_active_participants_and_newcomers_have_their_own_round() {
    let uri = communities::sampling_uri(ORIGIN, db::SamplingPlan::default()).unwrap();
    assert!(uri.contains("sort=TopMonth"));
    let uri = communities::sampling_uri(
        ORIGIN,
        db::SamplingPlan {
            round: 4,
            offset: 20,
            seed: 1,
        },
    )
    .unwrap();
    assert!(uri.contains("sort=New"));
    assert!(uri.contains("page=2"));
}

#[test]
fn lemmy_01919_directory_requires_local_public_unhidden_records() {
    let upstream: Value =
        serde_json::from_str(include_str!("fixtures/lemmy/group_01919.json")).unwrap();
    assert!(communities::public_community(&upstream));
    let allowed = record("garden");
    let mut rows = vec![allowed.clone(), allowed];
    for (key, value) in [
        ("local", json!(false)),
        ("deleted", json!(true)),
        ("removed", json!(true)),
        ("hidden", json!(true)),
        ("visibility", json!("LocalOnly")),
        ("visibility", json!("Private")),
        ("visibility", Value::Null),
        ("nsfw", json!("false")),
        ("actor_id", json!("https://foreign.example/c/garden")),
    ] {
        let mut denied = record("denied");
        denied["community"][key] = value;
        rows.push(denied);
    }
    let (hints, full) = communities::parse_page(&json!({"communities":rows}), ORIGIN).unwrap();
    assert_eq!(hints, vec![format!("{ORIGIN}/c/garden")]);
    assert!(!full);
    assert!(
        communities::parse_page(&json!({"communities":vec![record("garden");21]}), ORIGIN).is_err()
    );
    assert!(communities::parse_page(&json!({"communities":[{}]}), ORIGIN).is_err());
    for key in ["hidden", "localOnly", "deleted", "removed"] {
        let mut doc = actor("garden");
        doc[key] = json!(true);
        assert!(!communities::public_community(&doc));
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn fresh_individual_visibility_and_identity_override_listing_hints(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(
        false,
        json!([
            record("private"),
            record("wrong"),
            record("person"),
            record("refused"),
            record("sensitive")
        ]),
    );
    for name in ["private", "wrong", "person", "refused", "sensitive"] {
        setup(&f, name);
    }
    let mut detail = record("private");
    detail["community"]["visibility"] = json!("Private");
    f.set(
        "/api/v3/community?name=private",
        json!({"community_view":detail}),
    );
    f.set(
        "/api/v3/community?name=wrong",
        json!({"community_view":record("somebody_else")}),
    );
    let mut doc = actor("person");
    doc["type"] = json!("Person");
    f.set("/c/person", doc);
    let mut doc = actor("refused");
    doc["discoverable"] = json!(false);
    f.set("/c/refused", doc);
    let mut detail = record("sensitive");
    detail["community"]["nsfw"] = json!(true);
    f.set(
        "/api/v3/community?name=sensitive",
        json!({"community_view":detail}),
    );
    // Feed all five cases to verification independently of sampling order.
    let names = ["private", "wrong", "person", "refused", "sensitive"];
    let uris: Vec<_> = names
        .iter()
        .map(|name| format!("{ORIGIN}/c/{name}"))
        .collect();
    let hints: Vec<_> = uris
        .iter()
        .map(|uri| db::ProfileHint {
            uri,
            canonical: true,
        })
        .collect();
    db::enqueue(
        &pool,
        f.source,
        &format!("{ORIGIN}{}", communities::PATH),
        db::Operation::InspectSource,
        None,
    )
    .await
    .unwrap();
    let lease = db::claim_due_for(&pool, "fixture", &[db::Operation::InspectSource])
        .await
        .unwrap()
        .remove(0);
    db::finish_community_page(&pool, &lease, &hints, false)
        .await
        .unwrap();
    f.drain().await;
    let page = browse::page_kind(&f.state, 1, None, None, "community")
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert!(page.entries[0].sensitive);
    assert_eq!(community(&f, "refused").await.state, "denied");
    assert_eq!(cold_counts(&pool).await, vec![0; 10]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn missing_discovery_policy_erases_community_text_until_reverified(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    f.drain().await;
    let mut settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_discoverable = false;
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(community(&f, "garden").await.summary.is_none());
    settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_discoverable = true;
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(
        browse::page_kind(&f.state, 1, None, None, "community")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    db::request_profile_recheck(&pool, &format!("{ORIGIN}/c/garden"))
        .await
        .unwrap();
    f.drain().await;
    assert!(community(&f, "garden").await.summary.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn updates_deletes_access_failures_and_inflight_results_obey_revision_fences(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    f.drain().await;
    let entry = community(&f, "garden").await;
    profiles::verified_update(&f.state, &entry.canonical_uri, &actor("garden"))
        .await
        .unwrap();
    assert!(community(&f, "garden").await.summary.is_none());
    db::request_profile_recheck(&pool, &entry.canonical_uri)
        .await
        .unwrap();
    f.drain().await;
    assert!(community(&f, "garden").await.summary.is_some());
    db::request_profile_recheck(&pool, &entry.canonical_uri)
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() =
        Some(("/api/v3/community".into(), started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { worker::run_due(&state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    plamenu::discovery::posts::verified_delete(
        &f.state,
        Some(&entry.canonical_uri),
        &entry.canonical_uri,
    )
    .await
    .unwrap();
    release.add_permits(1);
    task.await.unwrap();
    assert_eq!(community(&f, "garden").await.state, "withdrawn");
    assert!(community(&f, "garden").await.summary.is_none());
    *f.peer.hold.lock().unwrap() = None;
    db::request_profile_recheck(&pool, &entry.canonical_uri)
        .await
        .unwrap();
    f.drain().await;
    f.status("/api/v3/community?name=garden", StatusCode::FORBIDDEN);
    db::request_profile_recheck(&pool, &entry.canonical_uri)
        .await
        .unwrap();
    f.drain().await;
    assert!(community(&f, "garden").await.summary.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_switch_is_independent_and_pause_keeps_fresh_cards(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    let mut settings = db::settings(&pool).await.unwrap();
    settings
        .disabled_mechanisms
        .push("community_directory".into());
    db::save_settings(&pool, &settings).await.unwrap();
    worker::run_due(&f.state).await.unwrap();
    assert!(f.peer.requests.lock().unwrap().is_empty());
    settings = db::settings(&pool).await.unwrap();
    settings
        .disabled_mechanisms
        .retain(|m| m != "community_directory");
    settings.disabled_mechanisms.push("directory".into());
    db::save_settings(&pool, &settings).await.unwrap();
    sqlx::query("UPDATE discovery_jobs SET run_at=clock_timestamp()")
        .execute(&pool)
        .await
        .unwrap();
    f.drain().await;
    settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    db::save_settings(&pool, &settings).await.unwrap();
    assert_eq!(
        browse::page_kind(&f.state, 1, None, None, "community")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    sqlx::query("UPDATE discovery_entries SET verified_at=clock_timestamp()-interval '2 days',valid_until=clock_timestamp()-interval '1 day'").execute(&pool).await.unwrap();
    assert!(
        browse::page_kind(&f.state, 1, None, None, "community")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn explicit_open_imports_one_group_without_joining_and_dismissals_are_personal(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    let remote = common::RemoteUser::new("communities-test.onion", "garden");
    let mut doc = actor("garden");
    doc["publicKey"] = json!({"id":format!("{ORIGIN}/c/garden#main-key"),"owner":format!("{ORIGIN}/c/garden"),"publicKeyPem":remote.keys.public_pem});
    f.set("/c/garden", doc);
    f.drain().await;
    let entry = community(&f, "garden").await;
    let path = format!("/web/discover/{}/open", entry.id);
    assert_eq!(
        request(&f.state, &member, "POST", &path, &[("csrf", "bad")])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let opened = request(
        &f.state,
        &member,
        "POST",
        &path,
        &[("csrf", &member.csrf), ("view", "communities")],
    )
    .await;
    assert_eq!(opened.0, StatusCode::SEE_OTHER);
    assert!(
        opened.1["location"]
            .to_str()
            .unwrap()
            .starts_with("/!garden@communities-test.onion")
    );
    let group = plamenu_db::account::find_by_uri(&pool, &entry.canonical_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group.actor_type.as_deref(), Some("Group"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    plamenu_db::account_domain_block::create(&pool, member.viewer, "communities-test.onion")
        .await
        .unwrap();
    assert!(
        browse::eligible(&f.state, member.viewer, entry.id)
            .await
            .unwrap()
            .is_none()
    );
    plamenu_db::account_domain_block::delete(&pool, member.viewer, "communities-test.onion")
        .await
        .unwrap();
    let dismissed = request(
        &f.state,
        &member,
        "POST",
        &format!("/web/discover/{}/dismiss", entry.id),
        &[("csrf", &member.csrf), ("view", "communities")],
    )
    .await;
    assert_eq!(
        dismissed.1["location"],
        "/discover/communities?notice=hidden"
    );
    assert!(
        browse::eligible(&f.state, member.viewer, entry.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        browse::eligible(&f.state, member.viewer + 1, entry.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn changed_actor_type_cannot_open_as_a_community(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    f.page(false, json!([record("garden")]));
    setup(&f, "garden");
    f.drain().await;
    let entry = community(&f, "garden").await;
    let remote = common::RemoteUser::new("communities-test.onion", "garden");
    let mut doc = actor("garden");
    doc["type"] = json!("Person");
    doc["publicKey"] = json!({"id":format!("{ORIGIN}/c/garden#main-key"),"owner":entry.canonical_uri,"publicKeyPem":remote.keys.public_pem});
    f.set("/c/garden", doc);
    let response = request(
        &f.state,
        &member,
        "POST",
        &format!("/web/discover/{}/open", entry.id),
        &[("csrf", &member.csrf), ("view", "communities")],
    )
    .await;
    assert_eq!(response.0, StatusCode::SEE_OTHER);
    assert_eq!(
        response.1["location"],
        "/discover/communities?notice=unavailable"
    );
}
