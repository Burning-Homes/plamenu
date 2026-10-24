//! Generated peer graph: no member activity, no public-network dependency.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response},
};
use plamenu::{
    AppState,
    discovery::{seeds, worker},
    federation::HttpFederation,
};
use plamenu_db::{PgPool, discovery_store as db};
use plamenu_federation::{FederationClient, ProxyConfig};
use plamenu_test_support as common;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Network {
    requests: Mutex<Vec<String>>,
}
async fn respond(State(network): State<Arc<Network>>, request: Request<Body>) -> Response<Body> {
    let host = request.uri().host().unwrap_or_default();
    let path = request.uri().path();
    assert!(!request.headers().contains_key("cookie"));
    assert!(!request.headers().contains_key("authorization"));
    network
        .requests
        .lock()
        .unwrap()
        .push(format!("{host}{path}"));
    let value = match path {
        "/api/v1/instance/peers" => match host {
            "a.onion" => Value::Array(vec![json!("b.onion"); 300]),
            "b.onion" => json!(["a.onion", "b.onion", "c.onion"]),
            "c.onion" => json!(["d.onion"]),
            _ => json!([]),
        },
        "/api/v1/directory" => {
            let n = if host == "b.onion" { 20 } else { 2 };
            Value::Array((0..n).map(|i|json!({"uri":format!("http://{host}/users/person{i}"),"display_name":"Untrusted"})).collect())
        }
        _ => {
            let name = path.rsplit('/').next().unwrap();
            json!({"id":format!("http://{host}{path}"),"type":"Person","preferredUsername":name,"name":format!("Person at {host}"),"summary":"<p>A canonical profile</p>"})
        }
    };
    Response::builder()
        .header(
            "content-type",
            if path.starts_with("/api/") {
                "application/json"
            } else {
                "application/activity+json"
            },
        )
        .body(Body::from(value.to_string()))
        .unwrap()
}
fn state(pool: &PgPool, proxy: &str) -> AppState {
    let config = common::test_config();
    let keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config).unwrap());
    let client = FederationClient::new("discovery-expansion-test", true)
        .unwrap()
        .with_proxies(&ProxyConfig {
            onion_proxy_url: Some(proxy.into()),
            ..ProxyConfig::default()
        })
        .unwrap()
        .with_fetch_signer(common::RemoteUser::new("plamenu.test", "instance").signer());
    let federation = Arc::new(HttpFederation::new(
        client,
        pool.clone(),
        config.domain.clone(),
        keyring,
    ));
    AppState::new(
        pool.clone(),
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}
async fn drain(state: &AppState) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    while tokio::time::Instant::now() < deadline {
        if worker::run_due(state).await.unwrap() == 0 {
            if db::diagnostics(&state.pool).await.unwrap().queued == 0 {
                return;
            }
            // Shared federation pacing can defer a job without failing it.
            // Exercise durable recovery rather than treating idle as drained.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    panic!("bounded graph did not settle");
}

struct Deferred {
    id: i64,
    run_at: time::OffsetDateTime,
    job: Value,
    receipts: Value,
}
async fn receipts(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY r.id),'[]'::jsonb) FROM discovery_requests r",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn defer_once(state: &AppState, network: &Network) -> Deferred {
    // Exercise actual foreground contention deterministically. The real hidden
    // fetch gate must reject admission before sending or reserving any bytes.
    let held = plamenu::hidden_gate::fetch_permit("http://a.onion")
        .await
        .unwrap();
    let wire = network.requests.lock().unwrap().len();
    let before = receipts(&state.pool).await;
    let lower: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(worker::run_due(state).await.unwrap(), 1);
    let upper: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let mut jobs: Vec<(i64, time::OffsetDateTime, Value)> = sqlx::query_as(
        "SELECT id,run_at,to_jsonb(j)-'run_at' FROM discovery_jobs j
         WHERE last_error='federation_busy' AND NOT withdrawn",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(jobs.len(), 1);
    let (id, run_at, job) = jobs.pop().unwrap();
    // Assert the unchanged production backoff, without sleeping through it.
    assert!(run_at >= lower + time::Duration::seconds(30));
    assert!(run_at <= upper + time::Duration::seconds(30));
    assert_eq!(job["attempts"], 0, "local pressure cannot consume retries");
    for key in ["lease_token", "lease_owner", "leased_until"] {
        assert!(job[key].is_null(), "deferred work releases {key}");
    }
    assert_eq!(network.requests.lock().unwrap().len(), wire);
    assert_eq!(receipts(&state.pool).await, before);
    drop(held);
    Deferred {
        id,
        run_at,
        job,
        receipts: before,
    }
}
async fn resume_deferred(state: &AppState, deferred: &Deferred) {
    let (run_at, job): (time::OffsetDateTime, Value) =
        sqlx::query_as("SELECT run_at,to_jsonb(j)-'run_at' FROM discovery_jobs j WHERE id=$1")
            .bind(deferred.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(run_at, deferred.run_at, "restart preserves the retry clock");
    assert_eq!(
        job, deferred.job,
        "restart preserves the entire durable job"
    );
    assert_eq!(receipts(&state.pool).await, deferred.receipts);
    // Advance only this owned fixture job's eligibility, after checking the
    // actual 30-second retry deadline above. Do not change its TTL, cursor,
    // attempts, revisions, any wire receipt or any production timing/limit.
    assert_eq!(
        sqlx::query("UPDATE discovery_jobs SET run_at=clock_timestamp() WHERE id=$1 AND last_error='federation_busy' AND lease_token IS NULL AND NOT withdrawn")
            .bind(deferred.id).execute(&state.pool).await.unwrap().rows_affected(),
        1
    );
    let resumed: Value =
        sqlx::query_scalar("SELECT to_jsonb(j)-'run_at' FROM discovery_jobs j WHERE id=$1")
            .bind(deferred.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        resumed, deferred.job,
        "only the fixture retry clock advances"
    );
    assert_eq!(receipts(&state.pool).await, deferred.receipts);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn graph_expands_survives_restart_and_accepts_independent_import(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let network = Arc::new(Network::default());
    let router = Router::new().fallback(respond).with_state(network.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = true;
    // Match the real singleton onion lane, avoiding contention between this
    // fixture's own jobs. Foreground deferral/restart is checked explicitly.
    settings.max_inflight = 1;
    db::save_settings(&pool, &settings).await.unwrap();
    let root = r#"{"version":1,"sources":[{"origin":"http://a.onion","verified":true}]}"#;
    assert_eq!(
        seeds::import(&pool, root, true, false)
            .await
            .unwrap()
            .admitted,
        1
    );
    let first = state(&pool, &proxy);
    worker::run_due(&first).await.unwrap();
    let deferred = defer_once(&first, &network).await;
    drop(first);
    let restarted = state(&pool, &proxy);
    resume_deferred(&restarted, &deferred).await;
    drain(&restarted).await;
    let sources = db::source_diagnostics(&pool).await.unwrap();
    assert_eq!(sources.len(), 3);
    assert_eq!(
        sources
            .iter()
            .find(|s| s.origin == "http://c.onion")
            .unwrap()
            .depth,
        2
    );
    assert_eq!(
        sources
            .iter()
            .find(|s| s.origin == "http://a.onion")
            .unwrap()
            .peer_offset,
        256
    );
    let profiles = db::profile_candidates(&pool).await.unwrap();
    assert_eq!(profiles.len(), 24); // 2 + the full bounded page of 20 + 2
    assert!(
        profiles
            .iter()
            .all(|e| e.discoverable == "missing" && e.title.as_deref() != Some("Untrusted"))
    );
    assert!(
        !network
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "c.onion/api/v1/instance/peers")
    );
    let independent = r#"{"version":1,"sources":[{"origin":"http://independent.onion"}]}"#;
    assert_eq!(
        seeds::import(&pool, independent, false, true)
            .await
            .unwrap()
            .admitted,
        1
    );
    drain(&restarted).await;
    assert!(
        !db::source_diagnostics(&pool)
            .await
            .unwrap()
            .iter()
            .find(|s| s.origin == "http://a.onion")
            .unwrap()
            .enabled
    );
    assert!(
        db::profile_candidates(&pool)
            .await
            .unwrap()
            .iter()
            .any(|e| e.canonical_uri.starts_with("http://independent.onion/"))
    );
    let exported = seeds::export(&pool).await.unwrap();
    for private in [
        "introduced_by",
        "enabled",
        "last_error",
        "discoverable",
        "lease",
        "account_id",
    ] {
        assert!(!exported.contains(private));
    }
    assert_eq!(seeds::parse(&exported).unwrap().len(), 4);
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, 0);
    server.abort();
}
