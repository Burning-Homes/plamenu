# Development

Install the pinned Rust toolchain, Docker Compose, PostgreSQL client tools,
FFmpeg/ffprobe, and cargo-nextest. E2E work also needs Python 3 and `uv`.
The development stack runs PostgreSQL 18 and Mailpit on loopback ports.

```sh
cp .env.example .env
./dev doctor
./dev up
```

`./dev up` generates an ignored configuration, creates a disposable local
account, and starts Plamenu in the background. Use `./dev run` for foreground
operation, and `./dev logs`, `./dev status`, and `./dev down` to manage it.
Set `PLAMENU_CONFIG` to use your own configuration.

Keep credentials, generated configuration, database dumps, media, and `.dev/`
out of version control. Media defaults to `.dev/media`; preserve that directory
when reusing a development database in another checkout.

## Choose a check

Run from the checkout; development commands load `.env` and export
`DATABASE_URL`. `./dev release` instead uses isolated configuration and disposable
services; see [Releasing](RELEASING.md).

| Command | Purpose |
| --- | --- |
| `./dev test` | Rust tests through cargo-nextest |
| `./dev check` | Inventory and federation-manifest checks, formatting, Clippy, offline Cargo checks, and the portable standalone gate |
| `./dev test-harness` | Python harness/budget tests, standalone E2E helpers and fault-proxy tests, and JavaScript unit tests |
| `./dev test-harness --profile full` | The same standalone suites plus the owned FUSE fault test; requires every case to run without skips |
| `./dev gate-inventory` | Recollect and compare the complete browser and live E2E inventories without executing tests |
| `./dev sqlx` | Regenerate `.sqlx` query metadata after SQL or migration changes |
| `./dev sqlx-check` | Check query metadata without updating it |
| `./dev docs-check` | Build the application, check CLI help and documentation examples, and build/check the book |
| `./dev a11y` | Run the full Chromium, Firefox, WebKit, mobile, and no-JavaScript accessibility regression matrix |
| `./dev a11y-matrix` | Validate complete WCAG 2.2 A/AA criterion-to-evidence coverage |
| `./dev ci` | Local check, docs-check, test, and dependency policy when cargo-deny is installed |

Prepare the standalone test dependencies once:

```sh
python3 -m venv .dev/gate-tools
.dev/gate-tools/bin/pip install -r scripts/requirements-tests.txt
npm --prefix accessibility ci --ignore-scripts
```

The standalone runner uses that interpreter when present; `PLAMENU_TEST_PYTHON`
can select another prepared interpreter. It attempts both Python and JavaScript
suites even after failures, ignores ambient test-selection options, and saves
logs, JUnit results and a scoped report under `target/gates/<run-id>/`.
The portable profile explicitly excludes the owned FUSE mount test and permits
the three scanner integration tests to skip when Gitleaks is absent. It never
reports a full standalone pass. The full profile also requires Gitleaks (on
`PATH` or through `GITLEAKS`), the FUSE 3 development library, `fusermount3`, and
permission to use `/dev/fuse`. A standalone full pass is not a full Rust,
browser, federation, performance, or release qualification.

Browser and E2E qualification use checked-in `accessibility/inventory.json` and
`e2e/inventory.json`. They bind the expected cases to their source files. After
changing those sources, prepare the locked E2E environment (`cd e2e && uv sync
--locked`), run `./dev gate-inventory --write` from the repository root, and review
the case additions, removals and capability changes. The command supplies no
selectors. A passing subset, an interrupted run, or an obsolete inventory cannot
qualify as a full pass. Automatic contribution jobs check inventory source hashes
and the federation pairing manifest without executing tests; test execution
remains manual.

`./dev test` prepares PostgreSQL's `template1` so SQLx tests can create isolated
schema copies. Use the disposable development database. Select a test suite
or filter when working on a specific feature:

```sh
./dev test -p plamenu --test web -E 'test(composer_)'
```

Commit generated `.sqlx` changes after running `./dev sqlx`; do not hand-edit
the metadata. For prose-only edits, check links and example syntax. See
[documentation guidance](contributing/index.md#documentation).

## Performance

For changes affecting database query counts or runtime cost, run the calibrated
benchmark and grade its results:

```sh
./dev bench
./dev bench-check --strict
```

Include the results with the change and commit the passing record in
`bench/results/`. Explain query-count changes, including decreases caused by
changed data. [Known query fan-outs](NPLUS1_INVENTORY.md) tracks remaining
per-item database work.

The benchmark uses the versioned `plamenu_bench` database and machine-specific
budgets. `./dev bench-concurrency` measures concurrent posting and delivery
fan-out. Calibration and coverage are described in `bench/README.md`.

## Regression and federation tests

Test the affected production entry point. Transport tests should exercise the
guarded HTTP client, including redirects. Mutations that enqueue federation
work need rollback coverage for both the data and the outgoing jobs. Deletion
tests should check the captured recipients. See [architecture](ARCHITECTURE.md).

For web changes, check the rendered controls and interactions in a browser;
server-response assertions cannot detect controls hidden by CSS or broken by
JavaScript. Run `./dev a11y-install` once for the pinned browser container, then
`./dev a11y`. The gate uses a digest-pinned browser container and an isolated
disposable database rather than the standing development data. The manual and
release protocol is documented in
[Accessibility release verification](ACCESSIBILITY_RELEASE.md).

Music renditions encode at normal speed in ordinary browser journeys. Cold
playback cases explicitly select `coldRendition: true` and require
`A11Y_COLD_RENDITION_FIXTURE=1`. Their owned fixture feeds real WAV audio through
a local FIFO, holds the final two seconds until the browser has proved playback
while encoding is unfinished, then releases the tail. The file-only FFmpeg
protocol restriction, codec checks, and all browser projects remain intact.
Missing release, encoder failure, or incomplete cleanup cannot qualify a pass.

Browser reset receipts include separate timings for database cloning, media
copying, process readiness and executable identity checks. The candidate is
hashed once and bound to its device, inode, size, nanosecond modification/change
times and mode. Every command and running process must still match that binding;
even equal-size writes with restored modification times are refused. Each test
continues to receive a fresh database, media tree and server process.

Rust integration fixtures live in the development-only `plamenu-test-support`
crate so their implementation compiles once rather than in every test binary.
Its actor helpers issue distinct RSA-2048 pairs from an immutable 64-pair
fixture reserve, then generate fresh pairs when the reserve is exhausted.
The shared allocator never recycles a pair within a process, even across local
and remote actors or repeated handles. Each caller owns its PEM strings;
Ed25519 generation, encrypted key provisioning, signing and verification remain
real. Tests that explicitly exercise key generation and rotation still call
the production generators. The reserve is public test material generated by
`plamenu_ap::keys::generate_keypair`, and is never linked into the shipping
server.
The music jobs, album-access and native-subscription fixtures similarly issue
independently salted PHC hashes for their known `fixture-password`, with fresh
production hashes after the reserve is exhausted. Real Argon2 verification
remains in use. The exhaustion regression compares every reserved hash's
parameters with a freshly generated production hash, so a change in the
production defaults requires updating the reserve.
Ordinary server tests reference `plamenu_db::MIGRATOR`, and database unit tests
reference `crate::MIGRATOR`, rather than embedding the same migrations in each
case. The migration source and checksum checks are unchanged. Local, release and
manual CI test databases seed `template1` once. Upgrade tests retain
`migrations = false`, explicitly clear
their isolated schema, and replay the required old migrations before upgrading.
The release and manual CI test runners execute serially without fail-fast.

The related `account_collections`, `account_feed`, `applications_settings` and
`local_identity` sources share the default `account_api` test binary to reduce
repeated compilation and linking. Likewise, `notification_delivery`,
`notification_filtering`, `notification_inbox`, `notification_retention` and
`notifications_v2` share `notifications_api`. The `admin_accounts`,
`admin_instance_policy`, `admin_metrics` and `admin_reports` sources share
`admin_api`. Registration, invitations, password reset, key storage, multi-account
sessions, OAuth scopes and token admission share `auth_api`. Their original files
and assertions stay in place. Blocking and muting, follows and follow settings,
lists, filters, markers, pins and bookmarks, and polls share `social_api` with
the same original-source and explicit-target compatibility.
The router API, client compatibility, content negotiation, CORS, forwarded
activities, instance metadata, rate limits, security headers and wire-fixture
sources share `http_api` under the same rules.
Search, account directory, tags and trends share `search_api` under the same
original-source and explicit-target rules.
Guarded discovery acquisition, communities, diversity, peer expansion, posts,
videos and wider search share `discovery_api` under those same rules. Their
original target names and every case remain selectable; the separate discovery
directory and web targets retain their existing wiring.
Grouped sources import the shared fixture crate directly, avoiding repeated
loading of the `common/mod.rs` shim; test bodies and assertions are unchanged.
`cargo nextest run -p plamenu --test NAME` selects one of these groups; explicit
`--test applications_settings`, `--test notification_inbox` and the other
original target names still work.
New test files remain automatically discoverable. A shared guard in `account_api`
checks that every target excluded from default execution appears in exactly
one default harness, under its original module name. Nextest continues to run
each case in its own process, with the configured serial execution. These groups
reduce test binary compilation and linking. The pinned Cargo toolchain also
honors `test = false` for compatibility targets under `--all-targets`, so Clippy
and online SQLx preparation check those sources through their groups once.
Explicit `--test NAME` still selects an original target without extra features.

For federation changes, check serialization and the relevant peer exchange.
The Python E2E suite is currently specific to one developer's machine and is
included for transparency and examples; it is not expected to work elsewhere
as provided. See [interoperability testing](INTEROPERABILITY.md) and
`e2e/README.md` for its setup limitations and existing workflow.
