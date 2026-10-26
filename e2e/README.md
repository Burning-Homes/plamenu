# Live federation tests

The Python E2E suite is currently tied to one developer's machine and an ad hoc
local setup. It is not portable or expected to be usable elsewhere as provided;
it is included for transparency and as examples. For release builds, the suite is
omitted by default (`./dev release --skip-e2e` makes that explicit); the
portable application tests and packaged-image restore drill still run.

This pytest project drives Plamenu and real peer servers over HTTPS. It covers
discovery, signatures, follow lifecycle, posts and edits, moderation,
interactions, polls, migration, media, groups, and software-specific wire
shapes, including Discourse category and topic-page interoperability through
the official ActivityPub plugin.

The tests expect the peer URLs and disposable credentials described in
`plamenu_e2e/config.py`. The repository contains the provisioning definitions,
Compose fixtures and exact upstream source revisions under `e2e/peers/`.
Generated checkouts, credentials, certificates and persistent state are
ignored rather than committed.

The existing machine-specific workflow uses these commands:

```sh
cp .env.example .env
./dev provision-peers
./dev up
./dev e2e-full
```

Provisioning clones the revisions in `e2e/peers/sources.lock`, applies the
small compatibility patches in `e2e/peers/patches/`, prepares the shared local
CA and hostnames, then starts and seeds the required matrix. Use
`./dev provision-peers --no-start` to prepare only the source checkouts, or
`./dev provision-peers --check` to verify an existing checkout without network
or container changes. `./dev up all` performs subsequent starts in dependency
order. `e2e-full` checks every required peer before pytest; ordinary `e2e`
permits software-specific tests to skip when an optional peer is absent.

The fixture fleet is intentionally heavyweight. A compatible separately
managed fleet can be selected with `PLAMENU_PEER_ROOT` and
`PLAMENU_PEER_DEV`; individual `*_FIXTURE_DIR` paths and peer URLs can also be
overridden in the ignored root `.env`.

When verifying an already-built candidate, set `PLAMENU_BINARY` to its absolute
executable path and `PLAMENU_CONFIG` to the standing test instance's config.
CLI fixtures and disposable peers then use that artifact without rebuilding.
`DATABASE_URL` must point to that same instance for database assertions. This
does not start or replace the server; run the same candidate there first.
Crash and offload drills freeze the running executable's inode and bind their
restarts, CLI commands and processors to that copy, so a later build replacing
the original pathname cannot change the program under test.

For a freshly recreated Plamenu identity, Lemmy also needs fresh peer state.
Its persistent instance and delivery records can otherwise suppress federation.
With the main instance and local proxy already running, use:

```sh
cd e2e
../scripts/run-isolated-lemmy.py \
  --state-dir ../target/lemmy-owned-NEW-RUN \
  --peer-root peers \
  --proxy-container mastodon-test-caddy-1 \
  --bootstrap-actor https://plamenu.local/users/developer \
  -- .venv/bin/python -m pytest tests/test_lemmy_federation.py --maxfail=0
```

This creates new Lemmy volumes, uses the pinned peer images, and temporarily
routes only `lemmy.local` to them. The bootstrap actor must be a real account on
the disposable main instance. Setup resolves it and waits for Lemmy's initial
federation checkpoint before test activities are sent; the upstream worker
otherwise starts at the latest activity and drops earlier queued messages.
Use the proxy exclusively during the run. Cleanup restores its original
configuration, exports container logs, and removes owned containers and the
private network while retaining named data volumes and diagnostic state. It does
not rewrite standing peer keys, queues or timestamps. This focused command
cannot qualify as a full pass. The supplied peer enables Lemmy's fast-federation
mode; community fixtures allow two seconds after Accept for its separate
one-second follower-cache refresh before creating posts. This is setup time,
not a shorter delivery timeout or a retry of lost posts.

Within pytest, ordinary disposable Plamenu peers clone one session-owned schema
prepared by `cargo sqlx migrate run` (requires `cargo-sqlx`). It starts from
`template0`, contains only migration data and is sealed against connections.
The template is bound to the candidate's file identity and digest, database and
exact migration checksums; changes or preparation failures fail fixture setup.
Each clone still runs the real candidate's migration-ledger checks and generates
fresh actor keys with its own encryption secret, media directory and process.
Backup/restore targets created independently remain blank `template0` databases.
The session removes its schema template at teardown; no standing `template1` or
application database is modified.

Add `--progress-events PATH` to retain live execution evidence in a fresh JSON
Lines file. Each event is flushed before execution continues. Collection records
the selected inventory; test start records the current nodeid, and test finish
records the result and duration after teardown. Every event includes completed
and total counts plus pass, fail, skip, xfail and xpass counters. The session
record includes source identity and the selected candidate's digest when
`PLAMENU_BINARY` is set. A final event distinguishes a completed selected
inventory from an interrupted run; use the release-matrix report to enforce full
coverage. Monitoring does not change assertions, timeouts, selection or retries.
An existing journal is never overwritten, and a runtime logging failure leaves
test execution running while making its monitoring evidence incomplete.

A fresh main database creates new instance signing keys. Reusing its old domain
against a persistent GoToSocial leaves that peer holding the previous keys;
authorized fetch then fails before federation assertions can run. Updating the
application executable alone preserves identity. Recreating the database must
also isolate the peer state; do not delete standing peer caches or relax signature
checks to make the suite pass. The same boundary applies when a disposable foreign
peer is reinstalled at an existing domain: its old remote actors must not coexist
with the replacement in the campaign-owned main database. Prepare that owned cache
before tests start, and retain the ownership and cleanup receipts. Reusing the same
peer identities across repeat runs avoids introducing another reinstall boundary.

For an already running disposable main instance, the explicit wrapper below
provides a fresh official GoToSocial with its own database, account credentials
and token cache. Select the existing local container names and fleet path:

```sh
cd e2e
../scripts/run-isolated-gotosocial.py \
  --state-dir ../target/gotosocial-owned-NEW-RUN \
  --peer-root peers \
  --proxy-container mastodon-test-caddy-1 \
  --standing-container gotosocial-test-gotosocial-1 \
  -- .venv/bin/python -m pytest --release-matrix --maxfail=0
```

The state directory must be new and the pinned image already cached. The wrapper
starts only owned containers, keeps strict server-side TLS and authorized fetch,
and temporarily changes only the local `gotosocial.local` proxy upstream. Run it
with exclusive ownership of that test proxy. It restores the original active
configuration on normal exit, test failure or interruption and stops only the
owned peer; it keeps containers, logs and data for diagnosis. The Caddyfile and
standing GoToSocial volume, credentials and process stay intact. Configuration
or ownership drift blocks restoration rather than overwriting someone else's
change. A forced `SIGKILL` cannot run cleanup; the original proxy JSON is retained
in the private state directory for recovery. This wrapper does not start or
replace Plamenu or operate a remote proxy.

The native-live matrix includes RTMP and HTTPS-FLV by default. To exercise an
optional `[live_rtmps]` listener, set `PLAMENU_RTMPS=1` when running
`tests/test_native_live.py`; the E2E cases then use real FFmpeg RTMPS publishing,
invalid-key rejection, PeerTube playback and reconnect. Set
`PLAMENU_RTMPS_CA` to the disposable certificate or local test CA to make
FFmpeg verify the server certificate; without it, the self-signed local
fixture uses `-tls_verify 0`. Production encoders must verify the host
certificate.

`tests/test_native_live_crash_drill.py` is deliberately excluded by default.
Back up the local database first. Only on an explicitly disposable standing
instance, set `PLAMENU_CRASH_DRILL=1`, `PLAMENU_SERVER_PID` to its exact PID,
and the matching absolute `PLAMENU_BINARY` and `PLAMENU_CONFIG`. The test
validates the process executable, working directory and command line before
SIGKILL, starts the same candidate after the crash, and leaves the replacement
serving. Do not run it against production or another user's active instance.

The outbound native-video/PeerTube test also drives PeerTube's actual watch UI
before deleting its temporary video. Install the pinned browser test dependencies
in `accessibility/` (`npm ci` and Playwright Chromium/Firefox browser binaries)
to run it. The check signs into the disposable PeerTube root test account,
plays both advertised HLS qualities, seeks, and confirms decoded frames on
desktop and 320px layouts. It writes a video-frame screenshot under
`target/e2e/`. HTTP/API import alone is not a substitute for this browser gate.

## The Plamenu peers

Two of the peers are this server, started and stopped by the test session
itself from the working tree (`plamenu_e2e/ephemeral.py`): `plamenu2.local`,
which `tests/test_plamenu2_*.py` federates against, and `doomed.local`, whose
whole life is one self-destruct. Each gets a throwaway database on the dev
Postgres, its own media directory and its own port (8422 and 8421).

`plamenu2` is fresh for each test. Default database names are unique; explicit
names must not already exist. Startup never drops a preexisting database.
Setup failures roll back resources already acquired, and teardown reaps the
owned server/encoder process group (allowing two seconds before forced termination),
closes database connections, removes the
created database and media, and retains logs under `target/e2e/logs/`.
Repeated teardown is safe. Backup/restore drills that create their own restore
database continue to own its cleanup.

Startup and crash recovery make bounded early readiness checks before returning
to the existing half-second cadence. The owned child must still hold the
listener, initialize its schema, and answer HTTP health checks within the full
startup timeout. Media and fault observation windows are unchanged.

Scalar observations reuse one private connection on the instance's owning
thread. Every call commits and discards session state, retaining fresh
autocommit visibility and releasing settings, temporary objects and advisory
locks. This connection is separate from `instance.db` transactions and closes
before database removal. Calls from other threads retain independent short
sessions; no additional worker or polling parallelism is introduced.

Tests needing the suite's open discovery settings use
`instance.start(suite_settings=True)`. The real CLI initializes only the fresh
owned database, then the fixture seeds settings before starting the HTTP server.
This avoids waiting six seconds for a cache populated with the wrong initial
values. Default startup retains production defaults. Tests of live settings
changes and cache expiry still use the production cache and its normal TTL.
The interrupted offload recovery drill also seeds its forced `veryslow` preset
while the origin is stopped, before restarting it. The live preset change in
the offload protocol test still waits for normal cache expiry.

The standing peer fleet remains serial. A host-local lock prevents overlapping
pytest runs, including runs from separate worktrees; per-port locks protect
disposable peers. Standing instance settings are restored even if setup fails,
and the lock is held through the settings cache expiry. SIGTERM follows the
same pytest cleanup path as interruption. A force kill cannot run finalizers;
unique database names prevent a later run from silently adopting its leftovers.
Standing foreign peers are still external dependencies, not recreated by this
fixture layer.

Repeated `make_sample_mp4` inputs are encoded by the host FFmpeg once per
duration, dimensions, encoding arguments and encoder file version within a
single Python process. A memory cache retains at most 16 clips and 16 MiB;
larger clips are copied without retention. Each test receives its own writable
file, never a shared path or hard link. FFmpeg must still be present on every
call; replacing it invalidates the entry, and encoding errors propagate.
The cache disappears with the process and does not cover application encodes,
live sources, cold-rendition barriers, or calibrated observation windows.

`./dev provision-peers` installs their host entries and includes their TLS
vhosts in the shared Caddy configuration, proxying to
`host.docker.internal:8422` and `:8421`.

Nothing answers on either domain outside a test run, and neither instance
touches the standing dev server's database.

## Webxdc browser testing

Session runtimes use separate origins under `webxdc.plamenu.local` and
`webxdc.plamenu2.local`. Caddy covers both wildcard TLS names. For persistent
local wildcard DNS, follow [the resolver setup](peers/webxdc-dns/README.md);
`/etc/hosts` only supports individual names.

Follow the [Webxdc browser checks](WEBXDC-UI.md) for invitations, realtime
exchange between accounts and guests, app isolation, and storage administration.

## Native media-object peers

Two fixtures cover publication types that the Note-oriented peers do
not produce reliably:

* `hubzilla-test` runs pinned Hubzilla 11.4 core/addons from local source with
  PubCrawl enabled. `@hazel@hubzilla.local` publishes WebDAV files through
  Hubzilla's native path as top-level `Image`, `Audio`, `Video`, or `Document`.
  The public fixture channel auto-approves contacts, so the
  `plamenu_e2e.hubzilla` helper can exercise signed `Create` delivery of all
  four types as well as upload and publication.
* `funkwhale-test` runs the official Funkwhale 2.0.8 API/frontend images,
  pinned by digest. Its followable
  `@plamenu_audio@funkwhale.local` channel publishes canonical top-level
  `Audio`; `plamenu_e2e.funkwhale` uses the public browser/upload API and waits
  for the Celery import and federation job. Funkwhale 2.0.8 does not fan public
  audio out to individual non-Funkwhale follower inboxes (that fallback is
  disabled upstream), so its ingestion test resolves the canonical object URL.
  This still exercises the real producer, actor fetch, `Audio` parser and media
  extraction. The fixture keeps Funkwhale's default API-authentication setting:
  its listen URL rejects an anonymous GET, then accepts Plamenu's instance-actor
  signature. Its renderer also requires an exact `application/activity+json`
  compatibility retry after ordinary Mastodon-style resource negotiation.

Start either with `./dev up hubzilla` or `./dev up funkwhale`. Both join the
same Caddy TLS network as the existing fleet, trust its local CA for outbound
delivery, and expose optional pytest fixtures (`hubzilla_hazel` and
`funkwhale_fiona`).

## The Owncast live peer

`owncast-test` builds the pinned current Owncast `develop` revision from
source, serves `https://owncast.local` through the shared Caddy CA, and exposes
RTMP ingest only on host port 1937. Its public actor is
`@streamer@owncast.local`; the helper pushes a synthetic H.264/AAC broadcast,
waits for Owncast's delayed go-live Note, and verifies root-page discovery,
status notifications, HLS/segment proxying, the first-party player, feed
deduplication, and the transition to ended. Start it with
`./dev up owncast`; `./dev creds` prints its disposable admin and stream key.

## The onion peer

`tests/test_tor_federation.py` federates with a Mitra instance whose identity
is a generated `http://….onion`, fronted by a real Tor daemon whose SOCKS
port is Plamenu's `[federation] onion_proxy_url` lane (the transport under
test — the peer has no clearnet route by construction). In the maintainer
workspace `./dev up onion` starts the whole rig; it is the only peer that
needs the real Tor network, so it is not part of `up all`, and its tests skip
like any optional peer when it is down. The harness drives the peer's client
API directly at `ONION_MITRA_URL` (default `http://127.0.0.1:8381`) — no Tor
needed from Python.

To run pytest directly once compatible peers and Plamenu are running:

```sh
uv sync
uv run pytest --collect-only -q
uv run pytest tests/test_follow.py
uv run pytest
```

Every test uses fresh marker values and accounts. Default `wait_for` polling
checks early at 0.1, 0.25 and 0.5 seconds, then uses a one-second cadence. This
adds at most three early observations to a long wait. The full timeout remains
available, check errors propagate, and explicitly specified intervals retain
their fixed cadence. Calibrated media observation windows are separate and
unchanged. Failure output identifies the last named `step(...)`. Optional-peer fixtures skip when their service is not
available during a diagnostic run. Peer readiness is checked at execution,
never while importing tests or collecting the inventory.

`./dev e2e-full` forces `--release-matrix --matrix-profile full` and disables
fail-fast execution. A full pass requires the exact source-bound `inventory.json`
selection and completed setup/call/teardown phases. It includes the Tor, Plup and
Mobilizon peers and every explicitly enabled drill. Capability markers name the
required environment flags; the runner never enables destructive drills itself.
Nested capabilities are listed separately in the inventory: the playlist source
sync case requires `AP09_BROWSER_SOURCE_SYNC=1` for full qualification. Its
browser journey uses fresh artifacts for each invocation and verifies every
configured browser project. Ordinary and standard runs retain the core source
sync assertions when that nested opt-in is disabled.
Missing opt-ins or peers, collection errors, unexpected skips, xfails/xpasses,
retries, empty runs and interrupted execution prevent full qualification.
The three documented unsupported peer behaviors (Akkoma outgoing blocks,
destructive GtS Move on a standing fixture, and Mitra reports) retain exact
test-and-reason exceptions and remain visible in evidence.

For a deliberately narrower, strict ordinary-federation run:

```sh
./dev e2e --release-matrix --matrix-profile standard
```

This profile excludes the declared opt-in drills and optional-peer modules. Its
report lists the exclusions and can record `profile_pass: true`, but always
records `full_pass: false`. Release qualification requires the full profile.
Explicit test paths, `--ignore`/`--ignore-glob`, `--lf`, `--sw`, or further
deselection cannot qualify even as a complete standard profile.

The four stack-free manifest/tracker helper tests now live in `unit/` and run
through `./dev test-harness`, together with the standalone fault-proxy tests.
All live cases remain in `tests/`; no behavioral case was removed. Refresh and
review the canonical inventory with `./dev gate-inventory --write` after changes.

Each matrix run writes `target/e2e/release-<run-id>.json`, with the source
revision, tracked changes, configured and checked-out peer revisions, running
container image IDs, and every test phase/outcome/skip/xfail reason. Archive it
with the release candidate; a failed report is diagnostic evidence, not a pass.
Use `--matrix-report PATH` to choose a new location; reuse is refused before
execution. The path is reserved with a non-passing running record, and final
evidence distinguishes selected, expected and completed cases. Source/candidate
changes during execution invalidate qualification. Configured revisions alone
do not prove which code a remote service is running.

Never commit peer tokens, cookies, private keys, local CA keys, or captured
private/direct activities.

The Discourse exchange removes its generated follower on exit and waits for
the peer to process the Unfollow. Its bootstrap also clears abandoned generated
followers after interrupted runs, while preserving standing accounts. Restart
that disposable peer after an interrupted exchange to apply this cleanup;
otherwise stale fan-out can exhaust its default object-fetch rate limit.

A reachable peer website does not establish that its delivery worker is
running. For a Follow/Accept timeout, correlate both servers' queue and worker
logs before changing the test timeout. In the Discourse fixture, Sidekiq can
fail at `Process.setpriority` when its parent threads inherit a niceness that
the worker cannot lower to its requested value. Check the worker supervisor
and inherited scheduling settings; restarting the web process alone may
preserve the cause. After restoring delivery, check for abandoned test
followers as described above before interpreting rate-limit failures. Keep
the normal rate limits and delivery assertions intact.

See [`docs/INTEROPERABILITY.md`](../docs/INTEROPERABILITY.md) for the scope of
the release matrix and how to report a peer-specific correctness gap.

## BookWyrm reading peer

`./dev up bookwyrm` starts and seeds BookWyrm 0.9.3 at
`https://bookwyrm.local`, using its official digest-pinned GHCR image, PostgreSQL,
Redis, a Celery worker and nginx behind the shared Caddy proxy. No application
image build or upstream source modifications are needed. Its matching source
revision is shallow-cloned by provisioning. The disposable login is `lena` /
`bookwyrm-lena-pass-123`; the fixture also creates one local Work and Edition.
`./dev down bookwyrm` preserves its named volumes for subsequent starts.

The private database network defaults to `10.234.240.0/24`; set
`BOOKWYRM_TEST_SUBNET` if that subnet is already in use. The application and
worker use the shared local CA bundle. Host resolution needs the provisioning
entry `127.0.0.1 bookwyrm.local`, plus the Caddy Docker network alias.

`tests/test_bookwyrm_federation.py` uses real browser login, CSRF-protected
search/follow/publication/edit/deletion forms and HTTPS federation. Database
queries only observe results; they do not manufacture follows or delivery events. Its
Plamenu endpoint is a disposable `doomed.local` instance, leaving the standing
`plamenu2.local` peer alone. Run it with the current local candidate:

```sh
PLAMENU_BINARY="$PWD/target/debug/plamenu" ./dev e2e tests/test_bookwyrm_federation.py -v
```

See [BookWyrm interoperability](../docs/INTEROPERABILITY.md#bookwyrm) for the
supported exchanges and deliberate peer limitations.

## Songhive music peer

`./dev up songhive` starts the digest-pinned Songhive 0.4.9 application,
PostgreSQL, Redis and its Celery delivery worker. `./dev down songhive` stops
only this fixture. Source provisioning checks out the exact upstream revision
in `peers/sources.lock`; the image and source are recorded separately.
The standing disposable login is `sylvia` / `songhive-sylvia-pass-123`.

The shared Caddy proxy serves `https://songhive.local`; provisioning installs
the hostname and Docker alias. Both Songhive processes trust the shared local
CA. The fixture-only `sitecustomize.py` allows Songhive's explicit remote
lookup to reach five exact HTTPS peer origins on the private test network.
All other origins keep its upstream fetch guard, and TLS and signature
verification remain active. This overlay is not shipped in Plamenu or applied
to the upstream source. It must never be used in a production deployment.

`tests/test_songhive_federation.py` drives the actual public Songhive API.
It covers signed account and music-library follows, native Audio delivery and
withdrawal, metadata and decoded audio in both directions. Library delivery is
observed before explicit URL lookup so a lookup cannot hide a broken subscription. The fixture is required by
`e2e-full`, which rejects unexpected skips. See the [music guide](../docs/members/music.md) for current member workflows
and the [music API reference](../docs/api/music.md) for integration entry points.
