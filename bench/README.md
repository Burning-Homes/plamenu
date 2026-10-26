# Benchmarks

Use these benchmarks to investigate request cost, database round trips, and
contention. Both drivers call the Axum router in-process against the persistent
`plamenu_bench` database on the development PostgreSQL server.

| Driver | Measures | Command |
| --- | --- | --- |
| `hot_paths` | Serial request latency, read query counts, and response sizes | `./dev bench` |
| `contention` | Concurrent posting, reads during writes, and sign-in bursts | `./dev bench-concurrency` |

## Run and grade

From the repository root, with the development environment configured:

```sh
./dev bench
./dev bench-check --strict
./dev bench-concurrency
```

Run the drivers sequentially, without competing builds, tests, or peer
provisioning. Initial compilation and database seeding add time before
measurement. The `dev` wrapper supplies `DATABASE_URL`; a bare `cargo bench`
needs it exported explicitly. Use only the development database server: the
harness creates, updates, and sometimes rebuilds its `plamenu_bench` database.
The wrappers pass `--no-default-features` to avoid Cargo building the unused
server executable alongside each benchmark. The default `cli` feature gates
only that executable; the benchmark uses the same complete server library.
Normal builds, integration tests and release packaging keep `cli` enabled.
For a bare invocation, use `cargo bench --locked --no-fail-fast
--no-default-features -p plamenu --bench hot_paths` (or `contention`).

For a focused investigation:

```sh
PLAMENU_BENCH_READ_CASE=api/home_sparse ./dev bench
./dev bench-check --filtered
```

This exact read-case selection drives both preparation and Criterion. Only the
selected case receives the metrics pre-pass and all 128 setup requests; the
shared dataset, fixture validation, body/content assertions, sample count and
warm-up/measurement windows remain unchanged. Unknown IDs fail. It cannot be
combined with Criterion arguments other than `--noplot` (and Cargo's `--bench`),
so a second filter or a changed measurement mode cannot disagree with the plan.
The manifest records the focused ID. Full grading and release evidence reject
it even if other result files on disk appear fresh.

The existing regex form `./dev bench -- api/home` remains available and retains
whole-inventory preparation; use the explicit selection above to avoid that cost.
Set `PLAMENU_BENCH_DIAGNOSTICS` to a private output directory to save the
untimed response bodies and print SQL events during the metrics pre-pass.
The `counted request:` marker separates each measured request from its warm-up.
Diagnostic output is disabled during Criterion timing; sample counts, response
assertions and measurement windows stay unchanged.
Custom Criterion arguments are recorded and require diagnostic `--filtered`
grading. Qualifying full runs use the default mode, with only `--noplot` allowed;
test/list modes, filters, and changed calibration cannot qualify using leftover
measurements. This also applies when saving or comparing diagnostic baselines.

Filtered grading allows missing measurements and an unmeasured ratio partner.
A measured benchmark still needs a budget. Filtered runs do not produce release
records. `--no-record` grades without writing a record; `--allow-stale` permits
comparison with a run from another revision, for investigation only.

Before validating arguments or preparing fixtures, the driver replaces the latest
manifest with a unique `running` attempt. Only normal completion publishes
`complete`; setup failures, interruptions and manifest write errors cannot reuse
an earlier pass. Manifests are retained under `plamenu-runs/` beside Criterion's
output. Standalone grading requires this boundary and measurements between its
start and completion, including for `--filtered` and `--allow-stale`. Older
standalone manifests need a new run; historical committed records retain their
separate revision, workload and calibration validation. `CRITERION_HOME` (before
`CARGO_TARGET_DIR`) selects the same directory for measurements and manifests;
pass that directory explicitly to `bench-check --criterion-dir` when overriding it.

## Dataset

The synthetic dataset contains about 61,000 remote accounts across 4,500
domains and 450,000 statuses, plus 300 local accounts. It includes replies,
groups, polls, events, articles, media, tags, moderation state, and interactions.
Its four main accounts exercise different workloads:

| Account | Workload |
| --- | --- |
| `bench_sparse` | Ten author follows and a followed hashtag |
| `bench_dense` | About 1,500 follows, lists, filters, blocks, mutes, and notifications |
| `bench_popular` | About 12,000 followers and 2,000 posts |
| `bench_writer` | Posting to 5,000 followers across shared inboxes |

These disposable accounts use `bench-password`. Query `SELECT key, value FROM
bench_seed` for fixture IDs; IDs change on reseeding.

The dataset is defined in
[`seed.rs`](../crates/server/benches/hot_paths/seed.rs). A run reuses it when
`SEED_VERSION` matches, cleans up previous benchmark writes, and checks its
shape and table counts. It refreshes planner statistics after migrations.
Tag vocabulary workers also reuse one physical connection, with each tag still
committed separately. Existing tags retain their IDs, casing and moderation
metadata. The worker releases its connection before ensuring the final benchmark
tag.

Each preview-card seed worker reuses one physical connection. Card upserts and
status attachments remain separate autocommit statements; cache invalidation,
provider policy and the usage rollup retain their existing behavior.
Run cleanup also removes retained discovery identities for the three disposable
`bench-peer.invalid`, `bench-purge.invalid` and `bench-search.invalid` origins;
seeded and unrelated retained metadata stays intact. A missing pristine row-count
baseline fails instead of disabling drift checks.
To rebuild it explicitly:

```sh
PLAMENU_BENCH_RESEED=1 ./dev bench
```

## Coverage and limits

| Group | Main cases |
| --- | --- |
| `api` | Timelines, threads, notifications, profiles, relationships, search, and trends |
| `write` | Posting, replies, edits, deletions, interactions, and signed inbox ingestion |
| `web` | Home, profile, thread, notifications, lists, and explore pages |
| `ap` | Actor, followers, outbox, Note, and replies documents |
| `stream` | Routing a status to one or fifty subscribers |
| `db`, `db_jobs` | Account posts, account purge, trends refresh, delivery, link crawling, and post cleanup |
| `micro` | Request signing, proof signing and verification, and rendering a status page |

Read cases have an untimed pass that checks response content and counts SQL
statements and response bytes. Mutation cases check their resulting data.
The benchmark definitions and [`budgets.toml`](budgets.toml) list individual cases.

Before each read case enters Criterion, the driver also sends eight setup
requests per configured pool connection (128 requests for the current limit of
16). Every response must succeed, be fully consumed, and satisfy the case's
body-size floor and required content. This fixed, bounded setup gives
connection-local prepared plans time to mature; it does not prove that every
connection chose a generic plan. It does not force PostgreSQL's plan mode or
change the measured request, dataset, pool settings, sample count, or existing
warm-up and measurement windows.

The run manifest keeps the setup request count, pool limit, first and last
setup request latency, and total setup time in `read_bootstrap`, separately from
Criterion measurements. These observations follow the existing metrics
pre-pass; they describe per-case setup, not a cold server's first request.
They are never used as qualifying medians. An explicit `PLAMENU_BENCH_READ_CASE`
run prepares only its selected read case; the ordinary full run and legacy
Criterion CLI filters retain preparation for every declared read case.

`write/inbox_update_note` measures subsequent edits: every target receives an
untimed first edit before sampling, with both original and updated snapshots
checked afterwards. Account purging warms up for three seconds before the
normal two-second measurement window. Other jobs use the default one-second
warm-up and two-second measurement window. The cleanup sweep starts with a
truncated delivery queue and must delete a full batch of 50 statuses per call.
Link-crawl and delivery drains require full 20-job batches and verify the
remaining queues after sampling. Their fixture capacities include warm-up
iterations; increasing queue capacity does not change the link-crawl target set.

The router runs without a listening socket, HTTP parsing, or TLS. The dev
PostgreSQL configuration disables fsync, outbound delivery is stubbed, and
rate limiting is disabled. ActivityPub GETs are unsigned with authorized fetch
disabled; signed inbox tests do not measure the GET authentication path.

The suite does not measure deployment throughput, network latency, cold start,
or production tail latency. Media serving, OAuth token exchange, concurrent
signed inbox bursts, and most background workers have no benchmark here.
Seeded translations, tombstones, and HLS segment rows do not establish coverage
of the routes that read them. Live-stream media is only sparsely represented.

Compare runs on the same machine and dataset. Inspect confidence intervals,
dispersion, and host load before attributing small timing changes to code.
Query counts can reveal extra database work even when latency stays within
budget. Use [`NPLUS1_INVENTORY.md`](../docs/NPLUS1_INVENTORY.md) to locate known
per-item queries; counts alone do not identify their cause.

## Hot-path budgets

`./dev bench-check` grades fresh Criterion output against `bench/budgets.toml`:

| Field | Check |
| --- | --- |
| `ms` | Median latency stays below the configured ceiling |
| `queries` | SQL statement count cannot increase; a decrease warns to recalibrate |
| `bytes` | Response size stays at or below 1.15 times the configured value |
| `ratio`, `ratio_of` | Median latency divided by a named partner's median stays below the ratio ceiling |
| Dispersion | Median absolute deviation divided by median stays at or below 15% |

Latency and ratio headroom below 25% produces a `TIGHT` warning. `--strict`
fails on warnings too, including reduced query counts and missing query or
body-size budgets for measured read cases. `known-bad` entries are shown
separately and excluded from ordinary grading; release validation rejects them.

The grader ignores measurements older than the run's start. Full grading
requires every budgeted case to have a fresh measurement and rejects a
revision mismatch unless `--allow-stale` is set.

## Contention scenarios

The driver uses half the available CPU parallelism, with at least two workers:

- **Fan-out:** concurrent posts, with exact post and delivery-job counts and
  distinct recipient counts checked afterwards.
- **Mixed read/write:** half the workers read the dense account's home timeline
  while the rest post. Latencies and request counts describe the reads; HTTP
  failures from both readers and writers fail the scenario.
- **Credential burst:** concurrent web sign-ins, including password hashing
  and its concurrency limit.

Each compares concurrent median latency with a serial baseline. The default
ratio ceiling is `1 + 0.28 × (workers − 1)`. Request failures and failed data
invariants also fail the run. Each router request has a 30-second response
timeout.

Investigation controls are `PLAMENU_BENCH_CONCURRENCY`, `PLAMENU_BENCH_POSTS`,
`PLAMENU_BENCH_READS`, `PLAMENU_BENCH_LOGINS`, `PLAMENU_BENCH_SERIAL`, and
`PLAMENU_BENCH_RATIO_BUDGET`. Optional release timing validation requires the
machine and workload in [`release-policy.toml`](release-policy.toml).

## Change a benchmark or budget

Add a budget with each new benchmark. Preserve the workload's meaning: fixture
writes must not displace seeded posts from measured pages or change trends.
Use private throwaway targets for destructive cases and fail when their pool
is exhausted. Add cleanup to `reset_run_residue` and a shape assertion where
needed.

Bump `SEED_VERSION` when changing the dataset. The CI diff check enforces this;
a change that cannot affect the dataset can explain that with
`SEED-SHAPE-UNCHANGED` on a changed line. Recalibrate after changing the machine
or dataset, and lower affected budgets after an optimization. Explain budget
increases in the change review: what extra work is required, or what was wrong
with the calibration. A failing run alone is not a reason to raise a ceiling.

## Notifications page calibration (27 October 2026)

`web/web_notifications` now renders 40 notifications; its original July 2026
calibration rendered 20. The old 153,452-byte baseline and 18.6 ms ceiling
therefore described a different page workload. These are regression baselines,
not independently established limits on acceptable user experience.

Two serial focused measurements of the current page returned 322,478 bytes
and 52 SQL statements, with medians of 18.435 and 18.291 ms. Both retained
40 notifications, 30 full post cards and all 264 forms. The new raw-body
baseline is 322,478 bytes, with the existing 15% allowance (370,850 bytes).
The latency ceiling follows the original calibration convention: twice the
larger measured median, rounded up to 0.1 ms, giving 36.9 ms. Strict grading
still requires 25% latency headroom, making its effective limit 27.675 ms.
The SQL ceiling remains 52.

Repeated control markup compresses well, but raw HTML size does not measure
compressed network transfer or browser rendering cost. Keep those distinctions
when reviewing future growth. This recalibration changes no workload, controls,
sample counts, timing windows or coverage requirements. The focused measurements
support this calibration only; they do not qualify a full gate or release.

## Native media account-purge calibration (11 October 2026)

The Songhive parity change adds required account/status erasure cascades for
native Music, Webmentions and notification delivery. A rollback-only diagnostic
on the calibration machine (Intel Core i7-11700F, x86-64, PostgreSQL 18) used the
existing purge footprint: five statuses, eight tag usages and three preview-card
usages. Ten `EXPLAIN (ANALYZE, BUFFERS, TIMING, FORMAT JSON)` deletes each rolled
back to the same savepoint. The final warmed sample measured 44 newly required
foreign-key triggers at 0.577 ms, within a 3.743 ms total delete. These cascades
check and remove private media/delivery state even for an account with no Music.

Only `db_jobs/purge_account` adds that measured fixed work to its existing
5.21 ms ceiling, yielding 5.79 ms. The workload, query ceilings and strict 25%
headroom requirement are unchanged. This is a calibration for required extra
integrity/privacy work; it does not increase other hot-path budgets. The simpler
fresh capture after owner locking is retained, with regression checks for managed
file cleanup, private metadata deletion, foreign metadata preservation and
idempotence. Final timing acceptance still requires the strict benchmark gate.

## Keep results

A passing full hot-path run is recorded under `bench/results/` unless recording
is disabled or the measured tree was dirty. The contention driver also writes
JSON records, including failed scenarios; inspect their outcome before using
them as evidence. Keep the checkout unchanged throughout measurement and
recording.

Records include the revision, machine, seed version, and measurements.
Criterion's detailed output under `target/criterion/` is temporary; its
`--save-baseline` and `--baseline` options support local comparisons.

Release timing checks are opt-in: `./dev release --with-performance` runs and
strictly grades both complete workloads on the calibration machine, retaining
the reports automatically. No manual report selection or commits are needed.
Other builders omit this machine-specific check; see the
[release procedure](../docs/RELEASING.md). `scripts/check-performance-evidence.py`
remains a separate tool for auditing historical committed records.
