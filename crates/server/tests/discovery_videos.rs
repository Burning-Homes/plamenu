//! `PeerTube` 8.3 catalogue contracts over the real guarded HTTP client and routes.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use plamenu::{
    AppState,
    discovery::{directory, policy::Use, posts, videos, worker},
    federation::HttpFederation,
};
use plamenu_db::{PgPool, discovery_store as db};
use plamenu_federation::{FederationClient, ProxyConfig};
use plamenu_test_support as common;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};
const ORIGIN: &str = "http://videos-test.onion";
fn uuid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}
fn uri(n: u32) -> String {
    format!("{ORIGIN}/videos/watch/{}", uuid(n))
}
fn owner() -> String {
    format!("{ORIGIN}/accounts/filmmaker")
}
fn channel(n: u32) -> String {
    format!("{ORIGIN}/video-channels/channel{}", n / 4)
}
fn record(n: u32) -> Value {
    json!({"uuid":uuid(n),"url":uri(n),"name":"Untrusted catalogue title", "privacy":{"id":1},"isLocal":true,"isLive":false,"nsfw":false,"state":{"id":1},"duration":125,"account":{"url":owner()},"channel":{"url":channel(n)}})
}
fn video(n: u32) -> Value {
    json!({"id":uri(n),"type":"Video","uuid":uuid(n),"name":format!("Garden film {n}"),"content":"<p>Planting a small garden</p>","published":"2026-01-01T12:00:00Z","to":["https://www.w3.org/ns/activitystreams#Public"],"attributedTo":[{"type":"Person","id":owner()},{"type":"Group","id":channel(n)}],"audience":channel(n),"state":1,"isLiveBroadcast":false,"sensitive":false,"tag":[{"type":"Hashtag","name":"#Gardens"}]})
}
fn actor(n: u32, group: bool) -> Value {
    let id = if group { channel(n) } else { owner() };
    let mut v = json!({"id":id,"type":if group{"Group"}else{"Person"},"preferredUsername":"filmmaker","name":if group{"Garden channel"}else{"Film maker"},"inbox":format!("{id}/inbox"),"outbox":format!("{id}/outbox")});
    if group {
        v["attributedTo"] = json!([owner()]);
    }
    v
}
fn setup(f: &Fixture, n: u32) {
    f.set(&format!("/videos/watch/{}", uuid(n)), video(n));
    f.set(&format!("/api/v1/videos/{}", uuid(n)), record(n));
    f.set("/accounts/filmmaker", actor(n, false));
    f.set(&format!("/video-channels/channel{}", n / 4), actor(n, true));
}
async fn entry(f: &Fixture, n: u32) -> db::Entry {
    db::entry(&f.state.pool, &uri(n)).await.unwrap().unwrap()
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
    fn page(&self, rows: Value) {
        let url = url::Url::parse(&videos::page_uri(ORIGIN).unwrap()).unwrap();
        let mut document = json!({"total":999});
        document["data"] = rows;
        self.set(
            &format!("{}?{}", url.path(), url.query().unwrap()),
            document,
        );
    }
    // Re-admission and lifecycle regressions cover every candidate; known
    // unchanged identities must not buy another individual verification.
    async fn admit_all_candidates(&self) {
        let url = url::Url::parse(&videos::page_uri(ORIGIN).unwrap()).unwrap();
        let document = self
            .peer
            .documents
            .lock()
            .unwrap()
            .get(&format!("{}?{}", url.path(), url.query().unwrap()))
            .unwrap()
            .1
            .clone();
        let hints = videos::parse_page(&document, ORIGIN).unwrap();
        let source = db::source(&self.state.pool, self.source)
            .await
            .unwrap()
            .unwrap();
        db::enqueue(
            &self.state.pool,
            self.source,
            &source.endpoint,
            db::Operation::InspectSource,
            None,
        )
        .await
        .unwrap();
        let lease = db::claim_due_for(&self.state.pool, "fixture", &[db::Operation::InspectSource])
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            db::finish_video_page(&self.state.pool, &lease, &hints)
                .await
                .unwrap(),
            db::Completion::Committed
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
    fixture_at(pool, videos::PATH).await
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

#[test]
fn catalogue_requires_local_public_vod_records_and_a_bounded_page() {
    let good = record(0);
    assert_eq!(
        videos::parse_page(&json!({"data":[good.clone(),good.clone()]}), ORIGIN)
            .unwrap()
            .len(),
        1
    );
    for (key, value) in [
        ("privacy", json!({"id":2})),
        ("privacy", json!({"id":3})),
        ("privacy", json!({"id":4})),
        ("privacy", json!({"id":5})),
        ("isLocal", json!(false)),
        ("isLive", json!(true)),
        ("nsfw", json!("false")),
        ("uuid", json!("not-a-uuid")),
        ("url", json!("http://other.onion/videos/watch/anything")),
        (
            "account",
            json!({"url":"http://other.onion/accounts/owner"}),
        ),
        ("channel", json!({"url":owner()})),
    ] {
        let mut row = good.clone();
        row[key] = value;
        assert!(
            videos::parse_page(&json!({"data":[row]}), ORIGIN)
                .unwrap()
                .is_empty(),
            "{key}"
        );
    }
    assert!(videos::parse_page(&json!({"data":vec![good;9]}), ORIGIN).is_err());
    assert!(videos::parse_page(&json!({"data":[null]}), ORIGIN).is_err());
    let mut detail = record(0);
    detail["state"] = json!({"id":2});
    assert!(!videos::public_record(&detail, true));
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_cards_channel_search_and_hide_have_no_import_side_effects(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    let before = cold_counts(&pool).await;
    f.drain().await;
    let e = entry(&f, 0).await;
    assert_eq!(e.kind, "video");
    assert_eq!(e.title.as_deref(), Some("Garden film 0"));
    assert_eq!(e.discoverable, "missing");
    assert_eq!(e.indexable, "missing");
    assert_eq!(db::entry_topics(&pool, e.id).await.unwrap(), ["gardens"]);
    let details = db::video_details(&pool, e.id).await.unwrap().unwrap();
    assert_eq!(details.channel_title.as_deref(), Some("Garden channel"));
    assert_eq!(details.duration, Some(125));
    for path in [
        "/discover/videos".to_owned(),
        "/discover/videos?q=planting".to_owned(),
        format!("/discover/videos?channel={}", e.id),
        format!("/discover/videos?source={}", f.source),
    ] {
        let page = request(&f.state, &member, "GET", &path, &[]).await;
        assert_eq!(page.0, StatusCode::OK);
        assert_eq!(page.1["cache-control"], "private, no-store");
        assert!(page.2.contains("Garden film 0"));
        assert!(page.2.contains("Garden channel"));
        assert!(page.2.contains("2:05"));
        assert!(page.2.contains("Open video"));
        assert!(!page.2.contains("Untrusted catalogue title"));
    }
    assert!(
        !request(&f.state, &member, "GET", "/discover/videos?q=missing", &[])
            .await
            .2
            .contains("Garden film 0")
    );
    assert!(
        request(&f.state, &member, "GET", "/discover?sources=true", &[])
            .await
            .2
            .contains("Browse videos")
    );
    let action = format!("/web/discover/{}/dismiss", e.id);
    assert_eq!(
        request(&f.state, &member, "POST", &action, &[("csrf", "bad")])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let r = request(
        &f.state,
        &member,
        "POST",
        &action,
        &[("csrf", &member.csrf), ("back", "/discover/videos")],
    )
    .await;
    assert_eq!(r.0, StatusCode::SEE_OTHER);
    assert!(
        r.1["location"]
            .to_str()
            .unwrap()
            .starts_with("/discover/videos")
    );
    assert!(
        videos::page(&f.state, member.viewer, None, None, "", "")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(cold_counts(&pool).await, before);
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 7);
    let requests = f.peer.requests.lock().unwrap();
    assert_eq!(requests.len(), 7); // catalogue + four documents + config + favicon
    assert!(requests.iter().all(|(path, signed)| {
        if path.starts_with("/api/") || path == "/favicon.ico" {
            !*signed
        } else {
            *signed
        }
    }));
    assert!(
        !requests
            .iter()
            .any(|(p, _)| p.contains("outbox") || p.contains(".mp4") || p.contains("thumbnail"))
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn fallback_and_shared_budget_stop_after_one_catalogue_page(pool: PgPool) {
    let f = fixture_at(pool.clone(), directory::PATH).await;
    for n in 0..8 {
        setup(&f, n);
    }
    f.page(json!((0..8).map(record).collect::<Vec<_>>()));
    f.drain().await;
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 24);
    assert_eq!(
        f.peer
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.contains("count=8"))
            .count(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE kind='video'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        8
    );
    // Re-admitting the unchanged page is a ledger hit, with no individual rereads.
    f.admit_all_candidates().await;
    f.drain().await;
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 24);
    let source = db::source(&pool, f.source).await.unwrap().unwrap();
    assert!(source.capabilities.contains(&"video_catalogue".into()));
    let member = session(&f.state).await;
    let first = videos::page(&f.state, member.viewer, None, None, "Garden", "")
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 4);
    let next = videos::page(&f.state, member.viewer, first.next, None, "Garden", "")
        .await
        .unwrap();
    assert_eq!(next.entries.len(), 4);
    assert!(
        first
            .entries
            .iter()
            .all(|a| next.entries.iter().all(|b| a.id != b.id))
    );
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = videos::page(&f.state, member.viewer, cursor, None, "", "")
            .await
            .unwrap();
        assert_eq!(
            page.entries.len(),
            1,
            "one film per author per ordinary page"
        );
        assert!(seen.insert(page.entries[0].id));
        if seen.len() == 1 {
            sqlx::query("DELETE FROM discovery_entries WHERE id=$1")
                .bind(page.entries[0].id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let Some(next) = page.next else { break };
        cursor = Some(next);
        assert!(seen.len() < 9, "continuation makes progress");
    }
    assert_eq!(seen.len(), 8, "deletion does not move later author slots");
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn owner_and_channel_flags_gate_indexing_and_recommendation(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    let mut channel = actor(0, true);
    channel["discoverable"] = json!(false);
    f.set("/video-channels/channel0", channel);
    f.drain().await;
    let e = entry(&f, 0).await;
    assert_eq!(e.state, "active");
    assert_eq!(e.discoverable, "deny");
    assert!(
        videos::eligible(&f.state, member.viewer, e.id, Use::Recommend)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        videos::eligible(&f.state, member.viewer, e.id, Use::IndexVideo)
            .await
            .unwrap()
            .is_some()
    );
    let page = request(&f.state, &member, "GET", "/discover/videos?q=garden", &[])
        .await
        .2;
    assert!(page.contains("Garden film 0"));
    assert!(!page.contains("Garden channel"));
    let mut owner = actor(0, false);
    owner["indexable"] = json!(false);
    f.set("/accounts/filmmaker", owner);
    // The current contract reuses canonical actor evidence for 14 days.
    // Expire that fixture evidence before testing a newly observed refusal.
    sqlx::query(
        "UPDATE discovery_actor_evidence SET verified_at=now()-interval '15 days' WHERE uri=$1",
    )
    .bind(self::owner())
    .execute(&pool)
    .await
    .unwrap();
    db::request_profile_recheck(&pool, &uri(0)).await.unwrap();
    f.drain().await;
    let e = entry(&f, 0).await;
    assert_eq!(e.state, "denied");
    assert!(e.title.is_none());
    assert!(
        db::video_details(&pool, e.id)
            .await
            .unwrap()
            .unwrap()
            .channel_title
            .is_none()
    );
    assert!(db::entry_topics(&pool, e.id).await.unwrap().is_empty());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn strict_missing_policy_and_video_switches_take_effect_immediately(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    f.drain().await;
    let e = entry(&f, 0).await;
    let mut settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_discoverable = false;
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    settings = db::settings(&pool).await.unwrap();
    assert!(
        videos::eligible(&f.state, member.viewer, e.id, Use::Recommend)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        videos::eligible(&f.state, member.viewer, e.id, Use::IndexVideo)
            .await
            .unwrap()
            .is_some()
    );
    settings.allow_missing_indexable = false;
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    settings = db::settings(&pool).await.unwrap();
    assert!(entry(&f, 0).await.title.is_none());
    assert!(
        db::video_details(&pool, e.id)
            .await
            .unwrap()
            .unwrap()
            .channel_title
            .is_none()
    );
    settings.allow_missing_indexable = true;
    settings.allow_missing_discoverable = true;
    settings.disabled_mechanisms.push("video_catalogue".into());
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    settings = db::settings(&pool).await.unwrap();
    db::request_profile_recheck(&pool, &uri(0)).await.unwrap();
    let before = f.peer.requests.lock().unwrap().len();
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), before);
    settings
        .disabled_mechanisms
        .retain(|v| v != "video_catalogue");
    settings.disabled_mechanisms.push("video_indexing".into());
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), before);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_visibility_and_attribution_override_catalogue_claims(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for n in 0..8 {
        setup(&f, n);
    }
    f.page(json!((0..8).map(record).collect::<Vec<_>>()));
    let mut v = video(0);
    v["to"] = json!([]);
    v["cc"] = json!(["https://www.w3.org/ns/activitystreams#Public"]);
    f.set(&format!("/videos/watch/{}", uuid(0)), v);
    let mut v = video(1);
    v["audience"] = json!("http://private.onion/group");
    f.set(&format!("/videos/watch/{}", uuid(1)), v);
    let mut v = video(2);
    v["attributedTo"] = json!("http://foreign.onion/owner");
    f.set(&format!("/videos/watch/{}", uuid(2)), v);
    let mut v = video(3);
    v["indexable"] = json!("yes");
    f.set(&format!("/videos/watch/{}", uuid(3)), v);
    let mut r = record(4);
    r["state"] = json!({"id":2});
    f.set(&format!("/api/v1/videos/{}", uuid(4)), r);
    let mut r = record(5);
    r["privacy"] = json!({"id":2});
    f.set(&format!("/api/v1/videos/{}", uuid(5)), r);
    let mut v = video(6);
    v["isLiveBroadcast"] = json!(true);
    f.set(&format!("/videos/watch/{}", uuid(6)), v);
    let mut v = video(7);
    v["attributedTo"] = json!(owner());
    f.set(&format!("/videos/watch/{}", uuid(7)), v);
    f.admit_all_candidates().await;
    f.drain().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE state='active'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(entry(&f, 7).await.state, "active");
    assert_eq!(entry(&f, 3).await.indexable, "invalid");
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn deletion_during_first_fetch_cancels_work_and_preserves_actor_binding(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() = Some((
        format!("/videos/watch/{}", uuid(0)),
        started.clone(),
        release.clone(),
    ));
    let state = f.state.clone();
    let task = tokio::spawn(async move { while worker::run_due(&state).await.unwrap() != 0 {} });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    posts::verified_delete(&f.state, Some(&channel(0)), &channel(0))
        .await
        .unwrap();
    release.add_permits(1);
    task.await.unwrap();
    assert!(db::entry(&pool, &uri(0)).await.unwrap().is_none());
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT withdrawn FROM discovery_jobs WHERE target_uri=$1")
            .bind(uri(0))
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_video_work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    *f.peer.hold.lock().unwrap() = None;
    db::request_profile_recheck(&pool, &uri(0)).await.unwrap();
    f.drain().await;
    assert_eq!(entry(&f, 0).await.state, "active");
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn actor_updates_and_owned_deletions_clear_all_metadata(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    f.drain().await;
    let e = entry(&f, 0).await;
    posts::verified_delete(&f.state, Some(&channel(0)), &uri(0))
        .await
        .unwrap();
    assert_eq!(entry(&f, 0).await.state, "active");
    db::forget_actor_topics(&pool, &channel(0)).await.unwrap();
    assert_eq!(entry(&f, 0).await.state, "denied");
    assert!(
        db::video_details(&pool, e.id)
            .await
            .unwrap()
            .unwrap()
            .channel_title
            .is_none()
    );
    db::request_profile_recheck(&pool, &uri(0)).await.unwrap();
    f.drain().await;
    posts::verified_delete(&f.state, Some(&owner()), &uri(0))
        .await
        .unwrap();
    assert_eq!(entry(&f, 0).await.state, "withdrawn");
    assert!(entry(&f, 0).await.title.is_none());
    assert!(db::entry_topics(&pool, e.id).await.unwrap().is_empty());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn auth_failure_never_falls_through_to_another_catalogue(pool: PgPool) {
    let f = fixture_at(pool, directory::PATH).await;
    let url =
        url::Url::parse(&directory::page_uri(&format!("{ORIGIN}{}", directory::PATH), 0).unwrap())
            .unwrap();
    f.status(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        StatusCode::FORBIDDEN,
    );
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn explicit_open_reaches_watch_and_channel_without_following(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    let remote = common::RemoteUser::new("videos-test.onion", "filmmaker");
    for group in [false, true] {
        let mut doc = actor(0, group);
        let id = doc["id"].as_str().unwrap().to_owned();
        doc["publicKey"] =
            json!({"id":format!("{id}#main-key"),"owner":id,"publicKeyPem":remote.keys.public_pem});
        f.set(
            if group {
                "/video-channels/channel0"
            } else {
                "/accounts/filmmaker"
            },
            doc,
        );
    }
    f.drain().await;
    let e = entry(&f, 0).await;
    let path = format!("/web/discover/{}/open", e.id);
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
        &[("csrf", &member.csrf), ("back", "/discover/videos")],
    )
    .await;
    assert_eq!(opened.0, StatusCode::SEE_OTHER);
    assert!(
        opened.1["location"]
            .to_str()
            .unwrap()
            .starts_with("/@filmmaker@videos-test.onion/"),
        "{:?}",
        opened.1
    );
    let stored = plamenu_db::status::find_by_uri(&pool, &uri(0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.object_type.as_deref(), Some("Video"));
    assert_eq!(
        plamenu_db::status::ingest_provenance(&pool, stored.id)
            .await
            .unwrap()
            .as_deref(),
        Some("explicit_resolution")
    );
    let opened = request(
        &f.state,
        &member,
        "POST",
        &format!("/web/discover/{}/channel", e.id),
        &[("csrf", &member.csrf)],
    )
    .await;
    assert_eq!(opened.0, StatusCode::SEE_OTHER);
    assert!(
        opened.1["location"]
            .to_str()
            .unwrap()
            .starts_with("/videos/explore?channel="),
        "{:?}",
        opened.1
    );
    let author = plamenu_db::account::find_by_uri(&pool, &owner())
        .await
        .unwrap()
        .unwrap();
    plamenu::ingest::ingest_remote_note_delivery(&f.state, &author, &video(0))
        .await
        .unwrap();
    assert_eq!(entry(&f, 0).await.state, "active");
    let mut narrowed = video(0);
    narrowed["audience"] = json!(format!("{ORIGIN}/private-group"));
    plamenu::ingest::ingest_remote_note_delivery(&f.state, &author, &narrowed)
        .await
        .unwrap();
    assert_eq!(entry(&f, 0).await.state, "denied");
    for table in ["follows", "notifications"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table}"
            )))
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn budget_and_pause_fence_video_verification(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    setup(&f, 0);
    f.page(json!([record(0)]));
    let mut settings = db::settings(&pool).await.unwrap();
    settings.requests_per_origin = 2;
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    worker::run_due(&f.state).await.unwrap();
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), 2);
    assert!(db::entry(&pool, &uri(0)).await.unwrap().is_none());
    settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    assert!(db::save_settings(&pool, &settings).await.unwrap());
    worker::run_due(&f.state).await.unwrap();
    assert_eq!(f.peer.requests.lock().unwrap().len(), 2);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn wrong_channel_ownership_or_actor_type_cannot_authorize_a_video(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for n in 0..8 {
        setup(&f, n);
    }
    f.page(json!((0..8).map(record).collect::<Vec<_>>()));
    let mut first = actor(0, true);
    first["attributedTo"] = json!([format!("{ORIGIN}/accounts/someone_else")]);
    f.set("/video-channels/channel0", first);
    let mut second = actor(4, true);
    second["type"] = json!("Person");
    f.set("/video-channels/channel1", second);
    f.admit_all_candidates().await;
    f.drain().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn channel_cap_and_actor_update_fence_are_enforced(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for n in 0..8 {
        setup(&f, n);
        let mut r = record(n);
        r["channel"]["url"] = json!(channel(0));
        f.set(&format!("/api/v1/videos/{}", uuid(n)), r);
        let mut v = video(n);
        v["attributedTo"][1]["id"] = json!(channel(0));
        v["audience"] = json!(channel(0));
        f.set(&format!("/videos/watch/{}", uuid(n)), v);
    }
    f.page(json!(
        (0..8)
            .map(|n| {
                let mut r = record(n);
                r["channel"]["url"] = json!(channel(0));
                r
            })
            .collect::<Vec<_>>()
    ));
    f.admit_all_candidates().await;
    f.drain().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE state='active'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        8 // fixed per-channel storage caps were removed; all bounded hints survive
    );
    let e: db::Entry = sqlx::query_as("SELECT * FROM discovery_entries ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    db::request_profile_recheck(&pool, &e.canonical_uri)
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let path = url::Url::parse(&e.canonical_uri).unwrap().path().to_owned();
    *f.peer.hold.lock().unwrap() = Some((path, started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { worker::run_due(&state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    plamenu::discovery::profiles::verified_update(&f.state, &channel(0), &actor(0, true))
        .await
        .unwrap();
    release.add_permits(1);
    task.await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE state='active'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM discovery_video_details WHERE channel_title IS NOT NULL OR duration IS NOT NULL").fetch_one(&pool).await.unwrap(),0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn fresh_channel_discovery_refusal_preserves_indexing_and_fences_cached_siblings(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    setup(&f, 0);
    setup(&f, 1);
    f.page(json!([record(0), record(1)]));
    f.drain().await;
    for n in 0..2 {
        let e = entry(&f, n).await;
        assert!(
            videos::eligible(&f.state, member.viewer, e.id, Use::Recommend)
                .await
                .unwrap()
                .is_some()
        );
    }
    // A canonical fresh opt-out (including an invalid discovery declaration)
    // restricts promotion of all siblings while public indexing remains allowed.
    for signal in [json!(false), Value::Null] {
        let mut document = actor(0, true);
        document["discoverable"] = signal.clone();
        f.set("/video-channels/channel0", document);
        sqlx::query(
            "UPDATE discovery_actor_evidence SET verified_at=now()-interval '15 days' WHERE uri=$1",
        )
        .bind(channel(0))
        .execute(&pool)
        .await
        .unwrap();
        db::request_profile_recheck(&pool, &uri(1)).await.unwrap();
        f.drain().await;
        for n in 0..2 {
            let e = entry(&f, n).await;
            assert_eq!(e.state, "active");
            assert_eq!(
                e.discoverable,
                if signal.is_null() { "invalid" } else { "deny" }
            );
            assert_eq!(
                e.title.as_deref(),
                Some(format!("Garden film {n}").as_str())
            );
            assert!(
                videos::eligible(&f.state, member.viewer, e.id, Use::Recommend)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                videos::eligible(&f.state, member.viewer, e.id, Use::IndexVideo)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(db::entry_topics(&pool, e.id).await.unwrap(), ["gardens"]);
            assert!(
                db::video_details(&pool, e.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .channel_title
                    .is_none_or(|title| title.is_empty())
            );
        }
    }
    let mut owner_document = actor(0, false);
    owner_document["indexable"] = json!(false);
    plamenu::discovery::profiles::verified_update(&f.state, &owner(), &owner_document)
        .await
        .unwrap();
    for n in 0..2 {
        let e = entry(&f, n).await;
        assert_eq!(e.state, "denied");
        assert!(e.title.is_none());
        assert!(
            videos::eligible(&f.state, member.viewer, e.id, Use::IndexVideo)
                .await
                .unwrap()
                .is_none()
        );
    }
}
