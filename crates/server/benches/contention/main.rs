//! Concurrency / contention tripwire — three scenarios over the same
//! persistent dataset the `hot_paths` benches use.
//!
//! The `hot_paths` benches measure one request at a time, so they are
//! structurally blind to anything that only appears under concurrency: lock
//! contention, connection-pool starvation, the transactional-outbox fan-out
//! INSERT competing with itself, or a credential-crypto gate that queues every
//! sign-in behind half the cores. This driver is the complement.
//!
//!   * **fan-out** — `N = cores/2` concurrent `POST /api/v1/statuses` as the
//!     5k-follower `bench_writer` persona. Each post fans out to `F` distinct
//!     follower inboxes in one batched `delivery_jobs` INSERT, committed
//!     *inside* the status-creation transaction;
//!   * **mixed** — the same post workers with dense-persona
//!     `GET /api/v1/timelines/home` readers alongside them, so the read path's
//!     caches and the pool actually meet a write path. What is reported
//!     is what the *reads* cost while writing is going on;
//!   * **credentials** — concurrent `POST /login`. Argon2id is deliberately
//!     expensive and runs behind a `cores/2` semaphore
//!     ([`plamenu::crypto_gate`]), so a sign-in burst is the one load shape
//!     where queueing is by design and the question is how much.
//!
//! Every scenario reports the same three signals, each chosen to be repeatable
//! regardless of interleaving order:
//!
//!   * a **correctness invariant** (hard pass/fail) where the scenario has one
//!     — for fan-out: exactly `posts` statuses, exactly `posts × F` delivery
//!     jobs, and every individual post fanned out to exactly `F` distinct
//!     inboxes, so no fan-out is lost or duplicated under concurrency;
//!   * **zero request failures** — a pool-acquire timeout or a `40P01`
//!     deadlock surfaces as a non-2xx and fails the run;
//!   * the **contention ratio** `p50(N workers) / p50(1 worker)` — the
//!     machine-portable signal, checked against a budget derived from the
//!     worker count rather than a constant (see [`ratio_budget`]).
//!
//! Everything else mirrors `hot_paths`: in-process `oneshot`, the persistent
//! `plamenu_bench` database, outbound delivery stubbed by `StubFederation`
//! (jobs are enqueued, never delivered), and **fixed work** (not fixed time)
//! so runs stay comparable. Deliberately runs at cores/2, not all cores: with
//! spare cores, latency growth under load is *contention*, not CPU
//! saturation — which is the signal we want.
//!
//! Each run appends a record to `bench/results/`, next to the ones
//! `bench/bench_budgets.py` files for the hot-path suite: `/target` is
//! gitignored, and before that artifact existed the entire performance history
//! of this project was prose in commit messages.
//!
//! Run: `./dev bench-concurrency` (or `cargo bench -p plamenu --bench contention`).
//! Knobs: `PLAMENU_BENCH_CONCURRENCY`, `PLAMENU_BENCH_POSTS`,
//! `PLAMENU_BENCH_READS`, `PLAMENU_BENCH_LOGINS`, `PLAMENU_BENCH_SERIAL`,
//! `PLAMENU_BENCH_RATIO_BUDGET`.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    reason = "deterministic bench math over small in-range values"
)]

#[path = "../../tests/common/mod.rs"]
mod common;
#[path = "../hot_paths/seed.rs"]
#[allow(
    dead_code,
    reason = "shared seed module; these scenarios read Seed::pool, ::writer and ::dense"
)]
mod seed;

/// The same allocator the server declares (`main.rs`) — and the one this
/// driver's whole subject matter, contention under concurrency, is most
/// sensitive to.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_secret};
use plamenu_db::oauth::{self, NewApp};
use plamenu_db::{PgPool, user};
use serde_json::json;
use tower::ServiceExt;

const SCOPES: &str = "read write follow push";

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the custom benchmark harness omits test entry points"
)]
mod tests;

/// The password `seed::seed_personas` gives every persona.
const PERSONA_PASSWORD: &str = "bench-password";

/// How much of one worker's latency a *second* concurrent worker is allowed to
/// add, as a fraction of the serial p50.
///
/// The ratio budget used to be a flat 3.0×, which was two different mistakes at
/// once (E5): the driver sizes its worker pool at `cores/2`, so on a 4-core box
/// two workers could serialize completely and still pass, while on a 64-core
/// box thirty-two workers sharing one Postgres would trip it without anything
/// being wrong. A ratio budget has to scale with the load that produces it.
///
/// Perfect parallelism holds the ratio at 1.0 whatever the worker count;
/// complete serialization drives it to the worker count itself. This is the
/// slope between those, set so the budget reproduces the historical 3.0× at the
/// 8 workers it was calibrated on — the 16-core dev machine measured 1.54× in
/// 2026-07-22 and 1.59× against seed v4, so the budget still sits at roughly
/// twice the observed excess, which is the same margin every other budget in
/// the suite carries.
const SERIALIZATION_SLOPE: f64 = 0.28;

/// `p50(N) / p50(1)` ceiling at `workers` concurrency.
fn ratio_budget(workers: usize) -> f64 {
    1.0 + SERIALIZATION_SLOPE * (workers.max(1) - 1) as f64
}

/// Ceiling for one request. Two orders of magnitude above the calibrated
/// concurrent p50 (120 ms), so only a hang can reach it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    std::process::exit(rt.block_on(run()));
}

/// One scenario's outcome, in the shape the report and the results record both
/// want.
struct Report {
    name: &'static str,
    /// What one request of this scenario is.
    unit: &'static str,
    serial_p50: Duration,
    concurrent_p50: Duration,
    concurrent_p99: Duration,
    concurrent_max: Duration,
    requests: usize,
    wall: Duration,
    failures: Vec<StatusCode>,
    /// `None` where the scenario has no row-level invariant to check.
    invariant: Option<(bool, String)>,
}

impl Report {
    fn ratio(&self) -> f64 {
        if self.serial_p50.is_zero() {
            return 0.0;
        }
        self.concurrent_p50.as_secs_f64() / self.serial_p50.as_secs_f64()
    }

    fn over_budget(&self, budget: f64) -> bool {
        budget > 0.0 && self.ratio() > budget
    }

    fn failed(&self, budget: f64) -> bool {
        self.over_budget(budget)
            || !self.failures.is_empty()
            || self.invariant.as_ref().is_some_and(|(ok, _)| !ok)
    }

    fn print(&self, budget: f64) {
        println!("── {} ─────────────────────────────────────", self.name);
        println!(
            "{} {}  wall {:.2}s  throughput {:.0}/s",
            self.requests,
            self.unit,
            self.wall.as_secs_f64(),
            self.requests as f64 / self.wall.as_secs_f64()
        );
        println!("serial     p50 {}", ms(self.serial_p50));
        println!(
            "concurrent p50 {}   p99 {}   max {}",
            ms(self.concurrent_p50),
            ms(self.concurrent_p99),
            ms(self.concurrent_max)
        );
        if budget > 0.0 {
            let verdict = if self.over_budget(budget) {
                "OVER BUDGET"
            } else {
                "ok"
            };
            println!(
                "contention ratio {:.2}×   budget {budget:.2}×   {verdict}",
                self.ratio()
            );
        } else {
            println!(
                "contention ratio {:.2}×   (advisory — no budget calibrated)",
                self.ratio()
            );
        }
        if let Some((ok, description)) = &self.invariant {
            println!(
                "invariant: {description} — {}",
                if *ok { "OK" } else { "VIOLATED" }
            );
        }
        if !self.failures.is_empty() {
            let sample = &self.failures[..self.failures.len().min(5)];
            println!(
                "request failures: {} (e.g. {sample:?})",
                self.failures.len()
            );
        }
    }

    fn as_json(&self, budget: f64) -> serde_json::Value {
        json!({
            "serial_p50_ms": self.serial_p50.as_secs_f64() * 1000.0,
            "concurrent_p50_ms": self.concurrent_p50.as_secs_f64() * 1000.0,
            "concurrent_p99_ms": self.concurrent_p99.as_secs_f64() * 1000.0,
            "concurrent_max_ms": self.concurrent_max.as_secs_f64() * 1000.0,
            "requests": self.requests,
            "wall_secs": self.wall.as_secs_f64(),
            "ratio": self.ratio(),
            "ratio_budget": budget,
            "failures": self.failures.len(),
            "invariant_ok": self.invariant.as_ref().map(|(ok, _)| *ok),
        })
    }
}

/// One phase's measurements: every completed latency, plus the statuses of
/// whatever did not succeed.
#[derive(Default)]
struct Phase {
    latencies: Vec<Duration>,
    failures: Vec<StatusCode>,
}

impl Phase {
    fn record(&mut self, status: StatusCode, latency: Duration, ok: fn(StatusCode) -> bool) {
        if ok(status) {
            self.latencies.push(latency);
        } else {
            self.failures.push(status);
        }
    }

    fn merge(&mut self, other: Self) {
        self.latencies.extend(other.latencies);
        self.failures.extend(other.failures);
    }
}

fn two_xx(status: StatusCode) -> bool {
    status.is_success()
}

/// A successful web sign-in answers `303 See Other` to the post-login
/// redirect, not a 2xx.
fn signed_in(status: StatusCode) -> bool {
    status.is_success() || status == StatusCode::SEE_OTHER
}

async fn run() -> i32 {
    let workers = env_usize("PLAMENU_BENCH_CONCURRENCY", default_workers());
    let posts_per_worker = env_usize("PLAMENU_BENCH_POSTS", 16);
    let reads_per_worker = env_usize("PLAMENU_BENCH_READS", 16);
    let logins_per_worker = env_usize("PLAMENU_BENCH_LOGINS", 4);
    let serial_samples = env_usize("PLAMENU_BENCH_SERIAL", 24);
    let budget = std::env::var("PLAMENU_BENCH_RATIO_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| ratio_budget(workers));

    let seed = seed::open().await;
    let pool = seed.pool.clone();
    let writer = seed.writer;

    // The same full reset the hot-path driver runs, before anything is
    // measured: this scenario's own residue is only part of what accumulates in
    // a shared bench database, and a run that starts from a different dataset
    // than the last one is not comparable to it.
    seed::reset_run_residue(&pool, writer, seed.popular).await;
    seed::assert_dataset_unchanged(&pool).await;

    let federation = Arc::new(common::StubFederation::default());
    let state = common::test_state_with(pool.clone(), federation);
    let router = plamenu::build_router(state);

    let app = oauth::create_app(
        &pool,
        NewApp {
            name: "contention-bench",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: SCOPES,
        },
    )
    .await
    .expect("create bench oauth app");
    let token = mint_token(&pool, app.id, writer).await;
    let reader_token = mint_token(&pool, app.id, seed.dense).await;

    let counter = Arc::new(AtomicU64::new(0));

    // Warm the caches and derive the fan-out width F from one real post.
    let (status, _) = post_once(&router, &token, &counter).await;
    if !status.is_success() {
        eprintln!("warm-up post failed: {status}");
        return 1;
    }
    let fanout = writer_jobs(&pool, writer).await;
    cleanup(&pool, writer).await;
    if fanout == 0 {
        eprintln!(
            "fan-out width is 0 — the bench_writer persona has no follower inboxes; \
             reseed with PLAMENU_BENCH_RESEED=1"
        );
        return 2;
    }

    let mut reports = Vec::new();
    reports.push(
        fanout_scenario(
            &pool,
            &router,
            &token,
            &counter,
            writer,
            workers,
            posts_per_worker,
            serial_samples,
            fanout,
        )
        .await,
    );
    reports.push(
        mixed_scenario(
            &pool,
            &router,
            &token,
            &reader_token,
            &counter,
            writer,
            workers,
            posts_per_worker,
            reads_per_worker,
            serial_samples,
        )
        .await,
    );
    reports.push(credential_scenario(&router, workers, logins_per_worker, serial_samples).await);

    // Full reset on the way out, including this run's OAuth app and token: the
    // next run of *either* driver asserts the dataset is unchanged, and it is
    // this run's job to make that true.
    seed::reset_run_residue(&pool, writer, seed.popular).await;

    println!(
        "workers {workers}  fan-out width {fanout}  serial samples {serial_samples}  \
         ratio budget {budget:.2}× (derived from the worker count)"
    );
    for report in &reports {
        report.print(budget);
    }
    println!("────────────────────────────────────────────────────────────");
    match record(&reports, budget, workers, fanout) {
        Ok(Some(path)) => println!("recorded {path}"),
        Ok(None) => println!(
            "not recorded: the working tree was dirty during the run, so these numbers \
             do not describe the revision they would be filed under. Commit first, then \
             re-run — the record is the release baseline and has to name a revision it \
             is true of."
        ),
        Err(error) => eprintln!("could not record this run: {error}"),
    }

    i32::from(reports.iter().any(|report| report.failed(budget)))
}

/// The original scenario: concurrent posting by a 5k-follower persona, with the
/// fan-out shape asserted afterwards.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site, all of it context"
)]
async fn fanout_scenario(
    pool: &PgPool,
    router: &Router,
    token: &str,
    counter: &Arc<AtomicU64>,
    writer: i64,
    workers: usize,
    posts_per_worker: usize,
    serial_samples: usize,
    fanout: i64,
) -> Report {
    let mut serial = Phase::default();
    for _ in 0..serial_samples {
        let (status, latency) = post_once(router, token, counter).await;
        serial.record(status, latency, two_xx);
    }
    cleanup(pool, writer).await;

    let total_posts = workers * posts_per_worker;
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let router = router.clone();
        let token = token.to_owned();
        let counter = Arc::clone(counter);
        tasks.push(tokio::spawn(async move {
            let mut phase = Phase::default();
            for _ in 0..posts_per_worker {
                let (status, latency) = post_once(&router, &token, &counter).await;
                phase.record(status, latency, two_xx);
            }
            phase
        }));
    }
    let mut concurrent = Phase::default();
    for task in tasks {
        concurrent.merge(task.await.expect("worker task panicked"));
    }
    let wall = started.elapsed();

    // Correctness invariant over the concurrent phase's writes.
    let statuses = writer_statuses(pool, writer).await;
    let groups = writer_fanout_groups(pool, writer).await;
    let total_jobs: i64 = groups.iter().map(|g| g.jobs).sum();
    let clean_groups = groups
        .iter()
        .filter(|g| g.jobs == fanout && g.distinct_inboxes == fanout)
        .count();
    let invariant_ok = statuses == total_posts as i64
        && groups.len() == total_posts
        && clean_groups == total_posts
        && total_jobs == total_posts as i64 * fanout;
    let description = format!(
        "{statuses} statuses, {total_jobs} jobs, {clean_groups}/{total_posts} posts fanned \
         out to exactly {fanout} inboxes"
    );
    cleanup(pool, writer).await;

    finish(
        "fan-out contention",
        "posts",
        serial,
        concurrent,
        wall,
        Some((invariant_ok, description)),
    )
}

/// Reads under write load. What is timed is the *reads*: the dense persona's
/// home timeline is the suite's slowest query, and the question this scenario
/// answers is what it costs while the fan-out INSERTs are competing for the
/// same pool. The writers run alongside but are not measured here — the
/// fan-out scenario already prices them on their own.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site, all of it context"
)]
async fn mixed_scenario(
    pool: &PgPool,
    router: &Router,
    token: &str,
    reader_token: &str,
    counter: &Arc<AtomicU64>,
    writer: i64,
    workers: usize,
    posts_per_worker: usize,
    reads_per_worker: usize,
    serial_samples: usize,
) -> Report {
    // Serial baseline for the *read*, with nothing else running.
    let mut serial = Phase::default();
    for _ in 0..serial_samples {
        let (status, latency) = read_home_once(router, reader_token).await;
        serial.record(status, latency, two_xx);
    }

    let readers = workers.max(2) / 2;
    let writers = workers - readers;
    let started = Instant::now();
    let mut write_tasks = Vec::with_capacity(writers);
    for _ in 0..writers {
        let router = router.clone();
        let token = token.to_owned();
        let counter = Arc::clone(counter);
        write_tasks.push(tokio::spawn(async move {
            let mut failures = Vec::new();
            for _ in 0..posts_per_worker {
                let (status, _) = post_once(&router, &token, &counter).await;
                if !two_xx(status) {
                    failures.push(status);
                }
            }
            failures
        }));
    }
    let mut read_tasks = Vec::with_capacity(readers);
    for _ in 0..readers {
        let router = router.clone();
        let reader_token = reader_token.to_owned();
        read_tasks.push(tokio::spawn(async move {
            let mut phase = Phase::default();
            for _ in 0..reads_per_worker {
                let (status, latency) = read_home_once(&router, &reader_token).await;
                phase.record(status, latency, two_xx);
            }
            phase
        }));
    }
    let mut concurrent = Phase::default();
    for task in read_tasks {
        concurrent.merge(task.await.expect("reader task panicked"));
    }
    // Let the writers finish before the wall clock stops and the rows go: a
    // half-run fan-out left behind is the next run's drift.
    for task in write_tasks {
        // Writer errors affect the verdict, but their latency must not enter
        // the home-read distribution or its request count.
        concurrent
            .failures
            .extend(task.await.expect("writer task panicked"));
    }
    let wall = started.elapsed();
    cleanup(pool, writer).await;

    finish(
        "mixed read/write",
        "home reads",
        serial,
        concurrent,
        wall,
        None,
    )
}

/// Sign-in under burst. Argon2id costs tens of megabytes and CPU by design and
/// runs behind a `cores/2` semaphore, so this is the one path where queueing is
/// the intended behaviour — and the one that had no coverage at all, serial or
/// concurrent. A ratio near the worker count is *expected* here once the
/// gate saturates; what it must not do is grow faster than that, which is what
/// a lock outside the gate would look like.
async fn credential_scenario(
    router: &Router,
    workers: usize,
    logins_per_worker: usize,
    serial_samples: usize,
) -> Report {
    let mut serial = Phase::default();
    for _ in 0..serial_samples.min(8) {
        let (status, latency) = login_once(router).await;
        serial.record(status, latency, signed_in);
    }

    let started = Instant::now();
    let mut tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let router = router.clone();
        tasks.push(tokio::spawn(async move {
            let mut phase = Phase::default();
            for _ in 0..logins_per_worker {
                let (status, latency) = login_once(&router).await;
                phase.record(status, latency, signed_in);
            }
            phase
        }));
    }
    let mut concurrent = Phase::default();
    for task in tasks {
        concurrent.merge(task.await.expect("login task panicked"));
    }
    let wall = started.elapsed();

    finish(
        "credential burst",
        "sign-ins",
        serial,
        concurrent,
        wall,
        None,
    )
}

fn finish(
    name: &'static str,
    unit: &'static str,
    mut serial: Phase,
    mut concurrent: Phase,
    wall: Duration,
    invariant: Option<(bool, String)>,
) -> Report {
    serial.latencies.sort_unstable();
    concurrent.latencies.sort_unstable();
    let mut failures = serial.failures;
    failures.extend(concurrent.failures);
    Report {
        name,
        unit,
        serial_p50: percentile(&serial.latencies, 0.50),
        concurrent_p50: percentile(&concurrent.latencies, 0.50),
        concurrent_p99: percentile(&concurrent.latencies, 0.99),
        concurrent_max: concurrent.latencies.last().copied().unwrap_or_default(),
        requests: concurrent.latencies.len(),
        wall,
        failures,
        invariant,
    }
}

/// Files this run next to the hot-path suite's records. `Ok(None)` when the
/// tree was dirty, which is not an error — just not a record.
///
/// Separate from `bench/bench_budgets.py`'s file rather than merged into it:
/// the two drivers run independently (`./dev bench` no longer runs this one),
/// and a record that could only be written when both had run would mostly not
/// be written. Same directory, same naming, and the same refusal to file a
/// measurement under a revision it is not true of.
fn record(
    reports: &[Report],
    budget: f64,
    workers: usize,
    fanout: i64,
) -> Result<Option<String>, std::io::Error> {
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    if git(&["status", "--porcelain", "--untracked-files=no"])
        .is_none_or(|out| !out.trim().is_empty())
    {
        return Ok(None);
    }
    let now = time::OffsetDateTime::now_utc();
    let stamp = now
        .format(&time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_else(|_| "unknown".to_owned());
    let path = results_dir().join(format!(
        "contention-{}-{stamp}.json",
        &sha[..sha.len().min(12)]
    ));
    std::fs::create_dir_all(path.parent().expect("results path has a parent"))?;
    let payload = json!({
        "kind": "contention",
        "git_sha": sha,
        "git_dirty": false,
        "recorded_at": now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "seed_version": seed::SEED_VERSION,
        "workers": workers,
        "fanout_width": fanout,
        "ratio_budget": budget,
        "machine": machine(),
        "scenarios": reports
            .iter()
            .map(|report| (report.name.to_owned(), report.as_json(budget)))
            .collect::<serde_json::Map<_, _>>(),
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&payload).unwrap())?;
    Ok(Some(path.display().to_string()))
}

/// Enough about this box to know whether two records are comparable at all —
/// the same fields `bench_budgets.py` records for the hot-path suite.
fn machine() -> serde_json::Value {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_owned());
    json!({
        "cpu": model,
        "cores": std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        "platform": std::env::consts::OS,
    })
}

/// `bench/results`, resolved from the package the same way the hot-path bench
/// resolves the criterion directory: cargo runs a bench binary with the
/// *package* as its working directory, so a relative path lands in
/// `crates/server`.
fn results_dir() -> std::path::PathBuf {
    let mut dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("Cargo.lock").is_file() {
            return dir.join("bench").join("results");
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return std::path::PathBuf::from("bench/results"),
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
        .map(|out| out.trim().to_owned())
}

fn default_workers() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    (cores / 2).max(2)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn mint_token(pool: &PgPool, app_id: i64, account_id: i64) -> String {
    let user = user::find_by_account_id(pool, account_id)
        .await
        .expect("load bench persona user")
        .expect("bench persona has a user row");
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app_id, Some(user.id), SCOPES)
        .await
        .expect("mint token");
    token
}

/// Sends one request through a clone of the router; returns the status and the
/// full request→drained-response latency.
async fn send_once(router: &Router, request: Request<Body>) -> (StatusCode, Duration) {
    let started = Instant::now();
    // Bounded on purpose. This driver exists to catch serialization, and the
    // worst serialization bug is a self-deadlock — which this project has
    // shipped (the InflightGuard `then_some` deadlock, found only because
    // staging started 502ing). Unbounded, that failure mode hangs the run
    // forever and looks like a slow machine; bounded, it fails.
    let response = tokio::time::timeout(REQUEST_TIMEOUT, router.clone().oneshot(request))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "a single request did not complete within {REQUEST_TIMEOUT:?} — treat \
                 this as a deadlock, not a slow machine"
            )
        })
        .expect("router oneshot");
    let status = response.status();
    let _ = response.into_body().collect().await;
    (status, started.elapsed())
}

/// One `POST /api/v1/statuses`. The status text is unique per call so no
/// idempotency path short-circuits the fan-out.
async fn post_once(router: &Router, token: &str, counter: &AtomicU64) -> (StatusCode, Duration) {
    let n = counter.fetch_add(1, Ordering::Relaxed);
    let body = serde_json::to_vec(&json!({
        "status": format!("contention bench {n} meadow harbor signal"),
        "visibility": "public",
    }))
    .expect("serialize status");
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/statuses")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("build request");
    send_once(router, request).await
}

/// One `GET /api/v1/timelines/home` as the 1,501-follow persona.
async fn read_home_once(router: &Router, token: &str) -> (StatusCode, Duration) {
    let request = Request::builder()
        .uri("/api/v1/timelines/home")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build request");
    send_once(router, request).await
}

/// One `POST /login` with a correct password — the Argon2id verification path.
async fn login_once(router: &Router) -> (StatusCode, Duration) {
    let form = format!("identifier=bench_sparse&password={PERSONA_PASSWORD}");
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .expect("build request");
    send_once(router, request).await
}

/// Drops the writer's rows between the run's phases, so each phase measures the
/// same starting state. The run's *boundaries* use `seed::reset_run_residue`,
/// which is the complete list; this is the cheap subset that runs several times
/// inside the scenarios (jobs first — nothing FK-references them).
async fn cleanup(pool: &PgPool, writer: i64) {
    for sql in [
        "DELETE FROM delivery_jobs WHERE account_id = $1",
        "DELETE FROM notifications WHERE from_account_id = $1",
        "DELETE FROM favourites WHERE account_id = $1",
        "DELETE FROM statuses WHERE account_id = $1",
    ] {
        sqlx::query(sql)
            .bind(writer)
            .execute(pool)
            .await
            .expect("cleanup writer rows");
    }
}

async fn writer_jobs(pool: &PgPool, writer: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM delivery_jobs WHERE account_id = $1")
        .bind(writer)
        .fetch_one(pool)
        .await
        .expect("count delivery jobs")
}

async fn writer_statuses(pool: &PgPool, writer: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(writer)
        .fetch_one(pool)
        .await
        .expect("count statuses")
}

struct FanoutGroup {
    jobs: i64,
    distinct_inboxes: i64,
}

/// One row per post (grouped by the Create activity's id), carrying its job
/// count and its distinct-inbox count — so a post that lost or double-wrote
/// part of its fan-out shows up as a group with the wrong width.
async fn writer_fanout_groups(pool: &PgPool, writer: i64) -> Vec<FanoutGroup> {
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT activity->>'id' AS activity_id,
                count(*) AS jobs,
                count(DISTINCT inbox_url) AS distinct_inboxes
         FROM delivery_jobs
         WHERE account_id = $1
         GROUP BY 1",
    )
    .bind(writer)
    .fetch_all(pool)
    .await
    .expect("group fan-out by activity");
    rows.into_iter()
        .map(|(_id, jobs, distinct_inboxes)| FanoutGroup {
            jobs,
            distinct_inboxes,
        })
        .collect()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}
