//! Reproducible acquisition under the same 100-attempt pilot budget. No Internet.
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
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Network {
    requests: Mutex<Vec<String>>,
}
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
fn uuid(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}
fn owner(n: usize) -> usize {
    if n < 4 { 0 } else { n }
}
fn community(origin: &str, name: &str) -> Value {
    json!({"id":1,"name":name,"title":name,"actor_id":format!("{origin}/c/{name}"),
        "local":true,"deleted":false,"removed":false,"hidden":false,"visibility":"Public","nsfw":false})
}
fn record(origin: &str, n: usize) -> Value {
    json!({"uuid":uuid(n),"url":format!("{origin}/videos/watch/{}",uuid(n)),"name":"Untrusted hint",
        "privacy":{"id":1},"state":{"id":1},"isLocal":true,"isLive":false,"nsfw":false,"duration":90,
        "account":{"url":format!("{origin}/accounts/{}",owner(n))},"channel":{"url":format!("{origin}/video-channels/{}",owner(n))}})
}
async fn respond(State(network): State<Arc<Network>>, request: Request<Body>) -> Response<Body> {
    assert!(!request.headers().contains_key("cookie"));
    assert!(!request.headers().contains_key("authorization"));
    let url = url::Url::parse(&request.uri().to_string()).unwrap();
    let origin = url.origin().ascii_serialization();
    let path = url.path();
    let query: HashMap<_, _> = url.query_pairs().collect();
    network.requests.lock().unwrap().push(url.to_string());
    let value = match path {
        "/api/v1/instance/peers" => json!([]),
        "/api/v1/directory" => {
            assert_eq!(query.get("order").map(AsRef::as_ref), Some("active"));
            let start: usize = query.get("offset").unwrap().parse().unwrap();
            Value::Array(
                (start..start + 20)
                    .map(|n| {
                        json!({"uri":format!("{origin}/users/author{n}"),
                "statuses_count":if n%2==0 {1} else {10_000},"last_status_at":"2026-10-15",
                "created_at":"2020-01-01T00:00:00Z"})
                    })
                    .collect(),
            )
        }
        "/api/v3/community/list" => {
            assert_eq!(query.get("sort").map(AsRef::as_ref), Some("TopMonth"));
            json!({"communities":(0..20).map(|n|json!({"community":community(&origin,&format!("group{n}"))})).collect::<Vec<_>>()})
        }
        "/api/v3/community" => {
            json!({"community_view":{"community":community(&origin,query.get("name").unwrap())}})
        }
        "/api/v1/videos" => {
            json!({"total":100,"data":(0..8).map(|n|record(&origin,n)).collect::<Vec<_>>()})
        }
        _ if path.starts_with("/api/v1/videos/") => {
            let n = usize::from_str_radix(path.rsplit('-').next().unwrap(), 16).unwrap();
            record(&origin, n)
        }
        _ if path.starts_with("/videos/watch/") => {
            let n = usize::from_str_radix(path.rsplit('-').next().unwrap(), 16).unwrap();
            json!({"id":url.as_str(),"type":"Video","name":format!("Film {n}"),"content":"A public film",
                "published":"2026-01-01T00:00:00Z","to":[PUBLIC],"sensitive":false,
                "attributedTo":[format!("{origin}/accounts/{}",owner(n)),format!("{origin}/video-channels/{}",owner(n))]})
        }
        _ if path.starts_with("/accounts/") => {
            json!({"id":url.as_str(),"type":"Person","preferredUsername":"filmmaker"})
        }
        _ if path.starts_with("/video-channels/") => {
            json!({"id":url.as_str(),"type":"Group","name":"Film channel",
            "attributedTo":format!("{origin}/accounts/{}",path.rsplit('/').next().unwrap())})
        }
        _ if path.starts_with("/c/") => {
            json!({"id":url.as_str(),"type":"Group","preferredUsername":path.rsplit('/').next().unwrap(),"name":"A community","summary":"A public discussion"})
        }
        _ if path.ends_with("/outbox") => {
            let actor = url.as_str().split("/outbox").next().unwrap();
            if query.contains_key("page") {
                json!({"id":url.as_str(),"type":"OrderedCollectionPage","partOf":format!("{actor}/outbox"),
                    "orderedItems":(0..32).map(|n|json!({"type":"Create","actor":actor,"object":{"id":format!("{actor}/posts/{n}")}})).collect::<Vec<_>>()})
            } else {
                json!({"id":url.as_str(),"type":"OrderedCollection","totalItems":10_000,"first":format!("{actor}/outbox?page=true")})
            }
        }
        _ if path.contains("/posts/") => {
            json!({"id":url.as_str(),"type":"Note","attributedTo":url.as_str().split("/posts/").next().unwrap(),
            "content":"A public post","published":"2026-01-01T00:00:00Z","to":[PUBLIC]})
        }
        _ => {
            json!({"id":url.as_str(),"type":"Person","preferredUsername":path.rsplit('/').next().unwrap(),
            "name":"A public author","summary":"A public biography","outbox":format!("{url}/outbox")})
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
    let client = FederationClient::new("discovery-diversity-test", true)
        .unwrap()
        .with_proxies(&ProxyConfig {
            onion_proxy_url: Some(proxy.into()),
            ..Default::default()
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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let worked = worker::run_due(state).await.unwrap();
        let due:i64=sqlx::query_scalar("SELECT count(*) FROM discovery_jobs WHERE NOT withdrawn AND run_at<now()+interval '45 seconds'")
            .fetch_one(&state.pool).await.unwrap();
        if worked == 0 && due == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "bounded graph must settle"
        );
        if worked == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hundred_wire_attempts_reach_all_four_kinds_and_resume_catalogues(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let network = Arc::new(Network::default());
    let router = Router::new().fallback(respond).with_state(network.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut settings = db::settings(&pool).await.unwrap();
    settings.enabled = true;
    settings.requests_per_day = 100;
    settings.requests_per_origin = 25;
    settings.bytes_per_day = 16 * 1024 * 1024;
    db::save_settings(&pool, &settings).await.unwrap();
    let sources:Vec<_>=(0..3).flat_map(|n|[
        json!({"origin":format!("http://people{n}.onion")}),
        json!({"origin":format!("http://communities{n}.onion"),"endpoint":format!("http://communities{n}.onion/api/v3/community/list")}),
        json!({"origin":format!("http://videos{n}.onion"),"endpoint":format!("http://videos{n}.onion/api/v1/videos")}),
    ]).collect();
    assert_eq!(
        seeds::import(
            &pool,
            &json!({"version":1,"sources":sources}).to_string(),
            true,
            false
        )
        .await
        .unwrap()
        .admitted,
        9
    );
    let first = state(&pool, &proxy);
    drain(&first).await;
    drop(first);
    let counts:Vec<(String,i64)>=sqlx::query_as("SELECT kind,count(*) FROM discovery_entries WHERE state='active' GROUP BY kind ORDER BY kind").fetch_all(&pool).await.unwrap();
    assert_eq!(
        counts,
        vec![
            ("community".into(), 9),
            ("post".into(), 9),
            ("profile".into(), 9),
            ("video".into(), 6)
        ]
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 96);
    let posts:i64=sqlx::query_scalar("SELECT max(n) FROM (SELECT count(*) n FROM discovery_entries WHERE kind='post' GROUP BY actor_uri) s").fetch_one(&pool).await.unwrap();
    assert_eq!(
        posts, 1,
        "10000 advertised posts do not buy extra sampling slots"
    );
    sqlx::query("UPDATE discovery_sampling SET next_at=now() WHERE source_id IN (SELECT id FROM discovery_sources WHERE origin='http://people0.onion')").execute(&pool).await.unwrap();
    let restarted = state(&pool, &proxy);
    drain(&restarted).await;
    let diagnostic = db::diagnostics(&pool).await.unwrap();
    assert_eq!(diagnostic.requests_24h, 100);
    assert!(
        network
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.contains("people0.onion/api/v1/directory") && u.contains("offset=20"))
    );
    let counts = (
        db::diagnostics(&pool).await.unwrap().requests_24h,
        network.requests.lock().unwrap().len(),
    );
    worker::run_due(&restarted).await.unwrap();
    assert_eq!(
        counts,
        (
            db::diagnostics(&pool).await.unwrap().requests_24h,
            network.requests.lock().unwrap().len()
        )
    );
    assert!(
        db::origin_diagnostics(&pool)
            .await
            .unwrap()
            .iter()
            .all(|o| o.requests <= 25)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    println!(
        "diversity qualification: 96 wire attempts -> 9 people, 9 posts by distinct authors, 9 communities, 6 videos; continuation stops at 100"
    );
    server.abort();
}
