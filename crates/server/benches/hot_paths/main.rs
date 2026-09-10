//! Hot-path latency benches: the full axum router driven in-process against
//! the persistent staging-scale `plamenu_bench` database (seeded on first
//! run — see `seed.rs`).
//!
//! Workflow (details in `bench/README.md`):
//! - `cargo bench -p plamenu` runs the suite;
//! - `bench/bench_budgets.py` checks medians against `bench/budgets.toml`;
//! - `cargo bench -p plamenu -- --save-baseline <name>` / `-- --baseline <name>` compares releases.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    reason = "deterministic bench math over small in-range values"
)]

#[path = "../../tests/common/mod.rs"]
mod common;
mod seed;

/// The same allocator the server declares (`main.rs`). glibc's malloc gives up
/// several percent under multithreaded load, and a benchmark that measures a
/// different allocator from the one production runs is measuring a different
/// program.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use criterion::{BatchSize, Criterion};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_secret};
use plamenu::state::AppState;
use plamenu_db::account::RemoteAccountData;
use plamenu_db::oauth::{self, NewApp};
use plamenu_db::{
    PgPool, account, lemmy_id, lemmy_inbox, lemmy_media, notification, post_read, status, user,
};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Runtime;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const SCOPES: &str = "read write follow push";
/// Sender notes pre-ingested for the `inbox_update_note` bench to edit.
const UPDATE_NOTES: u64 = 256;
static DATASET_DATE_ANCHOR: OnceLock<OffsetDateTime> = OnceLock::new();

/// Sender notes planted for `write/inbox_delete_note`. Unlike the edit targets
/// these are *consumed* — one per iteration — so the pool has to outlast the
/// run rather than wrap: a `Delete` naming an already-deleted note takes a much
/// cheaper path and would quietly drag the median down.
/// [`assert_write_invariants`] fails if a run ever reaches the end of it.
///
/// Sized against the *indexed* delete: migration 0034 took this benchmark from
/// 26.8 ms to ~3 ms, which took criterion from 110 iterations per run to ~950,
/// and the first post-index run ran off a 900-target pool. Planting is one
/// INSERT either way, so the headroom is free.
const DELETE_NOTES: usize = 4_000;

/// Throwaway remote accounts planted for `db_jobs/purge_account`, likewise one
/// per iteration. Each carries a small but real footprint (statuses, tag and
/// card usage rows) so the purge prices a cascade rather than an empty row.
/// Covers the three-second warm-up (including Criterion's doubling of batches)
/// and two-second measurement window without reusing a deleted account.
const PURGE_ACCOUNTS: usize = 4_000;
/// The domain those accounts live on, so `seed::reset_run_residue` can find
/// whatever a cancelled run left behind.
const PURGE_DOMAIN: &str = "bench-purge.invalid";

/// The three-arm search fixture: a term that matches accounts, statuses *and*
/// hashtags, so one benchmark assembles the whole response a client's search
/// screen asks for.
///
/// The dataset has no such term. Its account names come from one word list
/// (`zephyr…`), its statuses from another, its tags from a third, so
/// `api/search` was measuring the statuses arm plus two arms that matched
/// nothing (E7). Rather than redraw the dataset — a reseed invalidates every
/// budget in the suite — the harness plants its own small corner of it, the
/// same way the delete and purge benchmarks plant their pools.
const SEARCH_DOMAIN: &str = "bench-search.invalid";
const SEARCH_TERM: &str = "benchsearch";
/// Enough to fill the 20-account page and leave the ranker something to order.
const SEARCH_ACCOUNTS: usize = 60;
/// Held near the 902 statuses `zephyrite` matched, so the statuses arm of
/// `api/search` measures the same scale it always did and only the other two
/// arms changed.
const SEARCH_STATUSES: usize = 900;
/// A full page of prefix-matched tags, plus room to sort.
const SEARCH_TAGS: usize = 40;

/// Local replies planted under a writer-owned root for `ap/status_replies` —
/// one over `REPLIES_PER_PAGE` so the page is full and the collection emits a
/// `next` link, which is the shape a crawling peer walks.
///
/// They cannot go under the seeded deep thread: that is a *chain* (each reply
/// answers the previous one), so the root has exactly one direct child, and
/// hanging 61 more off it would rewrite `api/context_deep` as well.
const AP_REPLIES: usize = 61;

/// Writer-owned statuses planted for `write/post_delete`, consumed one per
/// iteration like the inbound delete pool. Sized against the fan-out: each
/// delete addresses ~1,860 inboxes, so iterations are expensive and few.
const POST_DELETE_TARGETS: usize = 600;

/// Pre-enqueued jobs for the three worker-drain benchmarks, each consumed a
/// batch at a time. Sized so a run cannot reach the end of its pool — past it
/// the drain finds nothing to do and returns immediately, which a latency
/// budget reads as a large improvement.
/// Allow for Criterion's doubling warm-up batches as well as measurement:
/// at the observed ~8 ms per delivery batch, even a 2x speed-up fits 1,200
/// batches. Link crawling (~3.6 ms) gets 4,000 batches for the same windows.
const DELIVERY_DRAIN_JOBS: usize = 24_000;
const LINK_CRAWL_DRAIN_JOBS: usize = 80_000;
// Keep the status population independent of queue capacity. More spare jobs
// must not broaden the workload; cycle over the same eligible statuses.
const LINK_CRAWL_TARGETS: i64 = 20_000;
const CLEANUP_SWEEP_STATUSES: usize = 6_000;
/// Inbox hosts the drain's jobs are spread over. `delivery::run_due` groups a
/// batch by inbox and runs up to 8 groups concurrently, so a single host would
/// measure the serial path only.
const DRAIN_HOSTS: usize = 40;
const DRAIN_DOMAIN_SUFFIX: &str = ".bench-deliver.invalid";
/// The local account the cleanup sweep deletes for. Its own account, not a
/// persona's: a sweep benchmark points at whatever it can find and *deletes*
/// it, so aiming it at seeded rows would fail the *next* run's drift check
/// rather than this one.
const CLEANUP_USERNAME: &str = "bench_cleanup";

/// Live `user`-stream subscribers for the wide arm of `stream/route_status`.
/// The narrow arm is one. What is budgeted is the ratio between them: the
/// routing loop renders one full status per recipient, serially, inside the
/// single-threaded listener task, so the cost per additional connected client
/// is the number that matters and it is the same on any machine.
///
/// Fifty, not the two hundred the plan sketched, purely for wall clock: each
/// recipient costs a full unbatched `render_status`, so 200 measured 1.24 s per
/// iteration and 26 s of run for a slope that 50 recovers just as well. The
/// benchmark's name carries the number, so raising it means renaming and
/// recalibrating — which is the honest cost of changing what is measured.
const STREAM_SUBSCRIBERS: usize = 50;

/// Criterion's defaults are 3 s warm-up + 5 s measurement per benchmark, which
/// put ~89% of the suite's wall clock into precision a 2x budget cannot
/// consume: 46 of 47 benchmarks sat inside 7.3-12.0 s despite medians spanning
/// four orders of magnitude. These floors keep the sample counts intact while
/// bringing the suite back under the threshold where people actually run it.
/// `configure_from_args` is applied *after*, so `--measurement-time` still
/// wins when a run wants more resolution.
const WARM_UP: Duration = Duration::from_secs(1);
const MEASUREMENT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Per-benchmark non-latency metrics
// ---------------------------------------------------------------------------

/// SQL statements observed while [`ARMED`] is set. Counting is deliberately
/// confined to the untimed pre-pass: see [`ArmedQueryCounter`].
static QUERIES: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

/// The armed sibling of `common::QueryCounter`.
///
/// Round-trip counts are the one budget here that means anything on another
/// machine, but a live `sqlx::query` subscriber would tax every timed
/// iteration. So this layer answers `Interest::never` while disarmed, and the
/// pre-pass calls [`tracing::callsite::rebuild_interest_cache`] on the way out
/// — after which every sqlx callsite is cached as never-interested and the
/// timed benches pay exactly what they paid with no subscriber installed at
/// all.
struct ArmedQueryCounter;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ArmedQueryCounter {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if ARMED.load(Ordering::Relaxed) && metadata.target().starts_with("sqlx::query") {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        ARMED.load(Ordering::Relaxed) && metadata.target().starts_with("sqlx::query")
    }

    fn on_event(
        &self,
        _event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        QUERIES.fetch_add(1, Ordering::Relaxed);
    }
}

/// What one request cost besides time. Both numbers are machine-independent,
/// which is what makes them worth gating on: a reintroduced per-row lookup
/// costs ~0.3 ms here and 10x that against a networked Postgres, and entity
/// bloat is otherwise a free action.
#[derive(Clone, Copy, serde::Serialize)]
struct CaseMetrics {
    queries: usize,
    bytes: usize,
}

/// Collected by the pre-pass, keyed by the criterion benchmark id
/// (`group/name`), and written to the run manifest.
static METRICS: Mutex<BTreeMap<String, CaseMetrics>> = Mutex::new(BTreeMap::new());

fn record_metrics(id: &str, queries: usize, bytes: usize) {
    METRICS
        .lock()
        .unwrap()
        .insert(id.to_owned(), CaseMetrics { queries, bytes });
}

struct Fixture {
    pool: PgPool,
    router: Router,
    /// Kept for the direct-call benches (trends refresh).
    state: AppState,
    sparse_token: String,
    dense_token: String,
    writer_token: String,
    sparse_account: i64,
    thread_root: i64,
    single_status: i64,
    prolific_account: i64,
    /// The seeded local group for the ranked-timeline benches.
    group_account: i64,
    /// The many-followers persona (followers/profile/AP benches).
    popular_account: i64,
    /// `bench_dense`'s 400-member list.
    list_id: i64,
    /// A mid-tail remote acct for the lookup bench.
    lookup_acct: String,
    rel_uri: String,
    /// The 5k-follower write persona, and the remote peer the inbox-ingest
    /// benches sign as — both needed by the write post-conditions.
    writer_account: i64,
    sender_account: i64,
    /// The remote peer the inbox-ingest benches sign as.
    sender: common::RemoteUser,
    /// Pre-fetched rows for the direct `render_statuses` bench.
    render_statuses: Vec<status::Status>,
    /// Remote public statuses the API favourite/boost write benches target
    /// (distinct per iteration, so no upsert short-circuits).
    fav_targets: Vec<i64>,
    boost_targets: Vec<i64>,
    /// Local status URIs the signed Like/Announce ingest benches reference.
    like_uris: Vec<String>,
    announce_uris: Vec<String>,
    /// Local statuses the reply benches answer: URIs for the signed ingest,
    /// ids for the API post. Distinct per iteration, so no two replies pile
    /// onto one parent and change what a thread costs mid-run.
    reply_uris: Vec<String>,
    reply_ids: Vec<i64>,
    /// The pre-ingested sender notes the Update bench edits.
    update_uris: Vec<String>,
    /// Sender notes the Delete bench consumes, one per iteration.
    delete_uris: Vec<String>,
    /// Throwaway remote actor URIs the purge bench consumes, one per iteration.
    purge_uris: Vec<String>,
    /// The reference instant the trends refresh scores against.
    trends_at: OffsetDateTime,
    /// `ap/status_note`: the path of a seeded local status carrying media and
    /// tags, so the Note builder assembles attachments and hashtags rather
    /// than the cheapest possible document.
    rich_note_path: String,
    /// `ap/status_replies`: the path of the planted 61-reply root.
    replies_path: String,
    /// Writer-owned statuses `write/post_delete` consumes, one per iteration.
    post_delete_ids: Vec<i64>,
    /// One writer-owned status `write/post_edit` rewrites over and over. Edits
    /// are not consuming — an edit of an edited status is the same work.
    post_edit_id: i64,
    /// Writer statuses the setup planted (the delete pool, the reply root and
    /// its replies, the edit target), which the write post-conditions have to
    /// account for before they can call the rest arithmetic.
    planted_writer_statuses: i64,
    /// Existing tag names the rich-ingest bench attaches, drawn from the seed
    /// so no benchmark's tag membership moves under it.
    rich_tags: Vec<String>,
    /// Local usernames the rich-ingest bench mentions. Deliberately not the
    /// read personas: a mention notifies, and the notification benches read
    /// the newest 40 rows.
    rich_mentions: Vec<String>,
    /// The throwaway local account `db_jobs/statuses_cleanup_sweep` deletes for.
    cleanup_account: i64,
    /// `bench_popular`'s local followers, which the streaming benchmark
    /// subscribes as live `user` streams, and one of that account's public
    /// statuses to route to them.
    stream_viewers: Vec<i64>,
    stream_status: i64,
}

fn main() {
    tracing::subscriber::set_global_default(
        <tracing_subscriber::Registry as Default>::default().with(ArmedQueryCounter),
    )
    .expect("install the bench subscriber");

    // Captured before anything runs, not at manifest-write time: a run takes
    // minutes, and a commit landing during one would otherwise have the
    // manifest name a revision the measurement never saw.
    let revision = Revision::current();
    let started_at = SystemTime::now();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let fixture = rt.block_on(setup());
    rt.block_on(collect_case_metrics(&fixture));

    let mut criterion = Criterion::default()
        .warm_up_time(WARM_UP)
        .measurement_time(MEASUREMENT)
        .configure_from_args();
    bench_api(&mut criterion, &rt, &fixture);
    bench_write(&mut criterion, &rt, &fixture);
    bench_web(&mut criterion, &rt, &fixture);
    bench_ap(&mut criterion, &rt, &fixture);
    bench_stream(&mut criterion, &rt, &fixture);
    bench_db(&mut criterion, &rt, &fixture);
    bench_micro(&mut criterion, &rt, &fixture);
    criterion.final_summary();
    write_run_manifest(started_at, &revision);
}

/// Sends one read case and drops the body — an uncounted warm-up.
async fn warm(fx: &Fixture, case: &ReadCase) {
    let response = fx
        .router
        .clone()
        .oneshot(build_request(case))
        .await
        .unwrap();
    let _ = response.into_body().collect().await;
}

/// The untimed pre-pass: sends every read case once, verifies it actually did
/// the work its budget assumes, and records what it cost besides time.
///
/// This runs before criterion so the counting subscriber can be retired before
/// the first timed iteration — see [`ArmedQueryCounter`].
async fn collect_case_metrics(fx: &Fixture) {
    let cases: Vec<(&str, ReadCase)> = api_cases(fx)
        .into_iter()
        .map(|case| ("api", case))
        .chain(web_cases(fx).into_iter().map(|case| ("web", case)))
        .chain(ap_cases(fx).into_iter().map(|case| ("ap", case)))
        .collect();

    // Warm every case first, uncounted. The budget describes the steady state,
    // and a process's first request pays one-off cache population (settings,
    // instance metadata, actor keys) that no subsequent request repeats — an
    // unwarmed count would bake that into the budget and then fail the day
    // someone reorders the case list.
    for (_, case) in &cases {
        warm(fx, case).await;
    }

    ARMED.store(true, Ordering::Relaxed);
    tracing::callsite::rebuild_interest_cache();

    // Problems are collected rather than asserted one at a time: a fixture
    // that drifted usually broke several cases, and each rebuild costs
    // minutes. One run should tell you everything that is wrong.
    let mut problems: Vec<String> = Vec::new();
    eprintln!(
        "\npre-pass over {} read cases\n{:<28}{:>9}{:>10}",
        cases.len(),
        "case",
        "queries",
        "bytes"
    );
    for (group, case) in &cases {
        // Warm again immediately before counting. Warming once up front is not
        // enough: `SettingsCache` expires on a five-second wall clock, and the
        // counted pass over ~50 cases outlives that, so whichever case happens
        // to be running when the TTL lapses pays one extra `instance_settings`
        // read. That made every settings-reading endpoint's count a function of
        // how long the cases *before* it took — `api/search_accounts` moved
        // 33 -> 34 with nothing on its own path touched, when `NoteBatch` took
        // ~300 ms out of the two AP collection cases. Re-warming per case keeps
        // a count a property of the endpoint.
        warm(fx, case).await;
        QUERIES.store(0, Ordering::Relaxed);
        let response = fx
            .router
            .clone()
            .oneshot(build_request(case))
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let queries = QUERIES.load(Ordering::Relaxed);
        let id = format!("{group}/{}", case.name);
        eprintln!("{id:<28}{queries:>9}{:>10}", body.len());

        if !status.is_success() {
            problems.push(format!(
                "{id}: request failed: {status} {}",
                String::from_utf8_lossy(&body)
            ));
            continue;
        }
        if body.len() < case.min_len {
            problems.push(format!(
                "{id}: {} byte body is under the {} byte floor — the endpoint \
                 returned nothing meaningful, which a latency budget would \
                 read as a speedup",
                body.len(),
                case.min_len
            ));
        }
        let text = String::from_utf8_lossy(&body);
        for needle in &case.must_contain {
            if !text.contains(needle) {
                problems.push(format!(
                    "{id}: response is missing {needle:?} — it rendered, but not \
                     the thing this benchmark claims to measure"
                ));
            }
        }
        record_metrics(&id, queries, body.len());
    }
    assert!(
        problems.is_empty(),
        "{} benchmark fixture(s) are not measuring what they claim:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );

    ARMED.store(false, Ordering::Relaxed);
    // Retires every sqlx callsite back to `Interest::never`, so the timed
    // benches below run exactly as they did with no subscriber installed.
    tracing::callsite::rebuild_interest_cache();
}

/// Writes what this run measured beyond criterion's own output, next to it.
///
/// `bench/bench_budgets.py` promotes this into the committed
/// `bench/results/<sha>-<ts>.json` record. It exists mostly for `started_at`:
/// criterion never removes a benchmark directory, so without a run boundary
/// the gate cannot tell a fresh measurement from one left over from a filtered
/// run three commits ago — and it was grading the leftovers as if they were
/// evidence about the current tree.
fn write_run_manifest(started_at: SystemTime, revision: &Revision) {
    let path = criterion_dir().join("plamenu-bench-run.json");
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("could not create {}: {error}", parent.display());
        return;
    }
    let manifest = json!({
        "seed_version": seed::SEED_VERSION,
        "git_sha": revision.sha,
        "git_dirty": revision.dirty,
        "started_at": rfc3339(started_at),
        "finished_at": rfc3339(SystemTime::now()),
        "warm_up_secs": WARM_UP.as_secs_f64(),
        "measurement_secs": MEASUREMENT.as_secs_f64(),
        "cases": &*METRICS.lock().unwrap(),
    });
    match std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()) {
        Ok(()) => eprintln!("wrote {}", path.display()),
        Err(error) => eprintln!("could not write {}: {error}", path.display()),
    }
}

/// Where criterion put this run's output.
///
/// Cargo runs a bench binary with the *package* as its working directory, so a
/// bare `target/criterion` lands in `crates/server/target` — next to nothing,
/// while criterion writes to the workspace target directory. Resolved the same
/// way criterion resolves it: `CARGO_TARGET_DIR` if set, otherwise the
/// `target` directory of the workspace root (the ancestor holding
/// `Cargo.lock`).
fn criterion_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return std::path::PathBuf::from(dir).join("criterion");
    }
    let mut dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("Cargo.lock").is_file() {
            return dir.join("target").join("criterion");
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return std::path::PathBuf::from("target/criterion"),
        }
    }
}

fn rfc3339(at: SystemTime) -> String {
    OffsetDateTime::from(at).format(&Rfc3339).unwrap()
}

/// The revision these numbers describe. Recorded here rather than by the
/// checker so the manifest is self-describing however the bench was launched,
/// and so a result measured before a rebase cannot be graded as if it were
/// evidence about the current tree.
/// The revision a run's numbers describe.
///
/// `dirty` is not optional detail: the wave that introduced this record
/// measured itself with every change still uncommitted, so the manifest named
/// the *previous* commit and described something else. A record that cannot
/// say "these numbers are not true of that revision" is worse than no record.
struct Revision {
    sha: Option<String>,
    dirty: Option<bool>,
}

impl Revision {
    fn current() -> Self {
        Self {
            sha: git(&["rev-parse", "HEAD"]).map(|sha| sha.trim().to_owned()),
            dirty: git(&["status", "--porcelain", "--untracked-files=no"])
                .map(|out| !out.trim().is_empty()),
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
}

async fn setup() -> Fixture {
    let seed = seed::open().await;
    let pool = seed.pool.clone();

    // Cleanup first, then the peer: the reset deletes the whole
    // `bench-peer.invalid` account, which is the cheapest way to take its
    // statuses, favourites, notifications and mentions with it — and the
    // keypair is minted fresh per process anyway, so it is re-upserted
    // immediately afterwards. The drift check sits between them, where the
    // dataset is supposed to be exactly what it was seeded as.
    seed::reset_run_residue(&pool, seed.writer, seed.popular).await;
    seed::assert_dataset_unchanged(&pool).await;

    // The seed persists across days. Anchor run fixtures to its newest real
    // post, otherwise `now() - 1 day` eventually overtakes the seeded feed.
    let dataset_head = sqlx::query_scalar(
        "SELECT max(s.sort_at) FROM statuses s JOIN accounts a ON a.id = s.account_id
         WHERE a.domain LIKE 'd%.bench.invalid'",
    )
    .fetch_one(&pool)
    .await
    .expect("read the seeded timeline date");
    DATASET_DATE_ANCHOR
        .set(dataset_head)
        .expect("initialize the fixture date once");

    // The persistent seed predates normalized encrypted keys, while this
    // benchmark constructs AppState directly instead of going through server
    // startup. Provision the two local actors that actually sign/serialize in
    // measured cases so the suite measures production's normalized path, not
    // the rollback fallback (which costs an extra query and cannot deliver).
    ensure_bench_local_keys(&pool, seed.writer).await;
    ensure_bench_local_keys(&pool, seed.popular).await;
    let bench_ring = plamenu::crypto::FederationKeyring::from_config(&common::test_config())
        .expect("bench federation keyring");
    plamenu::key_store::ensure_instance(&pool, &bench_ring, common::TEST_DOMAIN)
        .await
        .expect("provision benchmark instance keys");

    let sender = common::RemoteUser::new("bench-peer.invalid", "bench_sender");
    let sender_account = upsert_sender(&pool, &sender).await;
    plamenu_db::actor_key::replace_remote(
        &pool,
        sender_account,
        &sender.actor.id,
        &[plamenu_db::actor_key::NewPublicKey {
            key_uri: sender.actor.public_key.id.clone(),
            controller_uri: sender.actor.id.clone(),
            algorithm: "rsa".to_owned(),
            public_key: sender.keys.public_pem.clone(),
            source: "bench-sender".to_owned(),
            expires_at: None,
        }],
    )
    .await
    .expect("normalize the benchmark sender key");

    let federation = common::StubFederation::with_users(&[&sender]);
    // A real on-disk store rather than the in-memory one the tests use: the
    // dataset now carries attachments that finished downloading, and a store
    // whose keys do not exist is a dataset that disagrees with itself. The
    // files are planted below.
    let media_root = std::env::temp_dir().join("plamenu-bench-media");
    std::fs::create_dir_all(&media_root).expect("create the bench media directory");
    let mut state = common::test_state_with_store(
        pool.clone(),
        federation,
        std::sync::Arc::new(
            plamenu::storage::LocalDiskStore::new(media_root.clone())
                .expect("open the bench media store"),
        ),
    );
    // Pin the settings cache for the whole run: under the production 5-second
    // TTL, one `instance_settings` refresh lands inside whichever counted case
    // straddles the boundary, moving that case's query count by one — the
    // per-case re-warm narrows but cannot close that race. A count must be a
    // property of the endpoint, not of the wall clock.
    state.settings_cache = std::sync::Arc::new(plamenu::state::SettingsCache::pinned());
    let router = plamenu::build_router(state.clone());
    plant_cached_media(&pool, &media_root).await;

    // Rank the trends once so the trends API bench reads a populated table
    // (the db bench then measures the refresh itself).
    let trends_at = seed::trends_reference(&pool).await;
    let started = std::time::Instant::now();
    plamenu::trends::refresh_tags_at(&state, trends_at)
        .await
        .unwrap();
    eprintln!("setup: trends refresh in {:.0?}", started.elapsed());

    let app = oauth::create_app(
        &pool,
        NewApp {
            name: "bench",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: SCOPES,
        },
    )
    .await
    .unwrap();

    let rel_uri = seed
        .rel_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let sep = if i == 0 { '?' } else { '&' };
            format!("{sep}id[]={id}")
        })
        .fold("/api/v1/accounts/relationships".to_owned(), |uri, part| {
            uri + &part
        });
    let render_statuses = status::find_by_ids(&pool, &seed.render_ids).await.unwrap();

    // Distinct write-bench targets: remote public originals for the API
    // favourite/boost benches, local public originals for the signed
    // Like/Announce ingest benches (their objects resolve without network).
    // Moderated authors excluded: interacting with a suspended/silenced or
    // domain-blocked author's status is refused, and the seed deliberately
    // moderates a slice of the fleet.
    let remote_targets: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM statuses s
         JOIN accounts a ON a.id = s.account_id
         WHERE s.uri IS NOT NULL AND s.reblog_of_id IS NULL AND s.visibility = 'public'
           AND a.suspended_at IS NULL AND a.silenced_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM domain_blocks db WHERE db.domain = a.domain)
         ORDER BY s.id DESC LIMIT 40000",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let (fav_targets, boost_targets) = remote_targets.split_at(remote_targets.len() / 2);
    // Keep these top-level as well as public. The synthetic seed contains a
    // handful of public replies whose conversation root is unlisted; replying
    // to one correctly clamps the new reply to unlisted and omits relay
    // inboxes, which makes the fan-out invariant compare different audiences
    // instead of detecting lost jobs.
    let local_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT s.id, a.username FROM statuses s
         JOIN accounts a ON a.id = s.account_id
         WHERE s.uri IS NULL AND s.reblog_of_id IS NULL AND s.visibility = 'public'
           AND s.in_reply_to_id IS NULL
         ORDER BY s.id DESC LIMIT 16000",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let local_uris: Vec<String> = local_rows
        .iter()
        .map(|(id, username)| {
            format!(
                "https://{}/users/{username}/statuses/{id}",
                common::TEST_DOMAIN
            )
        })
        .collect();
    // Three disjoint slices, one per interaction kind, so no two benchmarks
    // are aiming at the same status: a reply changes what its parent's thread
    // costs, and a second Like of the same object is a no-op.
    let third = local_uris.len() / 3;
    let (like_uris, rest) = local_uris.split_at(third);
    let (announce_uris, reply_uris) = rest.split_at(third);
    let reply_ids: Vec<i64> = local_rows[third * 2..].iter().map(|(id, _)| *id).collect();

    // Notes for the Update bench to edit, ingested through the real inbox.
    let mut update_uris = Vec::new();
    for i in 0..UPDATE_NOTES {
        let uri = format!("{}/bench-edit-note-{i}", sender.actor.id);
        let create = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{uri}/activity"),
            "type": "Create",
            "actor": sender.actor.id,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": note_object(&sender, &uri, "<p>bench editable note</p>", None)
                .with_published(backdated(i)),
        });
        send(&router, signed_inbox(&sender, &create), 0).await;
        update_uris.push(uri);
    }

    let delete_uris = plant_delete_targets(&pool, sender_account, &sender.actor.id).await;
    let purge_uris = plant_purge_targets(&pool).await;
    plant_search_corpus(&pool).await;
    plant_status_trends(&pool).await;
    let (replies_root, replies) = plant_reply_collection(&pool, seed.writer).await;
    let post_delete_ids = plant_post_delete_targets(&pool, seed.writer).await;
    let post_edit_id = plant_edit_target(&pool, seed.writer).await;
    let cleanup_account = plant_cleanup_account(&pool).await;

    // The richest local status the AP Note builder can be pointed at: media
    // and tags both go through their own per-status queries, and a status
    // carrying neither measures the document and nothing that hangs off it.
    let (rich_note_author, rich_note_id): (String, i64) = sqlx::query_as(
        "SELECT a.username, s.id FROM statuses s
         JOIN accounts a ON a.id = s.account_id
         WHERE a.domain IS NULL AND s.uri IS NULL AND s.visibility = 'public'
           AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
           AND EXISTS (SELECT 1 FROM media_attachments m WHERE m.status_id = s.id)
           AND EXISTS (SELECT 1 FROM status_tags t WHERE t.status_id = s.id)
         ORDER BY s.id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("a local public status with media and tags");

    // Tag names taken from the dataset rather than invented: a new tag is a
    // new row in `tags`, and an invented one would also have to be swept up
    // after every run. `benchtag` is excluded by the offset — it is the one
    // `api/tag_timeline` reads.
    let rich_tags: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM tags WHERE name <> 'benchtag' ORDER BY name OFFSET 500 LIMIT 3",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let rich_mentions: Vec<String> = sqlx::query_scalar(
        "SELECT username FROM accounts
         WHERE domain IS NULL AND username LIKE 'bench_local%'
         ORDER BY username LIMIT 2",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    // The wide arm of the streaming benchmark needs recipients, and a
    // recipient is a *follower* with a live user stream — so the routed status
    // has to belong to the one persona with a local follower crowd.
    let stream_viewers: Vec<i64> = sqlx::query_scalar(
        "SELECT f.account_id FROM follows f
         JOIN accounts follower ON follower.id = f.account_id
         WHERE f.target_account_id = $1 AND NOT f.pending AND follower.domain IS NULL
         ORDER BY f.account_id LIMIT $2",
    )
    .bind(seed.popular)
    .bind(STREAM_SUBSCRIBERS as i64)
    .fetch_all(&pool)
    .await
    .unwrap();
    let stream_status: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses
         WHERE account_id = $1 AND visibility = 'public' AND reblog_of_id IS NULL
           AND deleted_at IS NULL
         ORDER BY id DESC LIMIT 1",
    )
    .bind(seed.popular)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Everything the run depends on now exists, including the notes just
    // ingested — so this is the point where "is the dataset the right shape"
    // can be answered. The contention driver does not run it: it needs the
    // trends refresh above, which only this driver performs.
    seed::verify(&pool).await;

    Fixture {
        sparse_token: mint_token(&pool, app.id, seed.sparse).await,
        dense_token: mint_token(&pool, app.id, seed.dense).await,
        writer_token: mint_token(&pool, app.id, seed.writer).await,
        sparse_account: seed.sparse,
        thread_root: seed.thread_root,
        single_status: seed.single_status,
        prolific_account: seed.prolific_account,
        group_account: seed.group_account,
        popular_account: seed.popular,
        list_id: seed.list_id,
        lookup_acct: seed.lookup_acct.clone(),
        rel_uri,
        writer_account: seed.writer,
        sender_account,
        sender,
        render_statuses,
        fav_targets: fav_targets.to_vec(),
        boost_targets: boost_targets.to_vec(),
        like_uris: like_uris.to_vec(),
        announce_uris: announce_uris.to_vec(),
        reply_uris: reply_uris.to_vec(),
        reply_ids,
        update_uris,
        delete_uris,
        purge_uris,
        trends_at,
        rich_note_path: format!("/users/{rich_note_author}/statuses/{rich_note_id}"),
        replies_path: format!("/users/bench_writer/statuses/{replies_root}/replies"),
        planted_writer_statuses: 1 + replies + post_delete_ids.len() as i64 + 1,
        post_delete_ids,
        post_edit_id,
        rich_tags,
        rich_mentions,
        cleanup_account,
        stream_viewers,
        stream_status,
        state,
        router,
        pool,
    }
}

async fn ensure_bench_local_keys(pool: &PgPool, account_id: i64) {
    if plamenu_db::actor_key::usable_for_account(pool, account_id)
        .await
        .expect("read benchmark actor keys")
        .iter()
        .any(|key| key.algorithm == "rsa" && key.encrypted_private_key.is_some())
    {
        return;
    }
    let account = account::find_by_id(pool, account_id)
        .await
        .expect("read benchmark account")
        .expect("benchmark account exists");
    let rsa = plamenu_ap::keys::generate_keypair().expect("generate benchmark RSA key");
    let ed = plamenu_ap::keys::generate_ed25519_keypair();
    // Keep the classic public compatibility copies aligned with the
    // normalized keys. Private material goes only into the encrypted store.
    sqlx::query("UPDATE accounts SET public_key = $2, ed25519_public_key = $3 WHERE id = $1")
        .bind(account_id)
        .bind(&rsa.public_pem)
        .bind(&ed.public_multibase)
        .execute(pool)
        .await
        .expect("update benchmark public compatibility keys");
    let ring = plamenu::crypto::FederationKeyring::from_config(&common::test_config())
        .expect("bench federation keyring");
    plamenu::key_store::provision_account(pool, &ring, common::TEST_DOMAIN, &account, &rsa, &ed)
        .await
        .expect("provision normalized benchmark account keys");
}

/// Writes a byte for every file name the dataset claims to have cached.
///
/// The seed sets the columns — that is what the render path branches on — but
/// the store lives outside the database and does not survive a `cargo clean` or
/// a different machine, so it is reconciled here rather than seeded. Cheap: a
/// few hundred one-byte files, skipped when they already exist.
async fn plant_cached_media(pool: &PgPool, root: &std::path::Path) {
    let names: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT file_name, small_file_name FROM media_attachments
         WHERE file_name IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut planted = 0_usize;
    for name in names.into_iter().flat_map(|(main, small)| [main, small]) {
        let Some(name) = name else { continue };
        let path = root.join(&name);
        if !path.exists() {
            std::fs::write(&path, b"\0").expect("plant a bench media file");
            planted += 1;
        }
    }
    if planted > 0 {
        eprintln!(
            "setup: planted {planted} cached media file(s) in {}",
            root.display()
        );
    }
}

/// Plants the notes `write/inbox_delete_note` consumes.
///
/// Written straight to the database rather than ingested through the inbox: the
/// measurement is the *delete*, and a signed round trip per target would add
/// ~15 s of setup to every run — including the filtered spot-checks — to
/// produce a row shape one INSERT already produces.
///
/// `unlisted` and backdated, both deliberately. Thousands of planted rows at
/// `public`/`now()` walk straight onto the head of the federated timeline, and
/// a benchmark paging the harness's own filler instead of the dataset is B6 all
/// over again — it showed up as `api/public_federated` losing half its round
/// trips and a third of its bytes with no code change. Visibility does not
/// affect what a delete costs: `stub_by_uri` resolves by URI and cascades the
/// same rows either way.
async fn plant_delete_targets(pool: &PgPool, sender_account: i64, actor_uri: &str) -> Vec<String> {
    let ids: Vec<i64> = (0..DELETE_NOTES).map(|_| plamenu_db::id::next()).collect();
    let uris: Vec<String> = (0..DELETE_NOTES)
        .map(|n| format!("{actor_uri}/bench-delete-note-{n}"))
        .collect();
    // Past the dataset's own 180-day window, so they cannot reach the head of
    // any timeline even if a future benchmark reads unlisted rows.
    let offsets: Vec<i32> = (0..DELETE_NOTES).map(|n| 200 + (n % 170) as i32).collect();
    sqlx::query(
        "INSERT INTO statuses (id, uri, account_id, content, created_at, sort_at, visibility)
         SELECT t.id, t.uri, $4, '<p>bench delete target</p>',
                now() - make_interval(days => t.age), now() - make_interval(days => t.age),
                'unlisted'
         FROM unnest($1::bigint[], $2::text[], $3::int[]) AS t(id, uri, age)",
    )
    .bind(&ids)
    .bind(&uris)
    .bind(&offsets)
    .bind(sender_account)
    .execute(pool)
    .await
    .expect("plant the delete-bench targets");
    uris
}

/// Plants the throwaway accounts `db_jobs/purge_account` consumes.
///
/// One `DELETE FROM accounts` makes Postgres walk every table that references
/// it, so the cost is paid whether or not the account has any rows there — but
/// a purge of an empty account would price only the scans and none of the
/// cascade, so each of these carries a small real footprint. Set-based on
/// purpose: four statements regardless of the size of the target pool.
async fn plant_purge_targets(pool: &PgPool) -> Vec<String> {
    let n = PURGE_ACCOUNTS as i32;
    let account_ids: Vec<i64> = (0..PURGE_ACCOUNTS)
        .map(|_| plamenu_db::id::next())
        .collect();
    let uris: Vec<String> = (0..PURGE_ACCOUNTS)
        .map(|i| format!("https://{PURGE_DOMAIN}/users/purge{i}"))
        .collect();
    let usernames: Vec<String> = (0..PURGE_ACCOUNTS).map(|i| format!("purge{i}")).collect();
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, uri, public_key, inbox_url, shared_inbox_url)
         SELECT t.id, t.username, $4, t.uri, '', t.uri || '/inbox',
                'https://' || $4 || '/inbox'
         FROM unnest($1::bigint[], $2::text[], $3::text[]) AS t(id, uri, username)",
    )
    .bind(&account_ids)
    .bind(&uris)
    .bind(&usernames)
    .bind(PURGE_DOMAIN)
    .execute(pool)
    .await
    .expect("plant the purge-bench accounts");

    // Five statuses, eight tag-usage rows and three card-usage rows each — three
    // of the tables whose foreign keys had no index, plus the statuses whose own
    // delete cascade the purge also pays. Ids are minted in Rust rather than
    // derived from the account id: a snowflake is already 48 bits of
    // milliseconds over 16 of sequence, and arithmetic on one produces a number
    // that is neither unique nor time-ordered.
    let status_ids: Vec<i64> = (0..PURGE_ACCOUNTS * 5)
        .map(|_| plamenu_db::id::next())
        .collect();
    // `unlisted`, and dated well before the dataset's 180-day window: the first
    // version of this stamped them `public` at `now()` and 1,500 of them went
    // straight to the head of the federated timeline, which showed up as
    // `api/public_federated` shedding half its round trips and a third of its
    // bytes while nothing about the endpoint had changed.
    sqlx::query(
        "INSERT INTO statuses (id, uri, account_id, content, created_at, sort_at, visibility)
         SELECT t.id, a.uri || '/statuses/' || s.n, a.id, '<p>bench purge target</p>',
                now() - make_interval(days => 200 + s.n * 7),
                now() - make_interval(days => 200 + s.n * 7),
                'unlisted'
         FROM (SELECT id, uri, row_number() OVER (ORDER BY id) - 1 AS ord
               FROM accounts WHERE domain = $2) a
         CROSS JOIN generate_series(0, 4) AS s(n)
         JOIN unnest($1::bigint[]) WITH ORDINALITY AS t(id, pos)
              ON t.pos = a.ord * 5 + s.n + 1",
    )
    .bind(&status_ids)
    .bind(PURGE_DOMAIN)
    .execute(pool)
    .await
    .expect("plant purge-target statuses");
    // `current_date - 400` and older, not recent days. Trending scores tag usage
    // *within one day* and its threshold is five distinct accounts, so dating
    // these anywhere near the trends reference day hands eight real tags 1,500
    // distinct users each and rewrites the trending set — which is what happened
    // on the first run of this, moving `api/trends_tags` from 0.68 ms to 1.68 ms
    // with nothing in the endpoint touched. The purge cost is the same either
    // way: cascading a row does not care what day it names.
    sqlx::query(
        "WITH picked AS (
             SELECT id, (row_number() OVER ()) - 1 AS n
             FROM (SELECT id FROM tags ORDER BY name LIMIT 8) t
         )
         INSERT INTO tag_usages (tag_id, day, account_id, uses)
         SELECT p.id, current_date - 400 - p.n::int, a.id, 1
         FROM accounts a CROSS JOIN picked p
         WHERE a.domain = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(PURGE_DOMAIN)
    .execute(pool)
    .await
    .expect("plant purge-target tag usage");
    sqlx::query(
        "WITH picked AS (
             SELECT id, (row_number() OVER ()) - 1 AS n
             FROM (SELECT id FROM preview_cards ORDER BY url LIMIT 3) c
         )
         INSERT INTO preview_card_usages (preview_card_id, day, account_id, uses)
         SELECT p.id, current_date - 400 - p.n::int, a.id, 1
         FROM accounts a CROSS JOIN picked p
         WHERE a.domain = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(PURGE_DOMAIN)
    .execute(pool)
    .await
    .expect("plant purge-target card usage");
    eprintln!("setup: planted {n} purge-bench accounts on {PURGE_DOMAIN}");
    uris
}

/// Plants the accounts, statuses and tags that make one search term match in
/// all three arms of `/api/v2/search`.
///
/// `api/search` had been measuring the statuses arm plus two arms that matched
/// nothing, because the dataset names accounts, statuses and tags from three
/// separate word lists and no term crosses them (E7). This is the smallest
/// thing that fixes it without redrawing the dataset, which would cost a
/// reseed and a recalibration of every budget in the suite.
///
/// `unlisted` and dated outside the 180-day window for the same reason the
/// delete and purge pools are: 900 public statuses stamped `now()` walk onto
/// the head of the federated timeline and quietly turn two of the suite's
/// medians into measurements of the harness (B6).
async fn plant_search_corpus(pool: &PgPool) {
    let account_ids: Vec<i64> = (0..SEARCH_ACCOUNTS)
        .map(|_| plamenu_db::id::next())
        .collect();
    let usernames: Vec<String> = (0..SEARCH_ACCOUNTS)
        .map(|i| format!("{SEARCH_TERM}{i}"))
        .collect();
    let uris: Vec<String> = usernames
        .iter()
        .map(|username| format!("https://{SEARCH_DOMAIN}/users/{username}"))
        .collect();
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, uri, display_name, public_key,
                               inbox_url, shared_inbox_url)
         SELECT t.id, t.username, $4, t.uri, 'Bench ' || t.username, '',
                t.uri || '/inbox', 'https://' || $4 || '/inbox'
         FROM unnest($1::bigint[], $2::text[], $3::text[]) AS t(id, uri, username)",
    )
    .bind(&account_ids)
    .bind(&uris)
    .bind(&usernames)
    .bind(SEARCH_DOMAIN)
    .execute(pool)
    .await
    .expect("plant the search-bench accounts");

    let status_ids: Vec<i64> = (0..SEARCH_STATUSES)
        .map(|_| plamenu_db::id::next())
        .collect();
    sqlx::query(
        "INSERT INTO statuses (id, uri, account_id, content, created_at, sort_at, visibility)
         SELECT t.id,
                'https://' || $3 || '/statuses/' || t.n,
                a.id,
                '<p>bench ' || $4 || ' corpus note ' || t.n || '</p>',
                now() - make_interval(days => 200 + (t.n % 170)::int),
                now() - make_interval(days => 200 + (t.n % 170)::int),
                'unlisted'
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)
         JOIN (SELECT id, row_number() OVER (ORDER BY id) - 1 AS ord
               FROM accounts WHERE domain = $3) a
              ON a.ord = (t.n - 1) % $2",
    )
    .bind(&status_ids)
    .bind(SEARCH_ACCOUNTS as i64)
    .bind(SEARCH_DOMAIN)
    .bind(SEARCH_TERM)
    .execute(pool)
    .await
    .expect("plant the search-bench statuses");

    // Tags carry no usage rows on purpose: the hashtag arm is a prefix scan of
    // `tags`, and usage inside the trending window would let the harness
    // decide what is trending (`seed::verify` fails the run for it).
    sqlx::query(
        // The uniqueness on `tags` is on `lower(name)`, not on `name`.
        "INSERT INTO tags (id, name)
         SELECT t.id, $2 || (t.n - 1)
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)
         ON CONFLICT (lower(name)) DO NOTHING",
    )
    .bind(
        (0..SEARCH_TAGS)
            .map(|_| plamenu_db::id::next())
            .collect::<Vec<i64>>(),
    )
    .bind(SEARCH_TERM)
    .execute(pool)
    .await
    .expect("plant the search-bench tags");
    eprintln!(
        "setup: planted the {SEARCH_TERM} search corpus \
         ({SEARCH_ACCOUNTS} accounts, {SEARCH_STATUSES} statuses, {SEARCH_TAGS} tags)"
    );
}

/// Ranks a set of statuses as trending, so `/explore` has a page to render.
///
/// Unlike the tag ranker, `trends::refresh_statuses` cannot be pointed at the
/// dataset's own past: a status's score decays with a *one-hour* half-life
/// from when it was posted, so nothing older than about a day can score above
/// the threshold however the reference instant is chosen. Ranking a slice
/// directly is the honest equivalent of "the ranker ran while these were hot"
/// — `status_trends` is a derived cache, and `reset_run_residue` already
/// clears it between runs.
async fn plant_status_trends(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO status_trends (status_id, account_id, score, language, allowed)
         SELECT s.id, s.account_id, 100.0 - row_number() OVER (ORDER BY f.uses DESC, s.id DESC),
                s.language, true
         FROM (SELECT status_id, count(*) AS uses FROM favourites
               GROUP BY status_id ORDER BY count(*) DESC LIMIT 60) f
         JOIN statuses s ON s.id = f.status_id
         JOIN accounts a ON a.id = s.account_id
         WHERE s.visibility = 'public' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
           AND a.suspended_at IS NULL AND a.silenced_at IS NULL
         ON CONFLICT (status_id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("plant the trending statuses");
    plamenu_db::status_trend::recalculate_ranks(pool)
        .await
        .expect("rank the planted trending statuses");
}

/// Plants a writer-owned root with a full page of the writer's own replies —
/// the shape `ap/status_replies` walks.
///
/// The AP replies collection inlines *local* replies as whole Notes, one
/// `account::find_by_id` and one `note_for_status` apiece, and the seeded deep
/// thread cannot exercise it: that thread is a chain, so its root has exactly
/// one direct child. Written straight to the database, `unlisted` and
/// backdated, for the reasons [`plant_delete_targets`] gives; the collection
/// serves `public` and `unlisted` alike.
///
/// Owned by `bench_writer` because `reset_run_residue` already deletes every
/// one of that account's statuses — a planted fixture the reset does not know
/// about is a dataset that drifts.
async fn plant_reply_collection(pool: &PgPool, writer: i64) -> (i64, i64) {
    let root = plamenu_db::id::next();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, content, created_at, sort_at, visibility, language)
         VALUES ($1, $2, '<p>bench replies root</p>',
                 now() - interval '200 days', now() - interval '200 days', 'unlisted', 'en')",
    )
    .bind(root)
    .bind(writer)
    .execute(pool)
    .await
    .expect("plant the replies-bench root");
    let ids: Vec<i64> = (0..AP_REPLIES).map(|_| plamenu_db::id::next()).collect();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, content, created_at, sort_at, visibility,
                               language, in_reply_to_id)
         SELECT t.id, $2, '<p>bench reply ' || t.n || '</p>',
                now() - make_interval(days => 199), now() - make_interval(days => 199),
                'unlisted', 'en', $3
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)",
    )
    .bind(&ids)
    .bind(writer)
    .bind(root)
    .execute(pool)
    .await
    .expect("plant the replies-bench replies");
    (root, ids.len() as i64)
}

/// Plants the writer-owned statuses `write/post_delete` consumes.
///
/// Consuming, like the inbound delete pool: past the end of it the endpoint
/// answers 404 without fanning anything out, which is far cheaper than the
/// real path and would read as an improvement.
async fn plant_post_delete_targets(pool: &PgPool, writer: i64) -> Vec<i64> {
    let ids: Vec<i64> = (0..POST_DELETE_TARGETS)
        .map(|_| plamenu_db::id::next())
        .collect();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, content, created_at, sort_at, visibility, language)
         SELECT t.id, $2, '<p>bench delete-me ' || t.n || '</p>',
                now() - make_interval(days => 200 + (t.n % 170)::int),
                now() - make_interval(days => 200 + (t.n % 170)::int),
                'unlisted', 'en'
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)",
    )
    .bind(&ids)
    .bind(writer)
    .execute(pool)
    .await
    .expect("plant the post-delete targets");
    ids
}

/// One writer-owned status for `write/post_edit` to rewrite. Not consuming: an
/// edit leaves the row in place, so every iteration does the same work — build
/// a snapshot, re-render, refresh the quote stamp, fan an `Update` out.
async fn plant_edit_target(pool: &PgPool, writer: i64) -> i64 {
    let id = plamenu_db::id::next();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, content, created_at, sort_at, visibility, language)
         VALUES ($1, $2, '<p>bench editable post</p>',
                 now() - interval '200 days', now() - interval '200 days', 'unlisted', 'en')",
    )
    .bind(id)
    .bind(writer)
    .execute(pool)
    .await
    .expect("plant the post-edit target");
    id
}

/// Plants the local account `db_jobs/statuses_cleanup_sweep` deletes for, its
/// auto-deletion policy, and the statuses the sweep consumes.
///
/// Its own account rather than a persona's, and that is the whole point: a
/// sweep benchmark deletes whatever its policy finds. Aimed at seeded rows it
/// would pass, and then fail the *next* run's drift check having already
/// destroyed the evidence. `reset_run_residue` deletes the account, which
/// takes the statuses, the policy and the tombstones with it.
///
/// No followers, so what is measured is the sweep — the policy page, the
/// eligibility query and the per-status delete cascade — rather than a
/// fan-out `write/post_delete` already prices.
async fn plant_cleanup_account(pool: &PgPool) -> i64 {
    let keys = plamenu_ap::keys::generate_keypair().expect("generate the cleanup account's key");
    let account = account::create_local(
        pool,
        plamenu_db::account::NewLocalAccount {
            username: CLEANUP_USERNAME,
            display_name: "Bench Cleanup",
            note: "bench cleanup persona",
            public_key_pem: &keys.public_pem,
        },
    )
    .await
    .expect("create the cleanup-bench account");
    // Ids minted a year back, not `id::next()`. The eligibility cutoff is a
    // *snowflake* derived from the wall clock (`compute_cutoff_id`), so a row
    // stamped `created_at = now() - 300 days` but carrying an id minted this
    // second is not old enough for any policy and the sweep would find
    // nothing to do — while still passing, because doing nothing is fast.
    let base_ms = i64::try_from(
        OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000
            - i128::from(400 * 86_400_000_i64),
    )
    .expect("a date 400 days ago fits in i64 milliseconds");
    let ids: Vec<i64> = (0..CLEANUP_SWEEP_STATUSES)
        .map(|n| plamenu_db::id::id_at(base_ms + n as i64))
        .collect();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, content, created_at, sort_at, visibility, language)
         SELECT t.id, $2, '<p>bench sweep-me ' || t.n || '</p>',
                now() - make_interval(days => 300 + (t.n % 60)::int),
                now() - make_interval(days => 300 + (t.n % 60)::int),
                'unlisted', 'en'
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)",
    )
    .bind(&ids)
    .bind(account.id)
    .execute(pool)
    .await
    .expect("plant the cleanup-sweep statuses");
    // Everything older than a week is eligible; the planted rows are a year
    // old, so what bounds each pass is the sweep's own budget, not the cutoff.
    sqlx::query(
        "INSERT INTO account_statuses_cleanup_policies (account_id, enabled, min_status_age)
         VALUES ($1, true, $2)
         ON CONFLICT (account_id) DO UPDATE SET enabled = true, min_status_age = $2",
    )
    .bind(account.id)
    .bind(7 * 24 * 3600_i32)
    .execute(pool)
    .await
    .expect("enable the cleanup-bench policy");
    account.id
}

async fn upsert_sender(pool: &PgPool, sender: &common::RemoteUser) -> i64 {
    let actor = &sender.actor;
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username: &actor.preferred_username,
            domain: "bench-peer.invalid",
            uri: &actor.id,
            display_name: "Bench Sender",
            note: "",
            inbox_url: &actor.inbox,
            shared_inbox_url: "https://bench-peer.invalid/inbox",
            public_key_pem: &sender.keys.public_pem,
            public_key_id: &actor.public_key.id,
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: true,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: true,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: Some("Person"),
        },
    )
    .await
    .unwrap()
    .id
}

async fn mint_token(pool: &PgPool, app_id: i64, account_id: i64) -> String {
    let user = user::find_by_account_id(pool, account_id)
        .await
        .unwrap()
        .expect("bench persona has a user row");
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app_id, Some(user.id), SCOPES)
        .await
        .unwrap();
    token
}

/// How a read case authenticates, and what it wants served.
enum Auth {
    Anon,
    Bearer(String),
    /// The web UI session cookie, which carries an ordinary OAuth token.
    Cookie(String),
    /// An `ActivityPub` GET (the actor route content-negotiates on Accept).
    ActivityPub,
}

/// One GET benchmark.
struct ReadCase {
    name: &'static str,
    uri: String,
    auth: Auth,
    /// The response body must be at least this many bytes.
    ///
    /// Asserting only on the status code lets the instrument lie in the one
    /// direction that flatters it: an empty timeline is a 200 with a 2-byte
    /// body, and a web page whose session stopped authenticating renders the
    /// anonymous landing page — also a 200. The endpoint breaks, the number
    /// improves, the gate goes green. Floors are set at roughly half the
    /// measured size: loose enough to survive dataset drift, tight enough that
    /// "returned nothing" cannot pass.
    min_len: usize,
    /// Substrings the body must contain, checked once in the pre-pass, for
    /// the cases where size alone cannot tell a working page from a plausible
    /// wrong one. More than one where a response has several parts that can
    /// each go empty independently — a search whose statuses arm fills and
    /// whose account arm does not is the same size and a different endpoint.
    must_contain: Vec<&'static str>,
    /// Overrides [`MEASUREMENT`] where the median alone would overrun it and
    /// criterion would spend the run warning about it.
    measurement: Option<Duration>,
}

impl ReadCase {
    fn new(name: &'static str, uri: impl Into<String>, auth: Auth, min_len: usize) -> Self {
        Self {
            name,
            uri: uri.into(),
            auth,
            min_len,
            must_contain: Vec::new(),
            measurement: None,
        }
    }

    fn containing(mut self, needle: &'static str) -> Self {
        self.must_contain.push(needle);
        self
    }

    fn measured_for(mut self, measurement: Duration) -> Self {
        self.measurement = Some(measurement);
        self
    }
}

fn build_request(case: &ReadCase) -> Request<Body> {
    let builder = Request::builder().uri(&case.uri);
    let builder = match &case.auth {
        Auth::Anon => builder,
        Auth::Bearer(token) => builder.header(header::AUTHORIZATION, format!("Bearer {token}")),
        Auth::Cookie(cookie) => builder.header(header::COOKIE, cookie),
        Auth::ActivityPub => builder.header(header::ACCEPT, "application/activity+json"),
    };
    builder.body(Body::empty()).unwrap()
}

/// Sends one request through a clone of the router, asserting success so a
/// regression to 4xx/5xx can never masquerade as a speedup, and asserting the
/// body cleared `min_len` so neither can a regression to nothing at all.
async fn send(router: &Router, request: Request<Body>, min_len: usize) -> usize {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        status.is_success(),
        "bench request failed: {status} {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        body.len() >= min_len,
        "bench response is {} bytes, under the {min_len} byte floor",
        body.len()
    );
    body.len()
}

/// Drives a list of read cases as one criterion group.
fn bench_reads(
    criterion: &mut Criterion,
    rt: &Runtime,
    fx: &Fixture,
    group: &str,
    cases: &[ReadCase],
) {
    let mut bench_group = criterion.benchmark_group(group);
    bench_group.sample_size(20);
    // Only touch the setter when a case actually overrides, so an explicit
    // `--measurement-time` on the command line still governs everything else.
    let mut overridden = false;
    for case in cases {
        match case.measurement {
            Some(measurement) => {
                bench_group.measurement_time(measurement);
                overridden = true;
            }
            None if overridden => {
                bench_group.measurement_time(MEASUREMENT);
                overridden = false;
            }
            None => {}
        }
        bench_group.bench_function(case.name, |b| {
            b.to_async(rt)
                .iter(|| send(&fx.router, build_request(case), case.min_len));
        });
    }
    bench_group.finish();
}

/// The endpoints behind every client screen. Shared with the pre-pass, so a
/// case can never be measured without also being verified.
fn api_cases(fx: &Fixture) -> Vec<ReadCase> {
    let sparse = || Auth::Bearer(fx.sparse_token.clone());
    let dense = || Auth::Bearer(fx.dense_token.clone());
    vec![
        ReadCase::new("home_sparse", "/api/v1/timelines/home", sparse(), 8_000),
        // The 1,500-follow persona is the suite's slowest read by an order of
        // magnitude; 20 samples of it alone overrun the default window.
        ReadCase::new("home_dense", "/api/v1/timelines/home", dense(), 8_000)
            .measured_for(Duration::from_secs(4)),
        ReadCase::new(
            "public_local",
            "/api/v1/timelines/public?local=true",
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "public_federated",
            "/api/v1/timelines/public",
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "public_federated_anon",
            "/api/v1/timelines/public",
            Auth::Anon,
            8_000,
        ),
        ReadCase::new(
            "tag_timeline",
            "/api/v1/timelines/tag/benchtag",
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "list_timeline",
            format!("/api/v1/timelines/list/{}", fx.list_id),
            dense(),
            8_000,
        ),
        ReadCase::new(
            "context_deep",
            format!("/api/v1/statuses/{}/context", fx.thread_root),
            sparse(),
            8_000,
        )
        .containing("\"descendants\""),
        ReadCase::new(
            "status_show",
            format!("/api/v1/statuses/{}", fx.single_status),
            sparse(),
            500,
        ),
        ReadCase::new("notifications", "/api/v1/notifications", sparse(), 8_000),
        ReadCase::new(
            "notifications_grouped",
            "/api/v2/notifications",
            dense(),
            8_000,
        )
        .containing("\"notification_groups\""),
        ReadCase::new("conversations", "/api/v1/conversations", sparse(), 4_000),
        ReadCase::new(
            "verify_credentials",
            "/api/v1/accounts/verify_credentials",
            dense(),
            500,
        )
        .containing("bench_dense"),
        ReadCase::new(
            "account_lookup",
            format!("/api/v1/accounts/lookup?acct={}", fx.lookup_acct),
            sparse(),
            300,
        ),
        ReadCase::new(
            "account_statuses",
            format!("/api/v1/accounts/{}/statuses", fx.prolific_account),
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "followers_page",
            format!("/api/v1/accounts/{}/followers", fx.popular_account),
            sparse(),
            8_000,
        ),
        ReadCase::new("favourites_page", "/api/v1/favourites", dense(), 8_000),
        ReadCase::new("bookmarks_page", "/api/v1/bookmarks", dense(), 8_000),
        // The group timelines: the boost-row feed and both ranked
        // sorts (Top over the unbounded window is its worst case).
        ReadCase::new(
            "group_new",
            format!("/api/v1/accounts/{}/statuses", fx.group_account),
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "group_top",
            format!(
                "/api/v1/accounts/{}/statuses?sort=top&t=all",
                fx.group_account
            ),
            sparse(),
            8_000,
        ),
        ReadCase::new(
            "group_hot",
            format!("/api/v1/accounts/{}/statuses?sort=hot", fx.group_account),
            sparse(),
            8_000,
        ),
        ReadCase::new("relationships", fx.rel_uri.clone(), sparse(), 1_000),
        // The whole three-arm response a client's search screen asks for. It
        // used to run `q=zephyrite`, which matches ~900 statuses and *nothing*
        // in the account or hashtag arms, so two thirds of the endpoint was
        // measured returning `[]` (E7). The term now matches in all three,
        // against a corpus the setup plants — see `plant_search_corpus`.
        ReadCase::new(
            "search",
            format!("/api/v2/search?q={SEARCH_TERM}&resolve=false"),
            sparse(),
            8_000,
        )
        // Size alone cannot tell a three-arm response from a one-arm one: the
        // statuses arm dominates the bytes either way, so each arm is asserted
        // non-empty by name.
        .containing("\"statuses\":[{")
        .containing("\"accounts\":[{")
        .containing("\"hashtags\":[{"),
        // The other half of status search: a term matching 42% of the corpus
        // rather than 0.19% of it. Cost is linear in *match count* — the
        // planner estimates the same ~1,500 rows for both — so this arm was
        // the one that mattered and the one nothing measured. It cost 9.0 s
        // before `SEARCH_CANDIDATES` bounded the rows the viewer-scoped
        // filters are applied to.
        ReadCase::new(
            "search_common",
            "/api/v2/search?q=meadow&type=statuses&resolve=false",
            sparse(),
            8_000,
        )
        .containing("\"statuses\""),
        ReadCase::new(
            "search_accounts",
            "/api/v2/search?q=zephyr&type=accounts&resolve=false",
            sparse(),
            2_000,
        ),
        ReadCase::new(
            "search_tags",
            "/api/v2/search?q=saffron&type=hashtags",
            sparse(),
            100,
        ),
        // The seed clears the trending threshold for one tag only (B7), so
        // this floor is deliberately low — it catches an empty array and
        // nothing more until the dataset grows a real trending set.
        ReadCase::new("trends_tags", "/api/v1/trends/tags", sparse(), 50),
        // Anonymous on purpose: this is the shape clients and crawlers poll,
        // and it was the one instance endpoint still running three full-table
        // aggregates per view. The pre-pass warms every case, so what is timed
        // here is the cached path — which is what production serves almost
        // always. The uncached path is priced in the CHANGELOG entry, not here;
        // a benchmark that recomputed it every iteration would be measuring a
        // cache miss rate no real instance has.
        ReadCase::new("instance_v1", "/api/v1/instance", Auth::Anon, 800)
            .containing("\"user_count\""),
    ]
}

fn bench_api(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    bench_reads(criterion, rt, fx, "api", &api_cases(fx));
}

fn bench_write(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    let mut group = criterion.benchmark_group("write");
    group.sample_size(10);
    group.measurement_time(MEASUREMENT);

    let posts = AtomicU64::new(0);
    let boosts = AtomicU64::new(0);
    let favourites = AtomicU64::new(0);
    let replies = AtomicU64::new(0);
    let post_edits = AtomicU64::new(0);
    let post_deletes = AtomicU64::new(0);

    let counter = &posts;
    group.bench_function("post_status", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/statuses")
                    .header(header::AUTHORIZATION, format!("Bearer {}", fx.writer_token))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "status": format!("bench write {n} meadow harbor signal"),
                            "visibility": "public",
                        }))
                        .unwrap(),
                    ))
                    .unwrap()
            },
            // The created status, serialized back.
            |request| send(&fx.router, request, 300),
            BatchSize::SmallInput,
        );
    });

    // The same post as a *reply*. 27% of the corpus is replies and none of the
    // write benches made one, so `resolve_thread_parent`, the reply-depth
    // clamp, the 0030 conversation trigger and the reply notification all
    // measured their empty branch. The delta against `post_status` is the
    // signal; a distinct parent per iteration keeps one thread from growing
    // under the benchmark.
    let counter = &replies;
    group.bench_function("post_reply", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                let parent = fx.reply_ids[n % fx.reply_ids.len()];
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/statuses")
                    .header(header::AUTHORIZATION, format!("Bearer {}", fx.writer_token))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "status": format!("bench reply {n} meadow harbor signal"),
                            "visibility": "public",
                            "in_reply_to_id": parent.to_string(),
                        }))
                        .unwrap(),
                    ))
                    .unwrap()
            },
            |request| send(&fx.router, request, 300),
            BatchSize::SmallInput,
        );
    });

    // Editing and deleting as the 5k-follower persona: the two write paths
    // that fan out exactly like `post_status` and had no budget at all.
    // The edit target is rewritten in place, so every iteration is the same
    // work; the delete pool is consumed one row per iteration.
    let counter = &post_edits;
    group.bench_function("post_edit", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/statuses/{}", fx.post_edit_id))
                    .header(header::AUTHORIZATION, format!("Bearer {}", fx.writer_token))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "status": format!("bench edit {n} meadow harbor signal"),
                        }))
                        .unwrap(),
                    ))
                    .unwrap()
            },
            |request| send(&fx.router, request, 300),
            BatchSize::SmallInput,
        );
    });

    let counter = &post_deletes;
    group.bench_function("post_delete", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                // Deliberately not `% len`: past the end the endpoint answers
                // 404 without fanning anything out, which is far cheaper than
                // the path this measures. `assert_write_invariants` fails the
                // run rather than let that read as an improvement.
                let id = fx.post_delete_ids.get(n).unwrap_or_else(|| {
                    panic!(
                        "post_delete ran past its pool of {} planted statuses — raise \
                         POST_DELETE_TARGETS",
                        fx.post_delete_ids.len()
                    )
                });
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/statuses/{id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", fx.writer_token))
                    .body(Body::empty())
                    .unwrap()
            },
            |request| send(&fx.router, request, 300),
            BatchSize::SmallInput,
        );
    });

    // API favourite/boost of a distinct remote status per iteration — the
    // notification + Like/Announce fan-out write path.
    for (name, targets, action, counter) in [
        ("post_favourite", &fx.fav_targets, "favourite", &favourites),
        ("post_boost", &fx.boost_targets, "reblog", &boosts),
    ] {
        group.bench_function(name, |b| {
            b.to_async(rt).iter_batched(
                || {
                    let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                    let id = targets[n % targets.len()];
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/v1/statuses/{id}/{action}"))
                        .header(header::AUTHORIZATION, format!("Bearer {}", fx.writer_token))
                        .body(Body::empty())
                        .unwrap()
                },
                |request| send(&fx.router, request, 300),
                BatchSize::SmallInput,
            );
        });
    }

    // The inbox answers a bare 202 with no body, so a byte floor cannot say
    // whether these did any work. Their equivalent is the row-count invariant
    // asserted in `assert_write_invariants` after the group.
    let creates = AtomicU64::new(0);
    let counter = &creates;
    group.bench_function("inbox_create_note", |b| {
        b.to_async(rt).iter_batched(
            // Building + signing the activity is setup — the measurement is
            // signature verification + ingest, the receiving side's cost.
            || signed_create_note(&fx.sender, counter.fetch_add(1, Ordering::Relaxed)),
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });

    // The same ingest as a reply to a local post, and as a note carrying
    // everything a real one does. `inbox_create_note` is the floor — a bare
    // body from a sender with no local followers — and the whole ingest
    // pipeline past that point was measuring its empty branch: no orphan
    // exists in the dataset, no note had a parent, a mention, a tag or an
    // attachment. What these two are worth is the *delta* against the
    // minimal case.
    let reply_creates = AtomicU64::new(0);
    let counter = &reply_creates;
    group.bench_function("inbox_create_reply", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let parent = &fx.reply_uris[(n as usize) % fx.reply_uris.len()];
                let uri = format!("{}/bench-reply-{n}", fx.sender.actor.id);
                let mut object = note_object(
                    &fx.sender,
                    &uri,
                    "<p>bench ingest reply: meadow harbor signal</p>",
                    None,
                )
                .with_published(backdated(n));
                object["inReplyTo"] = json!(parent);
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{uri}/activity"),
                    "type": "Create",
                    "actor": fx.sender.actor.id,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "object": object,
                });
                signed_inbox(&fx.sender, &activity)
            },
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });

    let rich_creates = AtomicU64::new(0);
    let counter = &rich_creates;
    group.bench_function("inbox_create_rich", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let uri = format!("{}/bench-rich-{n}", fx.sender.actor.id);
                let mut object = note_object(
                    &fx.sender,
                    &uri,
                    &rich_body(&fx.rich_mentions, &fx.rich_tags),
                    None,
                )
                .with_published(backdated(n));
                object["tag"] = json!(rich_tag_list(&fx.rich_mentions, &fx.rich_tags));
                object["attachment"] = json!([
                    {
                        "type": "Document",
                        "mediaType": "image/jpeg",
                        "url": format!("https://bench-peer.invalid/media/{n}-a.jpg"),
                        "name": "bench attachment a",
                    },
                    {
                        "type": "Document",
                        "mediaType": "image/jpeg",
                        "url": format!("https://bench-peer.invalid/media/{n}-b.jpg"),
                        "name": "bench attachment b",
                    },
                ]);
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{uri}/activity"),
                    "type": "Create",
                    "actor": fx.sender.actor.id,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "object": object,
                });
                signed_inbox(&fx.sender, &activity)
            },
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });

    // Signed Like / Announce of a local status — the two activity kinds that
    // dominate real inbound federation traffic.
    let likes = AtomicU64::new(0);
    let counter = &likes;
    group.bench_function("inbox_like", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{}/likes/{n}", fx.sender.actor.id),
                    "type": "Like",
                    "actor": fx.sender.actor.id,
                    "object": fx.like_uris[n % fx.like_uris.len()],
                });
                signed_inbox(&fx.sender, &activity)
            },
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });
    let announces = AtomicU64::new(0);
    let counter = &announces;
    group.bench_function("inbox_announce", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{}/announces/{n}", fx.sender.actor.id),
                    "type": "Announce",
                    "actor": fx.sender.actor.id,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "object": fx.announce_uris[n % fx.announce_uris.len()],
                });
                signed_inbox(&fx.sender, &activity)
            },
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });

    // Signed Update(Note) over the pre-ingested sender notes — the edit
    // ingest path (snapshot + re-render + quote-stamp refresh ordering).
    let edits = AtomicU64::new(0);
    let counter = &edits;
    group.bench_function("inbox_update_note", |b| {
        let next_update = || {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            let uri = &fx.update_uris[(n % UPDATE_NOTES) as usize];
            let updated = (OffsetDateTime::now_utc() + time::Duration::seconds(n as i64))
                .format(&Rfc3339)
                .unwrap();
            let content = format!("<p>bench edit {n} meadow harbor</p>");
            let activity = json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("{uri}#updates/{n}"),
                "type": "Update",
                "actor": fx.sender.actor.id,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "object": note_object(&fx.sender, uri, &content, Some(&updated)),
            });
            signed_inbox(&fx.sender, &activity)
        };
        // First edits also snapshot the original. Prime every target once so
        // timed samples consistently measure subsequent edits, rather than
        // mixing the two workloads until the counter has traversed the pool.
        // Keep these real requests in the counter and snapshot invariants.
        // This runs outside Bencher's timer, once and only when selected.
        if counter.load(Ordering::Relaxed) == 0 {
            rt.block_on(async {
                for _ in 0..UPDATE_NOTES {
                    send(&fx.router, next_update(), 0).await;
                }
            });
        }
        b.to_async(rt).iter_batched(
            next_update,
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });

    // Signed Delete(Note) over the planted targets — the inbound half of the
    // path C2 named. A remote note with no live replies is *hard*-deleted, so
    // this is where Postgres walks every table that references `statuses`; with
    // `conversations.root_status_id` unindexed that was an 18 MB sequential scan
    // per delete, paid whether or not any conversation row matched.
    let deletes = AtomicU64::new(0);
    let counter = &deletes;
    group.bench_function("inbox_delete_note", |b| {
        b.to_async(rt).iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
                // Deliberately not `% len`: see DELETE_NOTES. Running off the
                // end fails the run rather than silently measuring the
                // already-deleted fast path, which is ~10x cheaper and would
                // look like an improvement.
                let uri = fx.delete_uris.get(n).unwrap_or_else(|| {
                    panic!(
                        "inbox_delete_note ran past its pool of {} planted notes — raise \
                         DELETE_NOTES; past the end it would be timing deletes of notes \
                         that are already gone",
                        fx.delete_uris.len()
                    )
                });
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{uri}#delete"),
                    "type": "Delete",
                    "actor": fx.sender.actor.id,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "object": uri,
                });
                signed_inbox(&fx.sender, &activity)
            },
            |request| send(&fx.router, request, 0),
            BatchSize::SmallInput,
        );
    });
    group.finish();

    rt.block_on(assert_write_invariants(
        fx,
        WriteCounts {
            posts: posts.load(Ordering::Relaxed) as i64,
            boosts: boosts.load(Ordering::Relaxed) as i64,
            favourites: favourites.load(Ordering::Relaxed) as i64,
            replies: replies.load(Ordering::Relaxed) as i64,
            post_edits: post_edits.load(Ordering::Relaxed) as i64,
            post_deletes: post_deletes.load(Ordering::Relaxed) as i64,
            creates: creates.load(Ordering::Relaxed) as i64,
            reply_creates: reply_creates.load(Ordering::Relaxed) as i64,
            rich_creates: rich_creates.load(Ordering::Relaxed) as i64,
            likes: likes.load(Ordering::Relaxed) as i64,
            announces: announces.load(Ordering::Relaxed) as i64,
            updates: edits.load(Ordering::Relaxed) as i64,
            deletes: deletes.load(Ordering::Relaxed) as i64,
        },
    ));
}

/// How many times each write bench's setup ran, warm-up included — which is
/// exactly how many writes should have landed.
struct WriteCounts {
    posts: i64,
    boosts: i64,
    favourites: i64,
    replies: i64,
    post_edits: i64,
    post_deletes: i64,
    creates: i64,
    reply_creates: i64,
    rich_creates: i64,
    likes: i64,
    announces: i64,
    updates: i64,
    deletes: i64,
}

impl WriteCounts {
    /// Statuses the writer persona should own, on top of what the setup
    /// planted: an API delete leaves a *stub* (the row survives so the reply
    /// tree does), so deleting subtracts nothing here — what it consumes is
    /// the pool, which is checked separately.
    fn writer_writes(&self) -> i64 {
        self.posts + self.boosts + self.replies
    }

    /// Activities the writer fanned out — one delivery group each.
    fn writer_activities(&self) -> i64 {
        self.writer_writes() + self.favourites + self.post_edits + self.post_deletes
    }

    /// Statuses the ingest benches should have created for the remote peer.
    fn sender_writes(&self) -> i64 {
        self.creates + self.reply_creates + self.rich_creates + self.announces
    }

    fn total(&self) -> i64 {
        self.writer_activities() + self.sender_writes() + self.likes + self.updates + self.deletes
    }
}

/// The write half of "did this benchmark do any work".
///
/// A 202 with an empty body is indistinguishable from a 202 that silently
/// dropped the activity, and a write path that stopped fanning out would look
/// like a large speedup. The contention driver has always asserted this shape
/// for its own posts (exact fan-out width per activity, no lost or duplicated
/// jobs); this brings the same post-conditions to the benches that actually
/// carry budgets, so `./dev bench` no longer depends on the concurrency run to
/// notice that its writes stopped happening.
async fn assert_write_invariants(fx: &Fixture, n: WriteCounts) {
    // A filtered run that selected no write benchmark has nothing to check, and
    // saying so beats failing every `./dev bench -- api/...` spot-check on
    // "inbox_like (0) must have run".
    if n.total() == 0 {
        eprintln!("write invariants skipped: no write benchmark ran");
        return;
    }

    let writer_statuses: i64 =
        sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
            .bind(fx.writer_account)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    // A soft-delete stub exists only to keep somebody else's reply tree
    // standing; a status with no live reply — which is what the delete pool is,
    // and what most real deletions are — is removed outright, cascade and all.
    // So the pool shrinks by exactly the iteration count.
    assert_eq!(
        writer_statuses,
        fx.planted_writer_statuses + n.writer_writes() - n.post_deletes,
        "post_status ({}) + post_boost ({}) + post_reply ({}) - post_delete ({}) over \
         {} planted statuses left {writer_statuses} writer statuses",
        n.posts,
        n.boosts,
        n.replies,
        n.post_deletes,
        fx.planted_writer_statuses
    );
    // Editing rewrites one status over and over, so its effect is snapshots,
    // not rows. Stated as an effect so a filtered run compares 0 against 0.
    let writer_edits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM status_edits WHERE status_id = $1")
            .bind(fx.post_edit_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(
        writer_edits >= n.post_edits,
        "post_edit ran {} times but left only {writer_edits} edit snapshots",
        n.post_edits
    );
    // The durable trace a delete leaves: a tombstone, so the Note URI
    // answers `410 Gone` while the federated `Delete` propagates. Stated as an
    // effect rather than "did it run", so a filtered spot-check that selected
    // no delete compares 0 against 0.
    let writer_tombstones: i64 =
        sqlx::query_scalar("SELECT count(*) FROM status_tombstones WHERE account_id = $1")
            .bind(fx.writer_account)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(
        writer_tombstones >= n.post_deletes,
        "post_delete ran {} times but left only {writer_tombstones} tombstones",
        n.post_deletes
    );
    assert!(
        n.post_deletes < fx.post_delete_ids.len() as i64,
        "post_delete ran {} times against a pool of {} — raise POST_DELETE_TARGETS, \
         the tail of this run measured 404s instead of deletes",
        n.post_deletes,
        fx.post_delete_ids.len()
    );

    let writer_favourites: i64 =
        sqlx::query_scalar("SELECT count(*) FROM favourites WHERE account_id = $1")
            .bind(fx.writer_account)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(
        writer_favourites, n.favourites,
        "post_favourite ran {} times but left {writer_favourites} favourites",
        n.favourites
    );

    // Ingest side. `>=` rather than `==`: a re-announce of an already-announced
    // object is legitimately a no-op if a run ever wraps its target list. The
    // setup's two planted pools are counted explicitly — one is edited in place
    // and stays, the other is consumed one row per delete iteration, and a
    // delete bench that stopped deleting would otherwise read as a large
    // speedup with nothing to show for it.
    let sender_statuses: i64 =
        sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
            .bind(fx.sender_account)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    let planted = UPDATE_NOTES as i64 + fx.delete_uris.len() as i64;
    let expected_sender = planted + n.sender_writes() - n.deletes;
    assert!(
        sender_statuses >= expected_sender,
        "{} ingested notes + {planted} planted ones - inbox_delete_note ({}) should \
         have left at least {expected_sender} sender statuses, found {sender_statuses}",
        n.sender_writes(),
        n.deletes
    );
    // The two ingest benches that write no status of their own, asserted by
    // their effect rather than by "did it run at all". A signed Like leaves a
    // favourite owned by the sender; a signed Update leaves an edit snapshot.
    // Stated this way the check is self-guarding: a filtered spot-check that
    // selected neither compares 0 against 0 instead of failing on it, which is
    // how the previous formulation broke every `./dev bench -- api/...`.
    let sender_favourites: i64 =
        sqlx::query_scalar("SELECT count(*) FROM favourites WHERE account_id = $1")
            .bind(fx.sender_account)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(
        sender_favourites, n.likes,
        "inbox_like ran {} times but left {sender_favourites} favourites",
        n.likes
    );
    let sender_edits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM status_edits e
         JOIN statuses s ON s.id = e.status_id
         WHERE s.account_id = $1",
    )
    .bind(fx.sender_account)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(
        sender_edits,
        n.updates + n.updates.min(UPDATE_NOTES as i64),
        "inbox_update_note ran {} times; expected each new version and each original snapshot",
        n.updates
    );
    // The delete pool has to outlast the run: past its end the activity names a
    // note that is already gone, which is a much cheaper path and would drag the
    // median down while the benchmark still passed.
    assert!(
        n.deletes < fx.delete_uris.len() as i64,
        "inbox_delete_note ran {} times against a pool of {} — raise DELETE_NOTES, \
         the tail of this run measured deletes of notes that were already gone",
        n.deletes,
        fx.delete_uris.len()
    );

    // Fan-out. Every activity of a given kind must have delivered to the same
    // set of inboxes: a post and a boost address the writer's ~1.8k follower
    // inboxes, a favourite addresses the target author's one. Uniformity
    // *within* a kind is the invariant — a post that lost or duplicated part
    // of its fan-out shows up as a group of the wrong width — and it holds
    // without hard-coding a width that changes with the seed.
    let groups: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT activity->>'type', count(*), count(DISTINCT inbox_url)
         FROM delivery_jobs WHERE account_id = $1
         GROUP BY activity->>'type', activity->>'id'",
    )
    .bind(fx.writer_account)
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    assert_eq!(
        groups.len() as i64,
        n.writer_activities(),
        "{} write iterations produced {} delivery groups",
        n.writer_activities(),
        groups.len()
    );

    let mut by_kind: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for (kind, jobs, inboxes) in &groups {
        by_kind
            .entry(kind.as_str())
            .or_default()
            .push((*jobs, *inboxes));
    }
    let mut widths = Vec::new();
    for (kind, shapes) in &by_kind {
        let (width, inboxes) = shapes[0];
        assert!(
            width > 0 && inboxes == width,
            "{kind} delivered {width} jobs over {inboxes} distinct inboxes — \
             if the width is 0, reseed with PLAMENU_BENCH_RESEED=1"
        );
        let clean = shapes
            .iter()
            .filter(|shape| **shape == (width, inboxes))
            .count();
        assert_eq!(
            clean,
            shapes.len(),
            "{}/{} {kind} activities fanned out to something other than \
             {width} inboxes — the fan-out is losing or duplicating jobs",
            clean,
            shapes.len()
        );
        widths.push(format!("{}x{kind}@{width}", shapes.len()));
    }
    eprintln!(
        "write invariants OK: {writer_statuses} writer statuses, \
         {writer_favourites} favourites, {sender_statuses} sender statuses, \
         fan-out {}",
        widths.join(" ")
    );
}

fn note_object(
    sender: &common::RemoteUser,
    uri: &str,
    content: &str,
    updated: Option<&str>,
) -> serde_json::Value {
    let mut object = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": sender.actor.id,
        "content": content,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(updated) = updated {
        object["updated"] = json!(updated);
    }
    object
}

/// The body of the rich ingest bench's note: two local mentions and three
/// hashtags, marked up the way a remote server sends them.
fn rich_body(mentions: &[String], tags: &[String]) -> String {
    use std::fmt::Write as _;

    let mut body = String::from("<p>bench rich ingest");
    for username in mentions {
        let _ = write!(
            body,
            " <span class=\"h-card\"><a href=\"https://{}/@{username}\" class=\"u-url mention\">@<span>{username}</span></a></span>",
            common::TEST_DOMAIN
        );
    }
    for name in tags {
        let _ = write!(
            body,
            " <a href=\"https://bench-peer.invalid/tags/{name}\" rel=\"tag\">#<span>{name}</span></a>"
        );
    }
    body.push_str("</p>");
    body
}

/// The `tag` array that goes with [`rich_body`] — mention and hashtag objects,
/// which is what the ingest actually reconciles against.
fn rich_tag_list(mentions: &[String], tags: &[String]) -> Vec<serde_json::Value> {
    let mut list: Vec<serde_json::Value> = mentions
        .iter()
        .map(|username| {
            json!({
                "type": "Mention",
                "href": format!("https://{}/users/{username}", common::TEST_DOMAIN),
                "name": format!("@{username}@{}", common::TEST_DOMAIN),
            })
        })
        .collect();
    list.extend(tags.iter().map(|name| {
        json!({
            "type": "Hashtag",
            "href": format!("https://bench-peer.invalid/tags/{name}"),
            "name": format!("#{name}"),
        })
    }));
    list
}

/// An author date spread over the same 180-day window the dataset uses.
///
/// Without one, `ingest` falls back to `now()` and every note this harness
/// creates — the 256 pre-ingested edit targets, and every iteration of
/// `write/inbox_create_note` — becomes the newest public status in the
/// database. `api/public_federated` and `api/public_federated_anon` were
/// therefore paging `bench_sender`'s `<p>bench editable note</p>` with no
/// media, no tags and no cards: two of the suite's medians measured the
/// harness, not the product (B6).
fn backdated(n: u64) -> String {
    let at = *DATASET_DATE_ANCHOR.get().expect("fixture date initialized")
        - time::Duration::days(1 + (n % 170) as i64);
    at.format(&Rfc3339).unwrap()
}

/// Adds an author date to a note object.
trait Published {
    fn with_published(self, at: String) -> Self;
}

impl Published for serde_json::Value {
    fn with_published(mut self, at: String) -> Self {
        self["published"] = json!(at);
        self
    }
}

fn signed_inbox(sender: &common::RemoteUser, activity: &serde_json::Value) -> Request<Body> {
    let bytes = serde_json::to_vec(activity).unwrap();
    let signed =
        sender
            .signer()
            .sign_post(common::TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header(header::CONTENT_TYPE, "application/activity+json")
        .body(Body::from(bytes))
        .unwrap()
}

fn signed_create_note(sender: &common::RemoteUser, n: u64) -> Request<Body> {
    let actor = &sender.actor.id;
    let object_id = format!("{actor}/bench-note-{n}");
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{object_id}/activity"),
        "type": "Create",
        "actor": actor,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": object_id,
            "type": "Note",
            "attributedTo": actor,
            "content": "<p>bench ingest note: meadow harbor signal quiet ember</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": backdated(n),
        },
    });
    let bytes = serde_json::to_vec(&activity).unwrap();
    let signed =
        sender
            .signer()
            .sign_post(common::TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header(header::CONTENT_TYPE, "application/activity+json")
        .body(Body::from(bytes))
        .unwrap()
}

/// The server-rendered pages, as a logged-in user.
///
/// `must_contain` is load-bearing here rather than decorative: a session
/// cookie that stops authenticating does not 401, it renders the anonymous
/// landing page — same 200, comparable size, a fraction of the work.
fn web_cases(fx: &Fixture) -> Vec<ReadCase> {
    // The web UI session cookie carries an ordinary OAuth token.
    let cookie = || Auth::Cookie(format!("__Host-plamenu_session={}", fx.sparse_token));
    // The 400-member list belongs to the dense persona, so its page needs that
    // persona's session.
    let dense_cookie = || Auth::Cookie(format!("__Host-plamenu_session={}", fx.dense_token));
    vec![
        ReadCase::new("web_home", "/", cookie(), 20_000).containing("bench_sparse"),
        ReadCase::new("web_profile", "/@bench_popular", cookie(), 20_000)
            .containing("bench_popular"),
        ReadCase::new(
            "web_thread",
            format!("/@bench_local0/{}", fx.thread_root),
            cookie(),
            20_000,
        )
        .containing("bench_sparse"),
        // The same thread with no session — the crawler, the unfurler and
        // anybody following a link from elsewhere. `api/public_federated_anon`
        // was the only bench driving a `viewer = None` render, and it does not
        // go through the maud layer at all.
        ReadCase::new(
            "web_thread_anon",
            format!("/@bench_local0/{}", fx.thread_root),
            Auth::Anon,
            20_000,
        )
        .containing("thread root"),
        // 20 notification entities plus the advance-only marker write — the
        // one benched page that writes on a GET.
        ReadCase::new("web_notifications", "/notifications", cookie(), 20_000)
            .containing("notifications"),
        // The list timeline: the 35.7 ms list query with the reply and
        // boost-collapse passes on top of it.
        ReadCase::new(
            "web_list",
            format!("/lists/{}", fx.list_id),
            dense_cookie(),
            20_000,
        )
        .containing("bench_dense"),
        // `/explore` over the ranked trending set the setup plants.
        ReadCase::new("web_explore", "/explore", cookie(), 20_000).containing("bench_sparse"),
    ]
}

fn bench_web(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    bench_reads(criterion, rt, fx, "web", &web_cases(fx));
}

/// `ActivityPub` serving — what every remote peer hits when it derefs our
/// actors and collections.
///
/// These are **unsigned** GETs: the bench state sets `authorized_fetch =
/// false` (`tests/common/mod.rs`) while the shipped default is `true`
/// (`config.rs`), so this group measures a non-default posture and omits the
/// signature verification a real peer's request pays. The verification cost
/// itself is covered by the `write/inbox_*` benches, which are signed.
fn ap_cases(fx: &Fixture) -> Vec<ReadCase> {
    vec![
        ReadCase::new("actor", "/users/bench_popular", Auth::ActivityPub, 1_000)
            .containing("publicKey"),
        ReadCase::new(
            "followers_collection",
            "/users/bench_popular/followers?page=1",
            Auth::ActivityPub,
            400,
        ),
        ReadCase::new(
            "outbox_page",
            "/users/bench_popular/outbox?page=true",
            Auth::ActivityPub,
            3_000,
        ),
        // The single Note document, the most-dereferenced object on the
        // network after the actor. Pointed at a status carrying media and
        // tags: the builder assembles attachments and hashtags through their
        // own queries, and a bare body measures neither.
        ReadCase::new(
            "status_note",
            fx.rich_note_path.clone(),
            Auth::ActivityPub,
            800,
        )
        .containing("\"attachment\""),
        // The replies collection, which inlines a full page of local replies
        // as whole Notes — one account lookup and one Note build apiece. This
        // is the per-item fan-out on the highest-volume inbound deref
        // after the actor, on the fixture the setup plants for it.
        ReadCase::new(
            "status_replies",
            format!("{}?page=true", fx.replies_path),
            Auth::ActivityPub,
            8_000,
        )
        .containing("\"next\""),
    ]
}

fn bench_ap(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    bench_reads(criterion, rt, fx, "ap", &ap_cases(fx));
}

/// Direct DB/worker benches. The `api/account_statuses` bench above drives
/// the profile listing end-to-end, but only under the sparse persona's
/// default (`published`, `sort_at` keyset) ordering. These call
/// `status::by_account` directly against the prolific persona (a
/// 1,301-status remote author) under both orderings, one page deep, so the
/// two duplicated queries' plans are tracked independently: `published`
/// walks `idx_statuses_account_sort_at`, `received` walks
/// `idx_statuses_account`. The trends refresh is the recurring background
/// job whose cost scales with `tag_usages` volume.
fn bench_db(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    let mut group = criterion.benchmark_group("db");
    let filter = status::AccountStatusesFilter::default();
    for (name, order) in [
        ("by_account_published", user::TimelineOrder::Published),
        ("by_account_received", user::TimelineOrder::Received),
    ] {
        group.bench_function(name, |b| {
            b.to_async(rt).iter(|| async {
                status::by_account(
                    &fx.pool,
                    fx.prolific_account,
                    Some(fx.sparse_account),
                    &filter,
                    order,
                    20,
                )
                .await
                .unwrap()
            });
        });
    }
    let alias_ids = fx
        .render_statuses
        .iter()
        .take(20)
        .map(|status| status.id)
        .collect::<Vec<_>>();
    assert_eq!(alias_ids.len(), 20, "alias benchmark requires a full page");
    rt.block_on(lemmy_id::aliases_for(
        &fx.pool,
        lemmy_id::Kind::Status,
        &alias_ids,
    ))
    .expect("pre-allocate Lemmy aliases outside the measurement");
    group.bench_function("lemmy_aliases_20_warm", |b| {
        b.to_async(rt).iter(|| async {
            let aliases = lemmy_id::aliases_for(&fx.pool, lemmy_id::Kind::Status, &alias_ids)
                .await
                .unwrap();
            assert_eq!(aliases.len(), alias_ids.len());
        });
    });
    rt.block_on(post_read::set_many(
        &fx.pool,
        fx.sparse_account,
        &alias_ids,
        true,
    ))
    .expect("pre-mark benchmark posts outside the measurement");
    group.bench_function("post_reads_20_warm", |b| {
        b.to_async(rt).iter(|| async {
            let read = post_read::read_ids(&fx.pool, fx.sparse_account, &alias_ids)
                .await
                .unwrap();
            assert_eq!(read.len(), alias_ids.len());
        });
    });
    let sparse_user = rt
        .block_on(user::find_by_account_id(&fx.pool, fx.sparse_account))
        .expect("load benchmark user")
        .expect("sparse benchmark account has a user");
    group.bench_function("lemmy_unread_counts", |b| {
        b.to_async(rt).iter(|| async {
            notification::lemmy_unread_counts(&fx.pool, sparse_user.id, fx.sparse_account)
                .await
                .unwrap()
        });
    });
    let notification_ids = rt
        .block_on(
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM notifications WHERE account_id = $1 ORDER BY id DESC LIMIT 20",
            )
            .bind(fx.sparse_account)
            .fetch_all(&fx.pool),
        )
        .expect("load Lemmy inbox benchmark page");
    assert_eq!(
        notification_ids.len(),
        20,
        "inbox benchmark requires a full page"
    );
    group.bench_function("lemmy_inbox_read_states_20", |b| {
        b.to_async(rt).iter(|| async {
            let states = lemmy_inbox::read_states(
                &fx.pool,
                sparse_user.id,
                fx.sparse_account,
                &notification_ids,
            )
            .await
            .unwrap();
            assert_eq!(states.len(), notification_ids.len());
        });
    });
    let media_rows = rt
        .block_on(
            sqlx::query_as::<_, (i64, i64, String)>(
                "SELECT id, account_id, file_name
             FROM media_attachments
             WHERE file_name IS NOT NULL
             ORDER BY id LIMIT 20",
            )
            .fetch_all(&fx.pool),
        )
        .expect("load Lemmy media benchmark page");
    assert_eq!(media_rows.len(), 20, "media benchmark requires a full page");
    for (id, account_id, alias) in &media_rows {
        rt.block_on(async {
            sqlx::query(
                "INSERT INTO lemmy_media_uploads (media_id, account_id, alias, delete_token)
                 VALUES ($1, $2, $3, $4) ON CONFLICT (media_id) DO NOTHING",
            )
            .bind(id)
            .bind(account_id)
            .bind(alias)
            .bind(format!("bench-token-{id}"))
            .execute(&fx.pool)
            .await
            .unwrap();
        });
    }
    group.bench_function("lemmy_media_list_20", |b| {
        b.to_async(rt).iter(|| async {
            let rows = lemmy_media::list(&fx.pool, None, 20, 0).await.unwrap();
            assert_eq!(rows.len(), 20);
        });
    });
    group.finish();

    // The hourly trends refresh, against the dataset's own reference day rather
    // than the wall clock. Scoped to "today" it ranked ~15k candidates on the
    // day the dataset was built and one candidate on every day after, so it was
    // gated behind `PLAMENU_BENCH_RESEED=1` and marked known-bad; anchored to
    // `max(tag_usages.day)` it scores the same population whenever it runs, and
    // it is the suite's only coverage of a background job.
    let mut group = criterion.benchmark_group("db_jobs");
    group.sample_size(10);
    group.bench_function("trends_refresh_tags", |b| {
        b.to_async(rt).iter(|| async {
            plamenu::trends::refresh_tags_at(&fx.state, fx.trends_at)
                .await
                .unwrap();
        });
    });

    // Purging one remote account — the fediverse's mass-deletion storm, priced
    // one account at a time. `account::delete_by_uri` is a single
    // `DELETE FROM accounts`, so everything after it is Postgres walking the
    // tables that reference it; before the indexes in migration 0034 that was a
    // sequential scan of `tag_usages` (695k rows / 39 MB) and of
    // `conversations` (340k / 18 MB) per account, inside the deleting
    // transaction. Give every pool connection time to warm its cascade plans;
    // half a second left a cold-to-warm transition inside measured samples.
    let purges = AtomicU64::new(0);
    let counter = &purges;
    group.warm_up_time(Duration::from_secs(3));
    group.bench_function("purge_account", |b| {
        b.to_async(rt).iter(|| async {
            let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
            let uri = fx.purge_uris.get(n).unwrap_or_else(|| {
                panic!(
                    "purge_account ran past its pool of {} accounts — raise PURGE_ACCOUNTS; \
                     past the end it would be timing a lookup that matches nothing",
                    fx.purge_uris.len()
                )
            });
            assert!(
                account::delete_by_uri(&fx.pool, uri).await.unwrap(),
                "purge_account found nothing to purge at {uri}"
            );
        });
    });

    group.warm_up_time(WARM_UP);

    // The auto-deletion sweep. `statuses_cleanup::run_once` refuses to run
    // while more than 500 deliveries are already due — its analogue of
    // Mastodon's queue-latency `under_load?` guard — and `bench_write` leaves
    // roughly half a million enqueued jobs behind, which is not a state any
    // instance is ever in. So the queue is emptied first, and the drain
    // benchmarks below plant their own. `assert_write_invariants` has already
    // read those jobs by this point; that is why this is safe here and would
    // not be one group earlier. TRUNCATE also removes the index/dead-tuple
    // residue: DELETE made the empty-queue check depend on autovacuum timing.
    rt.block_on(async {
        sqlx::raw_sql("TRUNCATE delivery_jobs")
            .execute(&fx.pool)
            .await
            .expect("clear the delivery backlog before the sweep");
    });
    let sweeps = AtomicU64::new(0);
    let counter = &sweeps;
    group.bench_function("statuses_cleanup_sweep", |b| {
        b.to_async(rt).iter(|| async {
            counter.fetch_add(1, Ordering::Relaxed);
            let (deleted, _cursor) = plamenu::statuses_cleanup::run_once(&fx.state, 0).await;
            assert_eq!(
                deleted, 50,
                "the cleanup sweep must delete a full batch — its pool of \
                 {CLEANUP_SWEEP_STATUSES} planted statuses may have run out"
            );
        });
    });

    // The link-crawl drain. Post-C5 the queue carries statuses that might
    // carry a link, and the crawler still spends several round trips per job
    // deciding — which is the cost that scales with the queue, and the reason
    // this is an O(n) tripwire rather than a latency target.
    rt.block_on(plant_link_crawl_jobs(&fx.pool));
    let crawl_batches = AtomicU64::new(0);
    let counter = &crawl_batches;
    group.bench_function("link_crawl_drain", |b| {
        b.to_async(rt).iter(|| async {
            counter.fetch_add(1, Ordering::Relaxed);
            let claimed = plamenu::link_preview::run_due(&fx.state).await;
            assert_eq!(
                claimed, 20,
                "link-crawl must claim a full batch — its pool of \
                 {LINK_CRAWL_DRAIN_JOBS} planted jobs may have run out"
            );
        });
    });

    // The outbox drain — the one background job whose cost is multiplied by
    // every fan-out the write benches measure, and which no benchmark called.
    // Per job it does ~8 uncached round trips plus one RSA signature, and
    // `run_due` claims 20 and runs up to 8 inboxes concurrently. Delivery
    // itself is stubbed, so what is timed is our side of it.
    rt.block_on(plant_delivery_jobs(&fx.pool, fx.popular_account));
    let drains = AtomicU64::new(0);
    let counter = &drains;
    group.bench_function("delivery_drain", |b| {
        b.to_async(rt).iter(|| async {
            counter.fetch_add(1, Ordering::Relaxed);
            let claimed = plamenu::delivery::run_due(&fx.state).await;
            assert_eq!(
                claimed, 20,
                "delivery must claim a full batch — its pool of \
                 {DELIVERY_DRAIN_JOBS} planted jobs may have run out"
            );
        });
    });
    group.finish();

    // Verify completed work, not just successful claims. A failed completion
    // can otherwise leave leased jobs behind while the drain appears fast.
    rt.block_on(async {
        for (batches, capacity, query) in [
            (
                crawl_batches.load(Ordering::Relaxed),
                LINK_CRAWL_DRAIN_JOBS,
                "SELECT count(*) FROM link_crawl_jobs",
            ),
            (
                drains.load(Ordering::Relaxed),
                DELIVERY_DRAIN_JOBS,
                "SELECT count(*) FROM delivery_jobs WHERE inbox_url LIKE '%bench-deliver.invalid/inbox'",
            ),
        ] {
            if batches == 0 {
                continue;
            }
            let left: i64 = sqlx::query_scalar(query).fetch_one(&fx.pool).await.unwrap();
            assert_eq!(left, capacity as i64 - (batches * 20) as i64);
            eprintln!("drain fixture: {batches} full batches left {left} of {capacity} jobs");
        }
    });

    // What the consuming drains have left, so whoever next has to size one of
    // these pools has the number rather than a guess. Outside the measurement.
    let swept = sweeps.load(Ordering::Relaxed);
    if swept > 0 {
        let left: i64 = rt
            .block_on(
                sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
                    .bind(fx.cleanup_account)
                    .fetch_one(&fx.pool),
            )
            .unwrap();
        eprintln!(
            "db_jobs: {swept} cleanup sweeps left {left} of {CLEANUP_SWEEP_STATUSES} \
             planted statuses; {} link-crawl and {} delivery batches drained",
            crawl_batches.load(Ordering::Relaxed),
            drains.load(Ordering::Relaxed)
        );
    }
}

/// Pre-enqueues the link-crawl backlog `db_jobs/link_crawl_drain` consumes.
///
/// Planted here rather than in `setup` so a run that does not reach this
/// benchmark never has a queue at all — `statuses_cleanup::run_once` skips
/// itself when deliveries are backed up, and an unrelated benchmark's fixture
/// must not decide whether another one does any work.
async fn plant_link_crawl_jobs(pool: &PgPool) {
    sqlx::raw_sql("DELETE FROM link_crawl_jobs")
        .execute(pool)
        .await
        .expect("clear the link-crawl backlog before planting it");
    let ids: Vec<i64> = (0..LINK_CRAWL_DRAIN_JOBS)
        .map(|_| plamenu_db::id::next())
        .collect();
    let inserted = sqlx::query(
        "WITH eligible AS (
             SELECT array_agg(id ORDER BY id DESC) AS ids
             FROM (
                 SELECT id FROM statuses
                 WHERE deleted_at IS NULL AND reblog_of_id IS NULL
                 ORDER BY id DESC LIMIT $2
             ) AS statuses
         )
         INSERT INTO link_crawl_jobs (id, status_id)
         SELECT t.id,
                eligible.ids[(1 + ((t.n - 1) % cardinality(eligible.ids)))::int]
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)
         CROSS JOIN eligible
         WHERE cardinality(eligible.ids) > 0",
    )
    .bind(&ids)
    .bind(LINK_CRAWL_TARGETS)
    .execute(pool)
    .await
    .expect("plant the link-crawl drain backlog");
    assert_eq!(
        inserted.rows_affected(),
        LINK_CRAWL_DRAIN_JOBS as u64,
        "link-crawl benchmark fixture did not plant its declared backlog"
    );
}

/// Pre-enqueues the outbox backlog `db_jobs/delivery_drain` consumes.
///
/// Signed by the popular persona rather than the writer: `bench_write`'s
/// post-conditions count the writer's delivery groups, and a benchmark's
/// fixture must not appear in another benchmark's arithmetic.
///
/// Spread over [`DRAIN_HOSTS`] inboxes because `run_due` groups a batch by
/// inbox and runs up to eight groups concurrently — all of it on one host
/// would measure the serial path and call it the drain.
async fn plant_delivery_jobs(pool: &PgPool, signer: i64) {
    let ids: Vec<i64> = (0..DELIVERY_DRAIN_JOBS)
        .map(|_| plamenu_db::id::next())
        .collect();
    sqlx::query(
        "INSERT INTO delivery_jobs (id, account_id, inbox_url, activity, run_at)
         SELECT t.id,
                $2,
                'https://drain' || (t.n % $3) || $4 || '/inbox',
                jsonb_build_object(
                    '@context', 'https://www.w3.org/ns/activitystreams',
                    'id', 'https://' || $5 || '/activities/bench-drain-' || t.n,
                    'type', 'Create',
                    'actor', 'https://' || $5 || '/users/bench_popular',
                    'to', jsonb_build_array('https://www.w3.org/ns/activitystreams#Public'),
                    'object', jsonb_build_object(
                        'type', 'Note',
                        'content', '<p>bench drain body</p>')),
                now() - interval '1 minute'
         FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, n)",
    )
    .bind(&ids)
    .bind(signer)
    .bind(DRAIN_HOSTS as i64)
    .bind(DRAIN_DOMAIN_SUFFIX)
    .bind(common::TEST_DOMAIN)
    .execute(pool)
    .await
    .expect("plant the delivery drain backlog");
}

/// Routing one status to the live `user` streams — the single-threaded
/// listener loop that renders one full status *per connected recipient*,
/// serially.
///
/// Budgeted as a ratio, not an absolute: what matters is the marginal cost of
/// one more connected client, and that number is the same on any machine while
/// the absolute is not. The recipients are real — the popular persona's local
/// followers, subscribed as they would be by a websocket — so the routing
/// query finds them the way it does in production.
fn bench_stream(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    assert!(
        fx.stream_viewers.len() >= STREAM_SUBSCRIBERS,
        "the streaming benchmark wants {STREAM_SUBSCRIBERS} local followers of \
         bench_popular and the dataset has {}",
        fx.stream_viewers.len()
    );
    let mut group = criterion.benchmark_group("stream");
    group.sample_size(10);

    group.measurement_time(Duration::from_secs(4));
    for (name, subscribers) in [
        ("route_status_1", 1),
        ("route_status_50", STREAM_SUBSCRIBERS),
    ] {
        // Fresh subscriptions per arm, and the receivers are drained by their
        // own tasks the way a websocket connection drains them. Without that
        // the bounded queues fill after 256 iterations and the hub drops every
        // subscriber as lagging — after which the benchmark measures routing
        // to nobody and reports it as a large improvement.
        let hub = &fx.state.streaming;
        let mut drains = Vec::with_capacity(subscribers);
        for (i, viewer) in fx.stream_viewers.iter().take(subscribers).enumerate() {
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<String>(64);
            hub.subscribe(
                i as u64,
                plamenu::streaming::Channel::User(*viewer),
                *viewer,
                true,
                sender,
                std::sync::Arc::new(tokio::sync::Notify::new()),
            );
            drains.push(rt.spawn(async move { while receiver.recv().await.is_some() {} }));
        }
        assert_eq!(
            hub.subscription_count(),
            subscribers,
            "{name}: the hub did not register every subscription"
        );

        group.bench_function(name, |b| {
            b.to_async(rt).iter(|| async {
                plamenu::streaming::handle_event(
                    &fx.state,
                    &plamenu_db::streaming::Event::StatusNew {
                        status_id: fx.stream_status,
                    },
                )
                .await
                .unwrap();
            });
        });

        // A subscriber the hub dropped mid-run means the drains fell behind
        // and part of this measurement routed to nobody.
        assert_eq!(
            hub.subscription_count(),
            subscribers,
            "{name}: {} of {subscribers} subscriptions survived the run — the rest \
             were dropped as lagging, so the tail measured routing to fewer clients",
            hub.subscription_count()
        );
        for (i, _) in fx.stream_viewers.iter().take(subscribers).enumerate() {
            hub.disconnect(i as u64);
        }
        for drain in drains {
            drain.abort();
        }
    }
    group.finish();
}

fn bench_micro(criterion: &mut Criterion, rt: &Runtime, fx: &Fixture) {
    let mut group = criterion.benchmark_group("micro");

    // RSA request signing — the delivery worker's per-job CPU cost.
    let signer = fx.sender.signer();
    let body = serde_json::to_vec(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Create",
        "actor": fx.sender.actor.id,
        "object": {"type": "Note", "content": "<p>bench delivery body</p>"},
    }))
    .unwrap();
    group.bench_function("sign_post", |b| {
        b.iter(|| signer.sign_post("mastodon.example", "/inbox", &body, SystemTime::now()));
    });

    // RFC 9421 signing is deliberately *not* a separate benchmark: it is
    // the same RSA-2048 operation over a different signature base, and it
    // measured 1.199 ms against sign_post's 1.201 ms — two runs of the same
    // number for 4.5 s of wall clock. `micro/sign_post`'s budget covers both;
    // it is tight enough (1.5 ms, not 2.5) to actually fire.

    // FEP-8b32 integrity-proof sign + verify — Ed25519 over two SHA-256
    // JCS hashes, added to every delivery when proof emission is on, and paid
    // again on the inbox side for proof-carrying inbound activities. Benched
    // as one round trip: split apart they were 0.034 and 0.035 ms, 0.4% of an
    // inbox ingest, sharing the document, the JCS pass and the hashing.
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://plamenu.test/users/bench/statuses/1/activity",
        "type": "Create",
        "actor": "https://plamenu.test/users/bench",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {"type": "Note", "content": "<p>bench delivery body</p>"},
    });
    group.bench_function("proof_roundtrip", |b| {
        b.iter(|| {
            let proven = plamenu_ap::proof::sign_document(
                &activity,
                &ed25519.private_multibase,
                "https://plamenu.test/users/bench#ed25519-key",
                "2026-07-11T00:00:00Z",
            )
            .unwrap();
            plamenu_ap::proof::PreparedProof::from_document(&proven)
                .unwrap()
                .verify(&ed25519.public_multibase)
                .unwrap();
        });
    });

    // The shared status renderer over a 20-status page — the baseline for the
    // roadmap's "typed entities / simd-json" question.
    group.bench_function("render_statuses_20", |b| {
        b.to_async(rt).iter(|| async {
            plamenu::entities::render_statuses(
                &fx.pool,
                common::TEST_DOMAIN,
                &fx.render_statuses,
                Some(fx.sparse_account),
            )
            .await
            .unwrap()
        });
    });
    group.finish();
}
