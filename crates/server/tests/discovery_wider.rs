//! Real guarded HTTP and the member web workflow, using two public API families.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use common::{RemoteUser, create_local_account};
use plamenu::{
    AppState,
    discovery::{browse, wider},
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

const ORIGIN: &str = "http://wide-one.onion";
type Held = (String, Arc<Notify>, Arc<Semaphore>);
#[derive(Default)]
struct Peer {
    documents: Mutex<HashMap<String, (StatusCode, Value)>>,
    requests: Mutex<Vec<String>>,
    hold: Mutex<Option<Held>>,
}
async fn respond(State(peer): State<Arc<Peer>>, request: Request<Body>) -> Response<Body> {
    let uri = request.uri().to_string();
    let path = request.uri().path();
    assert!(!request.headers().contains_key("authorization"));
    assert!(!request.headers().contains_key("cookie"));
    if path.contains("search") {
        assert!(!request.headers().contains_key("signature"));
        assert_eq!(
            request.headers()["Mastodon-Async-Refresh-Id"],
            "plamenu-local-search"
        );
    }
    peer.requests.lock().unwrap().push(uri.clone());
    let held = peer.hold.lock().unwrap().clone();
    if let Some((target, started, release)) = held
        && target == uri
    {
        started.notify_one();
        let permit = release.acquire().await.unwrap();
        permit.forget();
    }
    let (status, body) = peer
        .documents
        .lock()
        .unwrap()
        .get(&uri)
        .cloned()
        .unwrap_or((StatusCode::NOT_FOUND, json!({})));
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
        .header("link", "<http://not-selected.onion/next>; rel=\"next\"")
        .header("retry-after", "120")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}
struct Fixture {
    state: AppState,
    peer: Arc<Peer>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    fn set(&self, uri: &str, value: Value) {
        self.peer
            .documents
            .lock()
            .unwrap()
            .insert(uri.into(), (StatusCode::OK, value));
    }
    fn status(&self, uri: &str, status: StatusCode) {
        self.peer
            .documents
            .lock()
            .unwrap()
            .insert(uri.into(), (status, json!({})));
    }
    fn query(&self, origin: &str, kind: &str, q: &str, rows: Value) {
        let mut document = json!({});
        document[if kind == "profile" {
            "accounts"
        } else {
            "communities"
        }] = rows;
        self.set(&wider::query_url(origin, kind, q).unwrap(), document);
    }
    fn hold(&self, uri: &str) -> (Arc<Notify>, Arc<Semaphore>) {
        let start = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        *self.peer.hold.lock().unwrap() = Some((uri.into(), start.clone(), release.clone()));
        (start, release)
    }
    fn calls(&self) -> Vec<String> {
        self.peer.requests.lock().unwrap().clone()
    }
}
async fn fixture(pool: PgPool) -> Fixture {
    let peer = Arc::new(Peer::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(respond).with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = common::test_config();
    let keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config).unwrap());
    let client = FederationClient::new("wider-test", true)
        .unwrap()
        .with_proxies(&ProxyConfig {
            onion_proxy_url: Some(proxy),
            ..Default::default()
        })
        .unwrap()
        .with_fetch_signer(RemoteUser::new("plamenu.test", "instance").signer());
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
    db::save_settings(&pool, &settings).await.unwrap();
    Fixture {
        state,
        peer,
        server,
    }
}
async fn source(pool: &PgPool, origin: &str, kind: &str) -> i64 {
    let cap = if kind == "community" {
        "community_directory"
    } else {
        "directory"
    };
    let endpoint = format!(
        "{origin}{}",
        if kind == "community" {
            "/api/v3/community/list"
        } else {
            "/api/v1/directory"
        }
    );
    let db::Admission::Added(id) = db::add_source(pool, &endpoint, None).await.unwrap() else {
        panic!("source")
    };
    sqlx::query("UPDATE discovery_sources SET canonical_uri=endpoint,capabilities=ARRAY[$2],verified_at=now(),valid_until=now()+interval '1 day' WHERE id=$1").bind(id).bind(cap).execute(pool).await.unwrap();
    id
}
fn actor(origin: &str, name: &str) -> Value {
    json!({"id":format!("{origin}/users/{name}"),"type":"Person","preferredUsername":name,"name":format!("{name} canonical"),"summary":"An independently verified public person","tag":[{"type":"Hashtag","name":"#gardens"}]})
}
async fn known(pool: &PgPool, source: i64, origin: &str) -> i64 {
    let id = plamenu_db::id::next();
    sqlx::query("INSERT INTO discovery_entries(id,canonical_uri,actor_uri,source_id,kind,policy_revision,discoverable,indexable,visibility,title,summary,verified_at,valid_until) VALUES($1,$2,$2,$3,'profile',1,'missing','missing','public','Cinder local','Already in the catalogue',now(),now()+interval '1 day')")
        .bind(id).bind(format!("{origin}/users/known")).bind(source).execute(pool).await.unwrap();
    id
}
async fn effects(pool: &PgPool) -> Vec<i64> {
    let mut result = Vec::new();
    for table in [
        "accounts",
        "statuses",
        "follows",
        "notifications",
        "delivery_jobs",
        "account_media_jobs",
        "link_crawl_jobs",
        "remote_history_jobs",
    ] {
        result.push(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table}"
            )))
            .fetch_one(pool)
            .await
            .unwrap(),
        );
    }
    result
}
struct Session {
    viewer: i64,
    token: String,
    csrf: String,
}
async fn session(state: &AppState) -> Session {
    let username = format!("member{}", plamenu_db::id::next());
    let viewer = create_local_account(&state.pool, &username, "Member").await;
    let user = plamenu_db::user::set_credentials(
        &state.pool,
        viewer.id,
        Some(&format!("{username}@example.test")),
        "unused",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(state).await.unwrap();
    let token = format!("wide-{}", plamenu_db::id::next());
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
        viewer: viewer.id,
        token,
        csrf,
    }
}
async fn request(
    state: &AppState,
    user: &Session,
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
                .header("cookie", format!("__Host-plamenu_session={}", user.token))
                .header("authorization", format!("Bearer {}", user.token))
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
fn run_id(start: db::SearchStart) -> i64 {
    match start {
        db::SearchStart::Started(r) | db::SearchStart::Cached(r) => r.id,
        _ => panic!("search did not run"),
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn explicit_search_verifies_only_selected_origins_preserves_local_results_and_caches_receipts(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    known(&pool, sid, ORIGIN).await;
    f.query(
        ORIGIN,
        "profile",
        "Cinder",
        json!([
        {"uri":format!("{ORIGIN}/users/new"),"display_name":"Unverified poison"},
        {"uri":"http://not-selected.onion/users/foreign"},
        {"uri":format!("{ORIGIN}/users/denied"),"discoverable":false}]),
    );
    f.set(&format!("{ORIGIN}/users/new"), actor(ORIGIN, "new"));
    let before = effects(&pool).await;
    let page = request(&f.state, &user, "GET", "/discover?q=Cinder", &[]).await;
    assert!(page.2.contains("Cinder local") && page.2.contains("Send this query to:"));
    assert!(f.calls().is_empty());
    let fields = [
        ("csrf", user.csrf.as_str()),
        ("q", "Cinder"),
        ("view", "people"),
        ("source1", &sid.to_string()),
    ];
    assert_eq!(
        request(
            &f.state,
            &user,
            "POST",
            "/web/discover/search",
            &[("csrf", "bad"), ("q", "Cinder"), ("view", "people")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let post = request(&f.state, &user, "POST", "/web/discover/search", &fields).await;
    assert_eq!(post.0, StatusCode::SEE_OTHER);
    let location = post.1["location"].to_str().unwrap();
    let page = request(&f.state, &user, "GET", location, &[]).await;
    assert!(page.2.contains("Cinder local") && page.2.contains("new canonical"));
    assert!(!page.2.contains("Unverified poison"));
    assert_eq!(page.1["cache-control"], "private, no-store");
    assert_eq!(f.calls().len(), 2);
    assert_eq!(effects(&pool).await, before);
    request(&f.state, &user, "POST", "/web/discover/search", &fields).await;
    assert_eq!(f.calls().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let receipt: String =
        sqlx::query_scalar("SELECT row_to_json(r)::text FROM discovery_search_runs r")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!receipt.contains("Cinder"));
    let other = session(&f.state).await;
    assert!(
        !request(&f.state, &other, "GET", location, &[])
            .await
            .2
            .contains("new canonical")
    );
    let filtered = request(&f.state, &user, "GET", &format!("{location}&source=0"), &[]).await;
    assert!(!filtered.2.contains("new canonical") && !filtered.2.contains("Servers checked"));
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = false;
    db::save_settings(&pool, &settings).await.unwrap();
    let disabled = request(&f.state, &user, "GET", location, &[]).await;
    assert!(!disabled.2.contains("Servers checked") && !disabled.2.contains("data-wider-search"));
    assert_eq!(f.calls().len(), 2);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn older_account_urls_resolve_only_on_selected_origin_and_never_enqueue_aliases(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    f.query(
        ORIGIN,
        "profile",
        "people",
        json!([{"url":format!("{ORIGIN}/@alice")},{"url":format!("{ORIGIN}/@foreign")} ]),
    );
    f.set(&format!("{ORIGIN}/@alice"), actor(ORIGIN, "alice"));
    f.set(&format!("{ORIGIN}/users/alice"), actor(ORIGIN, "alice"));
    f.set(
        &format!("{ORIGIN}/@foreign"),
        actor("http://not-selected.onion", "foreign"),
    );
    wider::search(&f.state, user.viewer, "profile", "people", &[sid])
        .await
        .unwrap();
    assert_eq!(f.calls().len(), 4);
    assert!(f.calls().iter().all(|u| u.starts_with(ORIGIN)));
    assert_eq!(
        db::entry_topics(
            &pool,
            db::entry(&pool, &format!("{ORIGIN}/users/alice"))
                .await
                .unwrap()
                .unwrap()
                .id
        )
        .await
        .unwrap(),
        ["gardens"]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_community_search_needs_canonical_group_and_current_individual_record(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "community").await;
    let record = json!({"actor_id":format!("{ORIGIN}/c/cinder"),"local":true,"visibility":"Public","deleted":false,"removed":false,"hidden":false,"nsfw":false});
    f.query(ORIGIN, "community", "cinder", json!([{"community":record}]));
    f.set(&format!("{ORIGIN}/c/cinder"),json!({"id":format!("{ORIGIN}/c/cinder"),"type":"Group","preferredUsername":"cinder","name":"Cinder canonical","summary":"Community","indexable":false}));
    f.set(
        &format!("{ORIGIN}/api/v3/community?name=cinder"),
        json!({"community_view":{"community":record}}),
    );
    let mut settings = db::settings(&pool).await.unwrap();
    settings
        .disabled_mechanisms
        .push("community_directory".into());
    db::save_settings(&pool, &settings).await.unwrap();
    wider::search(&f.state, user.viewer, "community", "cinder", &[sid])
        .await
        .unwrap();
    assert_eq!(f.calls().len(), 3);
    assert_eq!(
        browse::page_kind(&f.state, user.viewer, None, None, "community")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        db::entry(&pool, &format!("{ORIGIN}/c/cinder"))
            .await
            .unwrap()
            .unwrap()
            .indexable,
        "deny"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn authentication_failure_is_cached_without_disabling_directory_or_forwarding(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    let known = known(&pool, sid, ORIGIN).await;
    f.status(
        &wider::query_url(ORIGIN, "profile", "Cinder").unwrap(),
        StatusCode::UNAUTHORIZED,
    );
    wider::search(&f.state, user.viewer, "profile", "Cinder", &[sid])
        .await
        .unwrap();
    assert_eq!(f.calls().len(), 1);
    assert!(
        browse::eligible(&f.state, user.viewer, known)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        wider::sources(&f.state, user.viewer, "profile", None)
            .await
            .unwrap()
            .is_empty()
    );
    wider::search(&f.state, user.viewer, "profile", "again", &[sid])
        .await
        .unwrap();
    assert_eq!(f.calls().len(), 1);
    assert!(
        db::source(&pool, sid)
            .await
            .unwrap()
            .unwrap()
            .cooldown_until
            .is_none()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn deadline_preserves_local_and_partial_results_without_background_continuation(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let first = source(&pool, ORIGIN, "profile").await;
    let second = source(&pool, "http://wide-two.onion", "profile").await;
    let known = known(&pool, first, ORIGIN).await;
    f.query(
        ORIGIN,
        "profile",
        "Cinder",
        json!([{"uri":format!("{ORIGIN}/users/new")} ]),
    );
    f.set(&format!("{ORIGIN}/users/new"), actor(ORIGIN, "new"));
    let uri = "http://wide-two.onion/users/slow";
    f.query(
        "http://wide-two.onion",
        "profile",
        "Cinder",
        json!([{"uri":uri}]),
    );
    f.set(uri, actor("http://wide-two.onion", "slow"));
    let (_, release) = f.hold(uri);
    let start = std::time::Instant::now();
    let id = run_id(
        wider::search(&f.state, user.viewer, "profile", "Cinder", &[first, second])
            .await
            .unwrap(),
    );
    assert!(start.elapsed() < Duration::from_secs(22));
    let run = db::search_run(&pool, user.viewer, id, &wider::digest("profile", "Cinder"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.results[0]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(run.results[1]["outcome"], "deadline");
    assert!(
        browse::eligible(&f.state, user.viewer, known)
            .await
            .unwrap()
            .is_some()
    );
    assert!(db::claim_due(&pool, "background").await.unwrap().is_empty());
    release.add_permits(1);
    assert_eq!(f.calls().len(), 4);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn deletion_during_first_search_verification_keeps_its_guard_after_receipt_cleanup(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    let uri = format!("{ORIGIN}/users/new");
    f.query(ORIGIN, "profile", "Cinder", json!([{"uri":uri}]));
    f.set(&uri, actor(ORIGIN, "new"));
    let (started, release) = f.hold(&uri);
    let state = f.state.clone();
    let viewer = user.viewer;
    let task = tokio::spawn(async move {
        wider::search(&state, viewer, "profile", "Cinder", &[sid])
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    db::invalidate_entry(&pool, &uri, db::Invalidation::Withdrawn, "deleted")
        .await
        .unwrap();
    release.add_permits(1);
    task.await.unwrap();
    assert!(db::entry(&pool, &uri).await.unwrap().is_none());
    assert!(matches!(
        db::enqueue(&pool, sid, &uri, db::Operation::RefreshEntry, None)
            .await
            .unwrap(),
        db::Admission::Suppressed
    ));
    let guard: (bool, Option<i64>) =
        sqlx::query_as("SELECT withdrawn,search_run_id FROM discovery_jobs WHERE target_uri=$1")
            .bind(&uri)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(guard, (true, None));
    sqlx::query("DELETE FROM discovery_search_runs")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_jobs WHERE withdrawn")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn viewer_domain_changes_cancel_inflight_requests(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    let query = wider::query_url(ORIGIN, "profile", "Cinder").unwrap();
    f.query(ORIGIN, "profile", "Cinder", json!([]));
    let (started, release) = f.hold(&query);
    let state = f.state.clone();
    let viewer = user.viewer;
    let task = tokio::spawn(async move {
        wider::search(&state, viewer, "profile", "Cinder", &[sid])
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    plamenu_db::account_domain_block::create(&pool, user.viewer, "wide-one.onion")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    release.add_permits(1);
    assert_eq!(f.calls().len(), 1);
    assert!(db::claim_due(&pool, "background").await.unwrap().is_empty());
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(!wider::enabled(&f.state).await.unwrap());
    assert!(
        wider::sources(&f.state, user.viewer, "profile", None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn interactive_leases_share_budgets_are_throttled_and_never_enter_background_work(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    let digest = wider::digest("profile", "Cinder");
    let db::SearchStart::Started(run) =
        db::start_search(&pool, user.viewer, &digest, "profile", &[sid])
            .await
            .unwrap()
    else {
        panic!("run");
    };
    assert!(matches!(
        db::start_search(
            &pool,
            user.viewer,
            &wider::digest("profile", "different"),
            "profile",
            &[sid]
        )
        .await
        .unwrap(),
        db::SearchStart::Busy
    ));
    let uri = format!("{ORIGIN}/users/new");
    let lease = db::search_lease(&pool, &run, sid, &uri, true)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..24 {
        let db::Reservation::Reserved(ticket) =
            db::reserve_request(&pool, &lease, &uri).await.unwrap()
        else {
            panic!("wire budget");
        };
        db::complete_request(&pool, ticket, 100).await.unwrap();
    }
    assert_eq!(
        db::reserve_request(&pool, &lease, &uri).await.unwrap(),
        db::Reservation::Budget
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_requests")
            .fetch_one(&pool)
            .await
            .unwrap(),
        24
    );
    // A settings revision clears leases, but must never turn this canonical
    // verification into ordinary background work.
    let mut settings = db::settings(&pool).await.unwrap();
    settings.requests_per_day += 1;
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(!db::lease_is_current(&pool, &lease).await.unwrap());
    assert!(db::claim_due(&pool, "background").await.unwrap().is_empty());
    db::finish_search(&pool, run.id, &json!([])).await.unwrap();
    assert!(matches!(
        db::start_search(&pool, user.viewer, &digest, "profile", &[sid])
            .await
            .unwrap(),
        db::SearchStart::Cached(_)
    ));
    assert!(f.calls().is_empty());
    // Expiring the one retained receipt also removes any abandoned leases.
    sqlx::query("UPDATE discovery_search_runs SET expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    db::prune_jobs(&pool).await.unwrap();
    assert!(
        db::search_run(&pool, user.viewer, run.id, &digest)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_opt_outs_and_global_pause_win_over_search_hints(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let user = session(&f.state).await;
    let sid = source(&pool, ORIGIN, "profile").await;
    f.query(
        ORIGIN,
        "profile",
        "Cinder",
        json!([
            {"uri":format!("{ORIGIN}/users/denied"),"discoverable":true},
            {"uri":format!("{ORIGIN}/users/malformed"),"discoverable":true},
            {"uri":format!("{ORIGIN}/users/held"),"discoverable":true}
        ]),
    );
    let mut denied = actor(ORIGIN, "denied");
    denied["discoverable"] = json!(false);
    f.set(&format!("{ORIGIN}/users/denied"), denied);
    let mut malformed = actor(ORIGIN, "malformed");
    malformed["discoverable"] = json!("yes");
    f.set(&format!("{ORIGIN}/users/malformed"), malformed);
    let held = format!("{ORIGIN}/users/held");
    f.set(&held, actor(ORIGIN, "held"));
    let (started, release) = f.hold(&held);
    let state = f.state.clone();
    let task = tokio::spawn(async move {
        wider::search(&state, user.viewer, "profile", "Cinder", &[sid])
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    db::save_settings(&pool, &settings).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    release.add_permits(1);
    assert!(db::entry(&pool, &held).await.unwrap().is_none());
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = false;
    db::save_settings(&pool, &settings).await.unwrap();
    for name in ["denied", "malformed"] {
        if let Some(entry) = db::entry(&pool, &format!("{ORIGIN}/users/{name}"))
            .await
            .unwrap()
        {
            assert!(
                browse::eligible(&f.state, user.viewer, entry.id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
    assert!(db::claim_due(&pool, "background").await.unwrap().is_empty());
}

#[test]
fn query_parameters_and_hint_bounds_do_not_request_recursion() {
    let url = wider::query_url(ORIGIN, "profile", "@name a&b").unwrap();
    let parsed = url::Url::parse(&url).unwrap();
    let params: HashMap<_, _> = parsed.query_pairs().collect();
    assert_eq!(params["q"], "@name a&b");
    assert_eq!(params["resolve"], "false");
    assert!(!params.contains_key("offset"));
    assert!(wider::hints(&json!({"accounts":[{},{},{},{}]}), "profile", ORIGIN).is_err());
    assert!(wider::term("bad\0query").is_err());
    assert!(wider::term(&"a".repeat(257)).is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_and_alias_rate_limits_stop_remaining_candidates_and_pause_the_origin(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    for (origin, alias) in [(ORIGIN, false), ("http://wide-two.onion", true)] {
        let user = session(&f.state).await;
        let sid = source(&pool, origin, "profile").await;
        let existing = known(&pool, sid, origin).await;
        let first = format!(
            "{origin}/{}",
            if alias { "@limited" } else { "users/limited" }
        );
        let mut hint = json!({});
        hint[if alias { "url" } else { "uri" }] = json!(first);
        f.query(
            origin,
            "profile",
            "Cinder",
            json!([hint,{"uri":format!("{origin}/users/next")} ]),
        );
        f.status(&first, StatusCode::TOO_MANY_REQUESTS);
        f.set(&format!("{origin}/users/next"), actor(origin, "next"));
        let before = f.calls().len();
        wider::search(&f.state, user.viewer, "profile", "Cinder", &[sid])
            .await
            .unwrap();
        assert_eq!(f.calls().len(), before + 2);
        let source = db::source(&pool, sid).await.unwrap().unwrap();
        assert!(
            source.cooldown_until.unwrap()
                > time::OffsetDateTime::now_utc() + time::Duration::seconds(110)
        );
        assert_eq!(source.last_error.as_deref(), Some("rate_limited"));
        assert!(
            browse::eligible(&f.state, user.viewer, existing)
                .await
                .unwrap()
                .is_some()
        );
        let other = session(&f.state).await;
        assert!(matches!(
            wider::search(&f.state, other.viewer, "profile", "Cinder", &[sid])
                .await
                .unwrap(),
            db::SearchStart::Disabled
        ));
        assert_eq!(f.calls().len(), before + 2);
    }
    assert!(db::claim_due(&pool, "background").await.unwrap().is_empty());
}
