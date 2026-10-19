//! Bounded directory-to-canonical-actor journeys through the real HTTP worker.
mod common;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use plamenu::{
    AppState,
    discovery::{directory, profiles, worker},
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

const ORIGIN: &str = "http://directory-test.onion";
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
            if path == directory::PATH {
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
    fn page(&self, offset: u8, rows: Value) {
        let uri = directory::page_uri(&format!("{ORIGIN}{}", directory::PATH), offset).unwrap();
        let url = url::Url::parse(&uri).unwrap();
        self.set(&format!("{}?{}", url.path(), url.query().unwrap()), rows);
    }
    async fn drain(&self, limit: usize) {
        for _ in 0..limit {
            if worker::run_due(&self.state).await.unwrap() == 0 {
                break;
            }
        }
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
    let signer = common::RemoteUser::new("plamenu.test", "instance").signer();
    let client = FederationClient::new("discovery-directory-test", true)
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
    let db::Admission::Added(source) =
        db::add_source(&pool, &format!("{ORIGIN}{}", directory::PATH), None)
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
fn actor(name: &str) -> Value {
    json!({"id":format!("{ORIGIN}/users/{name}"),"type":"Person","preferredUsername":name,"name":"Canonical name","summary":"<p>Canonical <b>biography</b></p>","icon":{"type":"Image","url":"http://foreign.onion/avatar"}})
}
fn hint(name: &str) -> Value {
    json!({"uri":format!("{ORIGIN}/users/{name}"),"url":format!("{ORIGIN}/@{name}"),"display_name":"untrusted hint title","note":"untrusted hint biography"})
}
async fn recheck(f: &Fixture, name: &str) {
    assert!(matches!(
        db::request_profile_recheck(&f.state.pool, &format!("{ORIGIN}/users/{name}"))
            .await
            .unwrap(),
        db::Admission::Added(_)
    ));
    f.drain(2).await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn canonical_verification_preserves_missing_signals_and_has_no_imports(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0,json!([{ "url":format!("{ORIGIN}/@alice"),"discoverable":true,"display_name":"Wrong name"}]));
    let mut doc = actor("alice");
    doc["name"] = json!("Canonical <name>");
    doc["indexable"] = json!(false);
    doc["manuallyApprovesFollowers"] = json!(true);
    f.set("/@alice", doc.clone());
    f.set("/users/alice", doc);
    f.drain(5).await;
    let entry = profiles::eligible(&f.state, &format!("{ORIGIN}/users/alice"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.discoverable, "missing");
    assert_eq!(entry.indexable, "deny");
    assert_eq!(entry.title.as_deref(), Some("Canonical <name>"));
    assert_eq!(entry.summary.as_deref(), Some("Canonical biography"));
    assert!(
        db::entry(&pool, &format!("{ORIGIN}/@alice"))
            .await
            .unwrap()
            .is_none()
    );
    let counts=sqlx::query!(r#"SELECT (SELECT count(*) FROM accounts) AS "accounts!",(SELECT count(*) FROM statuses) AS "statuses!",(SELECT count(*) FROM follows) AS "follows!",(SELECT count(*) FROM notifications) AS "notifications!""#).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (
            counts.accounts,
            counts.statuses,
            counts.follows,
            counts.notifications
        ),
        (0, 0, 0, 0)
    );
    {
        let requests = f.peer.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(!requests[0].1);
        assert!(requests[1].1 && requests[2].1);
    }
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 3);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn directory_refusals_invalid_flags_private_profiles_and_foreign_ids_are_excluded(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    f.page(0,json!([hint("denied"),hint("invalid"),hint("private"),hint("foreign"),{"uri":"http://foreign.onion/users/bob"}]));
    let mut denied = actor("denied");
    denied["discoverable"] = json!(false);
    f.set("/users/denied", denied);
    let mut invalid = actor("invalid");
    invalid["discoverable"] = json!(null);
    f.set("/users/invalid", invalid);
    let mut private = actor("private");
    private["to"] = json!(["http://directory-test.onion/users/private/followers"]);
    f.set("/users/private", private);
    let mut foreign = actor("foreign");
    foreign["id"] = json!("http://foreign.onion/users/foreign");
    f.set("/users/foreign", foreign);
    // Exercise every authority failure independently of the random sample.
    f.page(0, json!([]));
    f.drain(1).await;
    for name in ["denied", "invalid", "private", "foreign"] {
        db::enqueue(
            &pool,
            f.source,
            &format!("{ORIGIN}/users/{name}"),
            db::Operation::RefreshEntry,
            None,
        )
        .await
        .unwrap();
    }
    f.drain(8).await;
    for name in ["denied", "invalid", "private", "foreign"] {
        assert!(
            profiles::eligible(&f.state, &format!("{ORIGIN}/users/{name}"))
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        db::profile_candidates(&pool)
            .await
            .unwrap()
            .iter()
            .all(|e| e.title.is_none() && e.summary.is_none())
    );
    assert_eq!(f.peer.requests.lock().unwrap().len(), 5);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn configured_missing_default_and_authoritative_reentry_are_applied(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", actor("alice"));
    let mut settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_discoverable = false;
    db::save_settings(&pool, &settings).await.unwrap();
    f.drain(3).await;
    let uri = format!("{ORIGIN}/users/alice");
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
    let mut allowed = actor("alice");
    allowed["discoverable"] = json!(true);
    f.set("/users/alice", allowed.clone());
    recheck(&f, "alice").await;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_some());
    f.set("/users/alice", json!({"id":uri,"type":"Tombstone"}));
    recheck(&f, "alice").await;
    assert_eq!(
        db::entry(&pool, &uri).await.unwrap().unwrap().state,
        "withdrawn"
    );
    assert_eq!(
        db::enqueue(&pool, f.source, &uri, db::Operation::RefreshEntry, None)
            .await
            .unwrap(),
        db::Admission::Suppressed
    );
    f.set("/users/alice", allowed);
    recheck(&f, "alice").await;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn refresh_expiry_access_denial_and_deletion_do_not_poison_other_profiles(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice"), hint("bob")]));
    f.set("/users/alice", actor("alice"));
    f.set("/users/bob", actor("bob"));
    f.drain(4).await;
    let uri = format!("{ORIGIN}/users/alice");
    f.status("/users/alice", StatusCode::FORBIDDEN);
    recheck(&f, "alice").await;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
    assert!(
        profiles::eligible(&f.state, &format!("{ORIGIN}/users/bob"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db::source(&pool, f.source)
            .await
            .unwrap()
            .unwrap()
            .cooldown_until
            .is_none()
    );
    f.set("/users/alice", actor("alice"));
    recheck(&f, "alice").await;
    sqlx::query!("UPDATE discovery_entries SET verified_at=clock_timestamp()-interval '2 days',valid_until=clock_timestamp()-interval '1 second',updated_at=clock_timestamp()-interval '24 hours' WHERE canonical_uri=$1",uri).execute(&pool).await.unwrap();
    f.status("/users/alice", StatusCode::BAD_GATEWAY);
    f.drain(2).await;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
    f.status("/users/bob", StatusCode::GONE);
    recheck(&f, "bob").await;
    assert_eq!(
        db::entry(&pool, &format!("{ORIGIN}/users/bob"))
            .await
            .unwrap()
            .unwrap()
            .state,
        "withdrawn"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn verified_revocation_fences_an_older_inflight_positive_result(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", actor("alice"));
    f.drain(3).await;
    let uri = format!("{ORIGIN}/users/alice");
    db::request_profile_recheck(&pool, &uri).await.unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() = Some(("/users/alice".into(), started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { worker::run_due(&state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let mut refusal = actor("alice");
    refusal["discoverable"] = json!(false);
    profiles::verified_update(&f.state, &uri, &refusal)
        .await
        .unwrap();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let entry = db::entry(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(entry.state, "denied");
    assert!(entry.title.is_none());
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_block_hides_and_invalidates_cached_profile_without_fetching(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", actor("alice"));
    f.drain(3).await;
    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "directory-test.onion",
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    let uri = format!("{ORIGIN}/users/alice");
    let before = db::diagnostics(&pool).await.unwrap().requests_24h;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
    assert!(
        db::entry(&pool, &uri)
            .await
            .unwrap()
            .unwrap()
            .summary
            .is_none()
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, before);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sampling_advances_after_a_bounded_round_instead_of_repeating_the_first_page(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for offset in [0_u8, 20] {
        let rows: Vec<_> = (offset..offset + 20)
            .map(|n| {
                let name = format!("person{n}");
                f.set(&format!("/users/{name}"), actor(&name));
                hint(&name)
            })
            .collect();
        f.page(offset, json!(rows));
    }
    f.drain(45).await;
    assert_eq!(db::profile_candidates(&pool).await.unwrap().len(), 3);
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 4);
    sqlx::query("UPDATE discovery_sampling SET next_at=now() WHERE source_id=$1")
        .bind(f.source)
        .execute(&pool)
        .await
        .unwrap();
    f.drain(8).await;
    assert_eq!(db::profile_candidates(&pool).await.unwrap().len(), 6);
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 8);
    assert_eq!(
        f.peer
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with(directory::PATH))
            .count(),
        2
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unavailable_directory_is_backed_off_without_claiming_capability(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let url =
        url::Url::parse(&directory::page_uri(&format!("{ORIGIN}{}", directory::PATH), 0).unwrap())
            .unwrap();
    f.status(
        &format!("{}?{}", url.path(), url.query().unwrap()),
        StatusCode::UNAUTHORIZED,
    );
    f.drain(3).await;
    let source = db::source(&pool, f.source).await.unwrap().unwrap();
    assert!(source.capabilities.is_empty());
    assert!(source.cooldown_until.is_some());
    assert_eq!(source.last_error.as_deref(), Some("access_denied"));
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 1);
}

#[test]
fn malformed_directory_shape_and_foreign_hints_never_gain_authority() {
    assert!(directory::parse_page(&json!({"accounts":[]}), ORIGIN).is_err());
    assert!(directory::parse_page(&json!([null]), ORIGIN).is_err());
    assert!(directory::parse_page(&json!(vec![hint("alice"); 21]), ORIGIN).is_err());
    let (hints,full)=directory::parse_page(&json!([hint("alice"),hint("alice"),{"uri":"https://elsewhere.example/actor"},{"url":"http://user:password@directory-test.onion/actor"}]),ORIGIN).unwrap();
    assert_eq!(hints.len(), 1);
    assert!(!full);
    assert!(directory::endpoint("https://peer.example/private/path").is_err());
}

/// Explicit opt-in: reads the configured development instance signer but writes
/// discovery state only to the disposable test database. No peer mutation.
#[sqlx::test(migrations = "../db/migrations")]
#[ignore = "requires the owned Mastodon peer and PLAMENU_DISCOVERY_LIVE_CONFIG"]
async fn owned_mastodon_directory_round_trip(pool: PgPool) {
    let path =
        std::env::var("PLAMENU_DISCOVERY_LIVE_CONFIG").expect("explicit local configuration path");
    let config = plamenu::Config::from_file(path).unwrap();
    let signer_pool = PgPool::connect(&config.database_url).await.unwrap();
    let keyring = Arc::new(plamenu::crypto::FederationKeyring::from_config(&config).unwrap());
    let records = plamenu_db::actor_key::usable_for_instance(&signer_pool)
        .await
        .unwrap();
    let rsa = plamenu::key_store::decrypt_record(
        &keyring,
        records
            .iter()
            .find(|k| k.algorithm == "rsa")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let signer = plamenu_federation::RequestSigner::from_pkcs8_pem(
        rsa.private.expose_str().unwrap(),
        rsa.record.key_uri,
    )
    .unwrap();
    let client = FederationClient::new(
        "plamenu-discovery-owned-peer-check",
        config.allow_private_fetch,
    )
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
    assert!(matches!(
        db::add_source(&pool, "https://mastodon.local/api/v1/directory", None)
            .await
            .unwrap(),
        db::Admission::Added(_)
    ));
    for _ in 0..5 {
        if worker::run_due(&state).await.unwrap() == 0 {
            break;
        }
    }
    let entries = db::profile_candidates(&pool).await.unwrap();
    assert!(
        !entries.is_empty(),
        "owned peer must publish at least one directory profile"
    );
    let mut eligible = 0;
    for entry in entries {
        if profiles::eligible(&state, &entry.canonical_uri)
            .await
            .unwrap()
            .is_some()
        {
            eligible += 1;
        }
    }
    assert!(eligible > 0);
    let accounts = sqlx::query_scalar!("SELECT count(*) FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, Some(0));
    println!(
        "owned peer verified: {eligible} eligible profiles, {} metered requests",
        db::diagnostics(&pool).await.unwrap().requests_24h
    );
    signer_pool.close().await;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn silenced_domain_is_excluded_even_though_it_can_still_federate(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", actor("alice"));
    f.drain(3).await;
    let uri = format!("{ORIGIN}/users/alice");
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_some());
    plamenu_db::instance_policy::create_domain_block(
        &pool,
        plamenu_db::instance_policy::NewDomainBlock {
            domain: "directory-test.onion",
            severity: "silence",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    assert!(
        plamenu::instance_policy::can_federate_url(&pool, "plamenu.test", &uri)
            .await
            .unwrap()
    );
    let before = db::diagnostics(&pool).await.unwrap().requests_24h;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
    assert!(
        db::entry(&pool, &uri)
            .await
            .unwrap()
            .unwrap()
            .title
            .is_none()
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, before);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn canonical_refusal_clears_old_text_even_when_profile_metadata_is_malformed(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", actor("alice"));
    f.drain(3).await;
    let uri = format!("{ORIGIN}/users/alice");
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_some());
    // Missing preferredUsername must not turn this explicit refusal into a
    // transient parse failure that leaves the old public card visible.
    f.set(
        "/users/alice",
        json!({"id":uri,"type":"Person","discoverable":false,"name":42}),
    );
    recheck(&f, "alice").await;
    let entry = db::entry(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(entry.discoverable, "deny");
    assert_eq!(entry.state, "denied");
    assert!(entry.title.is_none() && entry.summary.is_none());
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn first_fetch_tombstone_guard_requires_a_known_canonical_recheck(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let uri = format!("{ORIGIN}/users/alice");
    f.page(0, json!([hint("alice")]));
    f.set("/users/alice", json!({"id":uri,"type":"Tombstone"}));
    f.drain(3).await;
    assert!(db::entry(&pool, &uri).await.unwrap().is_none());
    assert_eq!(db::diagnostics(&pool).await.unwrap().withdrawal_guards, 1);
    assert_eq!(
        db::enqueue(&pool, f.source, &uri, db::Operation::RefreshEntry, None)
            .await
            .unwrap(),
        db::Admission::Suppressed
    );
    assert_eq!(
        db::request_profile_recheck(&pool, &format!("{ORIGIN}/users/unknown"))
            .await
            .unwrap(),
        db::Admission::Suppressed
    );
    f.set("/users/alice", actor("alice"));
    recheck(&f, "alice").await;
    assert!(profiles::eligible(&f.state, &uri).await.unwrap().is_some());
    assert_eq!(db::diagnostics(&pool).await.unwrap().withdrawal_guards, 0);
}
