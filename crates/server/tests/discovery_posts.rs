//! Outbox-to-canonical-post acquisition, local search and explicit-open reconciliation.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
};
use plamenu::{
    AppState,
    discovery::{directory, policy::Use, posts, profiles, worker},
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

const ORIGIN: &str = "http://posts-test.onion";
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
    json!({"id":format!("{ORIGIN}/users/{name}"),"type":"Person","preferredUsername":name,
        "tag":[{"type":"Hashtag","name":"#Gardens"}],"name":name,"outbox":format!("{ORIGIN}/users/{name}/outbox")})
}
fn note(name: &str, n: usize) -> Value {
    json!({"id":format!("{ORIGIN}/posts/{name}/{n}"),"type":"Note","attributedTo":format!("{ORIGIN}/users/{name}"),
        "to":["https://www.w3.org/ns/activitystreams#Public"],"published":"2026-01-01T12:00:00Z",
        "content":format!("<p>Canonical orchid {name} {n}</p><img src='http://foreign.onion/track'>"),
        "tag":[{"type":"Hashtag","name":"#Orchids"}],"attachment":[{"type":"Image","url":"http://foreign.onion/image"}]})
}
fn setup_author(f: &Fixture, name: &str, document: Value, count: usize) {
    f.set(&format!("/users/{name}"), document);
    let root = format!("{ORIGIN}/users/{name}/outbox");
    f.set(
        &format!("/users/{name}/outbox"),
        json!({"id":root,"type":"OrderedCollection","first":format!("{root}?page=true")}),
    );
    let items:Vec<_>=(0..count).map(|n|{
        let object=note(name,n);f.set(&format!("/posts/{name}/{n}"),object.clone());
        json!({"type":"Create","actor":format!("{ORIGIN}/users/{name}"),"object":{"id":object["id"],"content":"Untrusted directory text"}})
    }).collect();
    f.set(&format!("/users/{name}/outbox?page=true"),json!({"id":format!("{root}?page=true"),"type":"OrderedCollectionPage","partOf":root,"orderedItems":items,"next":"http://foreign.onion/crawl"}));
}
fn seed(f: &Fixture, names: &[&str]) {
    f.page(
        0,
        json!(
            names
                .iter()
                .map(|name| json!({"uri":format!("{ORIGIN}/users/{name}")}))
                .collect::<Vec<_>>()
        ),
    );
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
// Advance the fixture clock's daily sampling deadline, keeping canonical
// verification in the production worker for each additional post.
async fn next_author_round(f: &Fixture, name: &str) {
    sqlx::query("UPDATE discovery_entries SET outbox_next_at=now() WHERE canonical_uri=$1")
        .bind(format!("{ORIGIN}/users/{name}"))
        .execute(&f.state.pool)
        .await
        .unwrap();
    f.drain().await;
}
async fn first_post(f: &Fixture, name: &str) -> db::Entry {
    db::entry(&f.state.pool, &format!("{ORIGIN}/posts/{name}/0"))
        .await
        .unwrap()
        .unwrap()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn outbox_sampling_is_bounded_canonical_and_side_effect_free(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 8);
    let before = cold_counts(&pool).await;
    f.drain().await;
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_entries WHERE kind='post' AND state='active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(entries, 8);
    assert_eq!(cold_counts(&pool).await, before);
    let post = first_post(&f, "alice").await;
    assert_eq!(post.summary.as_deref(), Some("Canonical orchid alice 0"));
    assert_eq!(db::entry_topics(&pool, post.id).await.unwrap(), ["orchids"]);
    let actor = db::entry(&pool, post.actor_uri.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        db::entry_topics(&pool, actor.id).await.unwrap(),
        ["gardens"]
    );
    assert_eq!(post.indexable, "missing");
    let page = posts::page(&f.state, 1, None, "orchid").await.unwrap();
    assert_eq!(page.entries.len(), 4); // existing per-origin browse-page limit
    let next = posts::page(&f.state, 1, page.next, "orchid").await.unwrap();
    assert_eq!(next.entries.len(), 4);
    assert!(
        page.entries
            .iter()
            .all(|a| next.entries.iter().all(|b| a.id != b.id))
    );
    assert!(
        posts::page(&f.state, 1, None, "untrusted")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 14);
    let before = f.peer.requests.lock().unwrap().len();
    f.drain().await;
    assert_eq!(
        f.peer.requests.lock().unwrap().len(),
        before,
        "daily rounds do not repeat immediately"
    );
    let requests = f.peer.requests.lock().unwrap();
    assert_eq!(requests.len(), 14);
    for n in 0..8 {
        assert_eq!(
            requests
                .iter()
                .filter(|(p, _)| p == &format!("/posts/alice/{n}"))
                .count(),
            1
        );
    }
    assert!(requests.iter().all(|(p, s)| {
        if p.starts_with(directory::PATH) || p == "/api/v2/instance" || p == "/favicon.ico" {
            !*s
        } else {
            *s
        }
    }));
    assert!(
        !requests
            .iter()
            .any(|(p, _)| p.contains("crawl") || p.contains("image") || p.ends_with("/8"))
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn indexing_is_independent_of_profile_listing_and_recommendations(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["hidden", "refused", "invalid"]);
    let mut hidden = actor("hidden");
    hidden["discoverable"] = json!(false);
    hidden["indexable"] = json!(true);
    setup_author(&f, "hidden", hidden, 1);
    let mut refused = actor("refused");
    refused["indexable"] = json!(false);
    setup_author(&f, "refused", refused, 1);
    let mut invalid = actor("invalid");
    invalid["indexable"] = Value::Null;
    setup_author(&f, "invalid", invalid, 1);
    f.drain().await;
    assert_eq!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert!(
        posts::page(&f.state, 1, None, "")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        profiles::eligible(&f.state, &format!("{ORIGIN}/users/hidden"))
            .await
            .unwrap()
            .is_none()
    );
    {
        let requests = f.peer.requests.lock().unwrap();
        assert!(
            !requests
                .iter()
                .any(|(p, _)| p.contains("refused/outbox") || p.contains("invalid/outbox"))
        );
    }

    let mut settings = db::settings(&pool).await.unwrap();
    settings
        .disabled_mechanisms
        .extend(["profile_listing".into(), "recommendations".into()]);
    db::save_settings(&pool, &settings).await.unwrap();
    assert_eq!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    settings = db::settings(&pool).await.unwrap();
    settings
        .disabled_mechanisms
        .push("public_post_indexing".into());
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn canonical_visibility_and_authorship_refusals_never_become_documents(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 4);
    let mut unlisted = note("alice", 0);
    unlisted["cc"] = unlisted["to"].take();
    f.set("/posts/alice/0", unlisted);
    let mut foreign = note("alice", 1);
    foreign["attributedTo"] = json!(format!("{ORIGIN}/users/bob"));
    f.set("/posts/alice/1", foreign);
    let mut private = note("alice", 2);
    private["audience"] = json!(format!("{ORIGIN}/private-group"));
    f.set("/posts/alice/2", private);
    let mut refusal = note("alice", 3);
    refusal["indexable"] = json!(false);
    f.set("/posts/alice/3", refusal);
    f.drain().await;
    for _ in 0..3 {
        next_author_round(&f, "alice").await;
    }
    assert!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE kind='post'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(cold_counts(&pool).await, vec![0; 10]);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn missing_policy_tightening_erases_text_without_restore_on_relaxation(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice", "bob"]);
    setup_author(&f, "alice", actor("alice"), 1);
    let mut bob = actor("bob");
    bob["indexable"] = json!(true);
    setup_author(&f, "bob", bob, 1);
    f.drain().await;
    let post = first_post(&f, "alice").await;
    // The current actor can omit a previously explicit declaration while an
    // older excerpt still records explicit permission. Tightening must use both.
    f.set("/users/bob", actor("bob"));
    db::request_profile_recheck(&pool, &format!("{ORIGIN}/users/bob"))
        .await
        .unwrap();
    f.drain().await;
    assert_eq!(first_post(&f, "bob").await.indexable, "allow");
    let mut settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_indexable = false;
    db::save_settings(&pool, &settings).await.unwrap();
    let denied = db::entry_by_id(&pool, post.id).await.unwrap().unwrap();
    assert!(denied.summary.is_none());
    assert!(denied.title.is_none());
    assert!(first_post(&f, "bob").await.summary.is_none());
    settings = db::settings(&pool).await.unwrap();
    settings.allow_missing_indexable = true;
    db::save_settings(&pool, &settings).await.unwrap();
    assert!(
        posts::eligible(&f.state, 1, post.id, Use::IndexPost)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn actor_revocation_fences_inflight_post_and_removes_cached_excerpts(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 1);
    f.drain().await;
    let post = first_post(&f, "alice").await;
    db::request_profile_recheck(&pool, &post.canonical_uri)
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() =
        Some(("/posts/alice/0".into(), started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { worker::run_due(&state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let mut refusal = actor("alice");
    refusal["indexable"] = json!(false);
    profiles::verified_update(&f.state, &format!("{ORIGIN}/users/alice"), &refusal)
        .await
        .unwrap();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let post = db::entry_by_id(&pool, post.id).await.unwrap().unwrap();
    assert_eq!(post.state, "denied");
    assert!(post.summary.is_none());
    assert!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        profiles::eligible(&f.state, &format!("{ORIGIN}/users/alice"))
            .await
            .unwrap()
            .is_some(),
        "index opt-out leaves profile listing intact"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn post_updates_deletes_and_access_failures_erase_payload_and_guard_identity(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 3);
    f.drain().await;
    next_author_round(&f, "alice").await;
    let first = first_post(&f, "alice").await;
    assert!(
        !db::invalidate_owned_post(
            &pool,
            &format!("{ORIGIN}/users/bob"),
            &first.canonical_uri,
            true,
            "test"
        )
        .await
        .unwrap()
    );
    posts::verified_update(
        &f.state,
        &format!("{ORIGIN}/users/alice"),
        &note("alice", 0),
    )
    .await
    .unwrap();
    assert!(
        db::entry_by_id(&pool, first.id)
            .await
            .unwrap()
            .unwrap()
            .summary
            .is_none()
    );
    db::request_profile_recheck(&pool, &first.canonical_uri)
        .await
        .unwrap();
    f.drain().await;
    assert!(
        db::entry_by_id(&pool, first.id)
            .await
            .unwrap()
            .unwrap()
            .summary
            .is_some()
    );
    db::invalidate_owned_post(
        &pool,
        &format!("{ORIGIN}/users/alice"),
        &first.canonical_uri,
        true,
        "post_deleted",
    )
    .await
    .unwrap();
    assert_eq!(
        db::entry_by_id(&pool, first.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "withdrawn"
    );
    assert_eq!(
        db::enqueue(
            &pool,
            f.source,
            &first.canonical_uri,
            db::Operation::RefreshEntry,
            None
        )
        .await
        .unwrap(),
        db::Admission::Suppressed
    );
    let uri = format!("{ORIGIN}/posts/alice/1");
    f.status("/posts/alice/1", StatusCode::FORBIDDEN);
    db::request_profile_recheck(&pool, &uri).await.unwrap();
    f.drain().await;
    assert!(
        db::entry(&pool, &uri)
            .await
            .unwrap()
            .unwrap()
            .summary
            .is_none()
    );
}

#[test]
fn narrowed_audiences_and_invalid_declarations_are_not_public_posts() {
    assert!(posts::public_post(&note("alice", 0)));
    for (field, value) in [
        ("localOnly", json!(true)),
        ("visibility", json!("members")),
        ("audience", json!(["https://private.example/group"])),
        ("indexable", Value::Null),
        ("sensitive", json!("false")),
        ("bcc", json!(["https://one.example/hidden"])),
    ] {
        let mut document = note("alice", 0);
        document[field] = value;
        assert!(!posts::public_post(&document), "{field}");
    }
    let mut document = note("alice", 0);
    document["attributedTo"] = json!([
        format!("{ORIGIN}/users/alice"),
        format!("{ORIGIN}/users/bob")
    ]);
    assert!(posts::author_uri(&document).is_none());
}

#[test]
fn outbox_hints_ignore_boosts_foreign_origins_wrong_actors_and_duplicate_objects() {
    let actor = format!("{ORIGIN}/users/alice");
    let id = format!("{ORIGIN}/posts/alice/1");
    let page = json!({"orderedItems":[{"type":"Announce","actor":actor,"object":id},{"type":"Create","actor":"http://foreign.onion/actor","object":id},{"type":"Create","actor":actor,"object":"http://foreign.onion/post"},{"type":"Create","actor":actor,"object":id},{"type":"Create","actor":actor,"object":{"id":id}}]});
    assert_eq!(plamenu::discovery::outbox::hints(&page, &actor), vec![id]);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn pending_deletion_cancels_first_post_fetch_and_retains_guard(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 1);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    *f.peer.hold.lock().unwrap() =
        Some(("/posts/alice/0".into(), started.clone(), release.clone()));
    let state = f.state.clone();
    let task = tokio::spawn(async move { while worker::run_due(&state).await.unwrap() != 0 {} });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let uri = format!("{ORIGIN}/posts/alice/0");
    db::invalidate_owned_post(
        &pool,
        &format!("{ORIGIN}/users/alice"),
        &uri,
        true,
        "post_deleted",
    )
    .await
    .unwrap();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(db::entry(&pool, &uri).await.unwrap().is_none());
    assert_eq!(
        db::enqueue(&pool, f.source, &uri, db::Operation::RefreshEntry, None)
            .await
            .unwrap(),
        db::Admission::Suppressed
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_jobs WHERE withdrawn")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn mechanism_switches_and_shared_catalogue_allowance_bound_post_work(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 4);
    let mut settings = db::settings(&pool).await.unwrap();
    settings.disabled_mechanisms.push("outbox".into());
    settings.max_entries = 2;
    db::save_settings(&pool, &settings).await.unwrap();
    f.drain().await;
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, 4); // includes branding
    settings = db::settings(&pool).await.unwrap();
    settings.disabled_mechanisms.retain(|m| m != "outbox");
    db::save_settings(&pool, &settings).await.unwrap();
    f.drain().await;
    next_author_round(&f, "alice").await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM discovery_entries WHERE kind='post'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().entries, 2);
    let mut settings = db::settings(&pool).await.unwrap();
    settings.paused = true;
    db::save_settings(&pool, &settings).await.unwrap();
    let before = db::diagnostics(&pool).await.unwrap().requests_24h;
    assert_eq!(worker::run_due(&f.state).await.unwrap(), 0);
    assert_eq!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    // The retention trigger derives expiry from immutable first discovery,
    // rather than accepting an arbitrary legacy valid_until assignment.
    sqlx::query("UPDATE discovery_identity SET first_discovered_at=now()-interval '181 days' WHERE kind='post'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE discovery_entries SET valid_until=now()-interval '1 minute',verified_at=now()-interval '2 days' WHERE kind='post'").execute(&pool).await.unwrap();
    assert!(
        posts::page(&f.state, 1, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(db::diagnostics(&pool).await.unwrap().requests_24h, before);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn personal_dismissal_domain_blocks_and_content_filters_apply_without_import(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    seed(&f, &["alice"]);
    setup_author(&f, "alice", actor("alice"), 2);
    f.drain().await;
    for _ in 0..1 {
        next_author_round(&f, "alice").await;
    }
    let viewer = common::create_local_account(&pool, "reader", "Reader").await;
    let other = common::create_local_account(&pool, "other", "Other").await;
    let before = f.peer.requests.lock().unwrap().len();
    let post = first_post(&f, "alice").await;
    db::dismiss(&pool, viewer.id, post.id).await.unwrap();
    assert_eq!(
        posts::page(&f.state, viewer.id, None, "orchid")
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        posts::page(&f.state, other.id, None, "orchid")
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    plamenu_db::account_domain_block::create(&pool, viewer.id, "posts-test.onion")
        .await
        .unwrap();
    assert!(
        posts::page(&f.state, viewer.id, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    plamenu_db::account_domain_block::delete(&pool, viewer.id, "posts-test.onion")
        .await
        .unwrap();
    plamenu_db::custom_filter::create(
        &pool,
        viewer.id,
        "Plants",
        "hide",
        &["public".into()],
        None,
        &[plamenu_db::custom_filter::NewKeyword {
            keyword: "orchid".into(),
            whole_word: true,
        }],
    )
    .await
    .unwrap();
    plamenu::filters::invalidate(viewer.id);
    assert!(
        posts::page(&f.state, viewer.id, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(f.peer.requests.lock().unwrap().len(), before);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM accounts WHERE domain IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn explicit_open_reconciles_once_with_delivery_and_preserves_personal_search(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let member = session(&f.state).await;
    let remote = common::RemoteUser::new("posts-test.onion", "alice");
    let mut doc = actor("alice");
    let actor_uri = format!("{ORIGIN}/users/alice");
    doc["inbox"] = json!(format!("{actor_uri}/inbox"));
    doc["publicKey"] = json!({"id":format!("{actor_uri}#main-key"),"owner":actor_uri,"publicKeyPem":remote.keys.public_pem});
    seed(&f, &["alice"]);
    setup_author(&f, "alice", doc, 1);
    let mut object = note("alice", 0);
    object["content"] = json!("<p>A public orchid update</p>");
    object["attachment"] = json!([]);
    object["cc"] = json!(["https://plamenu.test/users/member"]);
    object["tag"] =
        json!([{"type":"Mention","href":"https://plamenu.test/users/member","name":"@member"}]);
    f.set("/posts/alice/0", object.clone());
    f.drain().await;
    let post = first_post(&f, "alice").await;
    let before = cold_counts(&pool).await;
    let (code, headers, body) =
        request(&f.state, &member, "GET", "/discover/posts?q=orchid", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(headers["cache-control"], "private, no-store");
    assert!(body.contains("A public orchid update"));
    assert!(body.contains("Open post"));
    assert_eq!(cold_counts(&pool).await, before);
    let path = format!("/web/discover/{}/open", post.id);
    assert_eq!(
        request(&f.state, &member, "POST", &path, &[("csrf", "bad")])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let opened = request(&f.state, &member, "POST", &path, &[("csrf", &member.csrf)]).await;
    assert_eq!(opened.0, StatusCode::SEE_OTHER);
    assert!(
        opened.1["location"]
            .to_str()
            .unwrap()
            .starts_with("/@alice@posts-test.onion/")
    );
    let stored = plamenu_db::status::find_by_uri(&pool, &post.canonical_uri)
        .await
        .unwrap()
        .unwrap();
    let author = plamenu_db::account::find_by_uri(&pool, &format!("{ORIGIN}/users/alice"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu_db::status::ingest_provenance(&pool, stored.id)
            .await
            .unwrap()
            .as_deref(),
        Some("explicit_resolution")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let first = plamenu::ingest::ingest_remote_note_delivery(&f.state, &author, &object)
        .await
        .unwrap();
    let replay = plamenu::ingest::ingest_remote_note_delivery(&f.state, &author, &object)
        .await
        .unwrap();
    assert_eq!(first.status.id, stored.id);
    assert_eq!(replay.status.id, stored.id);
    assert!(first.delivery_effects);
    assert!(!replay.delivery_effects);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications WHERE status_id=$1")
            .bind(stored.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    plamenu_db::block::create(&pool, author.id, member.viewer, None)
        .await
        .unwrap();
    assert!(
        posts::eligible(&f.state, member.viewer, post.id, Use::IndexPost)
            .await
            .unwrap()
            .is_none()
    );
    plamenu_db::block::delete(&pool, author.id, member.viewer)
        .await
        .unwrap();
    let mut refusal = actor("alice");
    refusal["indexable"] = json!(false);
    profiles::verified_update(&f.state, &author.uri.clone().unwrap(), &refusal)
        .await
        .unwrap();
    assert!(
        posts::page(&f.state, member.viewer, None, "orchid")
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let unavailable = request(
        &f.state,
        &member,
        "POST",
        &path,
        &[("csrf", &member.csrf), ("view", "posts"), ("q", "orchid")],
    )
    .await;
    assert_eq!(unavailable.0, StatusCode::SEE_OTHER);
    assert_eq!(
        unavailable.1["location"],
        "/discover/posts?notice=unavailable&q=orchid"
    );
    assert_eq!(
        request(&f.state, &member, "GET", "/discover/posts?q=%00", &[])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let personal = request(
        &f.state,
        &member,
        "GET",
        "/api/v2/search?q=orchid&type=statuses",
        &[],
    )
    .await;
    assert_eq!(personal.0, StatusCode::OK);
    let body: Value = serde_json::from_str(&personal.2).unwrap();
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1);
}
