//! Generated peer graph: no member activity, no public-network dependency.
mod common;
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

#[sqlx::test(migrations = "../db/migrations")]
async fn graph_expands_survives_restart_and_accepts_independent_import(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let network = Arc::new(Network::default());
    let router = Router::new().fallback(respond).with_state(network.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = true;
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
    drop(first);
    let restarted = state(&pool, &proxy);
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
    assert_eq!(profiles.len(), 7); // 2 + three rotated candidates + 2
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
