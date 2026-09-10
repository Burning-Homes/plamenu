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

For a focused investigation:

```sh
./dev bench -- api/home
./dev bench-check --filtered
```

Filtered grading allows missing measurements and an unmeasured ratio partner.
A measured benchmark still needs a budget. Filtered runs do not produce release
records. `--no-record` grades without writing a record; `--allow-stale` permits
comparison with a run from another revision, for investigation only.

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
