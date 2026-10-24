//! Reproducible acquisition under the same 100-attempt pilot budget. No Internet.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response},
};
use plamenu::{
    AppState,
    discovery::{directory, seeds, worker},
    federation::HttpFederation,
};
use plamenu_db::{PgPool, discovery_store as db};
use plamenu_federation::{FederationClient, ProxyConfig};
use plamenu_test_support as common;
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
struct Exhausted {
    reason: &'static str,
    receipts: Value,
    deferred: HashMap<i64, time::OffsetDateTime>,
    pending: Vec<String>,
}
async fn wire_receipts(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY r.id),'[]'::jsonb) FROM discovery_requests r",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn pending_targets(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT target_uri FROM discovery_jobs WHERE NOT withdrawn
         UNION SELECT uri FROM discovery_candidates",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}
async fn global_exhaustion(state: &AppState, deadline: tokio::time::Instant) -> Option<Exhausted> {
    // Stop only for an allowance shared by every origin and purpose. Include
    // all possible pacing refill through the existing settlement deadline, so
    // per-origin/purpose deferrals never hide useful work elsewhere.
    let window = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .as_secs_f64();
    let (requests, limit, tokens): (i64, i64, f64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM discovery_requests WHERE started_at>clock_timestamp()-interval '24 hours'),
         s.requests_per_day,p.tokens+(GREATEST(EXTRACT(epoch FROM clock_timestamp()-p.refilled_at),0)::double precision+$1)*s.requests_per_day::double precision/86400
         FROM discovery_settings s CROSS JOIN discovery_pacing p WHERE s.singleton AND p.singleton",
    )
    .bind(window)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let reason = if requests >= limit {
        "rolling_allowance"
    } else if tokens < 1.0 {
        "shared_pacing"
    } else {
        return None;
    };
    let deferred: Vec<(i64, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT id,run_at FROM discovery_jobs WHERE NOT withdrawn AND last_error='budget_exhausted'",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    Some(Exhausted {
        reason,
        receipts: wire_receipts(&state.pool).await,
        deferred: deferred.into_iter().collect(),
        pending: pending_targets(&state.pool).await,
    })
}
async fn verified_denial(state: &AppState, before: &Exhausted) -> bool {
    // Execute a real worker turn even when globally blocked. Cached useful
    // work may finish first; settle only after a fresh durable budget denial.
    assert_eq!(wire_receipts(&state.pool).await, before.receipts);
    let deferred: Vec<(i64, time::OffsetDateTime, bool)> = sqlx::query_as(
        "SELECT id,run_at,attempts=0 AND lease_token IS NULL AND lease_owner IS NULL AND leased_until IS NULL
         FROM discovery_jobs WHERE NOT withdrawn AND last_error='budget_exhausted'",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    let denied = deferred.iter().any(|(id, at, released)| {
        *released && before.deferred.get(id).is_none_or(|previous| at > previous)
    });
    if denied {
        let pending = pending_targets(&state.pool).await;
        assert!(
            before.pending.iter().all(|uri| pending.contains(uri)),
            "settling exhausted work must not discard pending hints"
        );
        eprintln!(
            "drain settled after a real {} denial; every receipt and pending hint retained",
            before.reason
        );
    }
    denied
}

// Pump currently claimable work. Budget-deferred jobs remain durable; a hard
// allowance is not a promise that every hint in the generated graph can finish.
async fn drain(state: &AppState) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let exhausted = global_exhaustion(state, deadline).await;
        let worked = worker::run_due(state).await.unwrap();
        assert!(
            tokio::time::Instant::now() < deadline,
            "bounded graph must settle"
        );
        if let Some(before) = exhausted {
            assert_eq!(worked, 1, "exercise an actual exhausted-budget claim");
            if verified_denial(state, &before).await {
                break;
            }
        }
        if worked == 0 {
            break;
        }
    }
}
async fn refill_pacing(pool: &PgPool) {
    // Simulate elapsed token-refill time only. Do not refund a wire receipt,
    // its body bytes or the rolling request/origin allowance.
    sqlx::query("UPDATE discovery_pacing SET refilled_at=now()-interval '1 day'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE discovery_jobs SET run_at=now() WHERE NOT withdrawn AND last_error='budget_exhausted'")
        .execute(pool).await.unwrap();
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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
    // All fixture origins share the real, single hidden-service fetch lane.
    // Claim one job at a time so the acquisition test does not manufacture
    // thirty-second foreground-contention deferrals between its own jobs.
    // This tightens admission; it does not bypass either transport gate.
    settings.max_inflight = 1;
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
    let first_requests = db::diagnostics(&pool).await.unwrap().requests_24h;
    assert!(first_requests <= 100);
    assert_eq!(
        usize::try_from(first_requests).unwrap(),
        network.requests.lock().unwrap().len()
    );
    // Restart with a persisted full-page checkpoint. Pick the least-used people
    // origin, so the continuation itself still fits the strict 25/origin cap.
    let (source, origin): (i64, String) = sqlx::query_as("SELECT s.id,s.origin FROM discovery_sources s WHERE s.origin LIKE 'http://people%.onion' AND s.verified_at IS NOT NULL AND EXISTS(SELECT 1 FROM discovery_sampling p WHERE p.source_id=s.id) ORDER BY (SELECT count(*) FROM discovery_requests r WHERE r.origin=s.origin),s.origin LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    let checkpoint: i64 = sqlx::query_scalar(
        "SELECT active_offset::bigint FROM discovery_sampling WHERE source_id=$1",
    )
    .bind(source)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(checkpoint, 20);
    sqlx::query("UPDATE discovery_sampling SET next_at=now() WHERE source_id=$1")
        .bind(source)
        .execute(&pool)
        .await
        .unwrap();
    let restarted = state(&pool, &proxy);
    refill_pacing(&pool).await;
    // The production scheduler prioritizes already-admitted verification and
    // its bounded overflow queue. Finish real work to make a queue slot, then
    // admit this explicit next-page adapter pass without raising queue limits.
    let admission_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match db::enqueue(
            &pool,
            source,
            &format!("{origin}/api/v1/directory"),
            db::Operation::ListDirectory,
            None,
        )
        .await
        .unwrap()
        {
            db::Admission::Added(_) | db::Admission::Existing(_) => break,
            db::Admission::Capacity => {
                assert!(tokio::time::Instant::now() < admission_deadline);
                assert!(
                    worker::run_due(&restarted).await.unwrap() > 0,
                    "pending verification makes queue room"
                );
            }
            result => panic!("next-page admission: {result:?}"),
        }
    }
    let before_continuation = db::diagnostics(&pool).await.unwrap().requests_24h;
    let leases = db::claim_due_for(&pool, "restart-catalogue", &[db::Operation::ListDirectory])
        .await
        .unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].source_id, source);
    directory::fetch_page(&restarted, &leases[0]).await.unwrap();
    assert_eq!(
        db::diagnostics(&pool).await.unwrap().requests_24h,
        before_continuation + 1
    );
    assert!(
        network
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.starts_with(&format!("{origin}/api/v1/directory?"))
                && u.contains("offset=20"))
    );
    // The full page now contributes all novel hints. Exercise several paced
    // turns without increasing any hard limit or removing pending candidates.
    drain(&restarted).await;
    for _ in 0..3 {
        refill_pacing(&pool).await;
        drain(&restarted).await;
    }
    let diagnostic = db::diagnostics(&pool).await.unwrap();
    let purposes: Vec<(String, i64)> = sqlx::query_as(
        "SELECT purpose,count(*) FROM discovery_requests GROUP BY purpose ORDER BY purpose",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let pending: Vec<(String, Option<String>, i64)> = sqlx::query_as("SELECT operation,last_error,count(*) FROM discovery_jobs WHERE NOT withdrawn GROUP BY operation,last_error ORDER BY operation,last_error").fetch_all(&pool).await.unwrap();
    let outcomes: Vec<(String, i64)> = sqlx::query_as("SELECT kind,count(*) FROM discovery_entries WHERE state='active' GROUP BY kind ORDER BY kind").fetch_all(&pool).await.unwrap();
    eprintln!(
        "bounded acquisition: requests={} purposes={purposes:?} pending={pending:?} entries={outcomes:?}",
        diagnostic.requests_24h
    );
    assert_eq!(diagnostic.requests_24h, 100);
    assert_eq!(network.requests.lock().unwrap().len(), 100);
    let counts: Vec<(String, i64)> = sqlx::query_as("SELECT kind,count(*) FROM discovery_entries WHERE state='active' GROUP BY kind ORDER BY kind")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        counts
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["community", "post", "profile", "video"]
    );
    assert!(counts.iter().all(|(_, count)| *count > 0));
    let posts:i64=sqlx::query_scalar("SELECT max(n) FROM (SELECT count(*) n FROM discovery_entries WHERE kind='post' GROUP BY actor_uri) s").fetch_one(&pool).await.unwrap();
    assert!(
        (1..=32).contains(&posts),
        "10000 advertised posts cannot exceed the bounded 32-item page"
    );

    assert!(diagnostic.charged_bytes_24h <= 16 * 1024 * 1024);
    // Refill cannot bypass the unchanged rolling allowance.
    refill_pacing(&pool).await;
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
    let requests = network.requests.lock().unwrap();
    let mut fetched_posts = std::collections::HashSet::new();
    for uri in requests.iter().filter(|uri| uri.contains("/posts/")) {
        assert!(
            fetched_posts.insert(uri),
            "known unchanged posts are not reacquired"
        );
    }
    println!(
        "diversity qualification: all four kinds, durable catalogue continuation, 100 wire attempts, 25/origin, no duplicate post acquisition"
    );
    server.abort();
}
