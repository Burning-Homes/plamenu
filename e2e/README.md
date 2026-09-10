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

## The Plamenu peers

Two of the peers are this server, started and stopped by the test session
itself from the working tree (`plamenu_e2e/ephemeral.py`): `plamenu2.local`,
which `tests/test_plamenu2_*.py` federates against, and `doomed.local`, whose
whole life is one self-destruct. Each gets a throwaway database on the dev
Postgres, its own media directory and its own port (8422 and 8421).

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

Every test uses fresh marker values and accounts. Failure output identifies the
last named `step(...)`. Optional-peer fixtures skip when their service is not
available; release validation treats unexpected skips as a failed full-matrix
run. `./dev e2e-full` passes `--release-matrix` to pytest: skips are allowed
only in the Tor, Plup, and Mobilizon modules, plus the explicitly documented
Akkoma outgoing-blocks, destructive GtS Move fixture, and unsupported Mitra
reports cases. These three exceptions match exact test names and reason text;
other skips in the same peer modules still fail. Other skips (including collection skips), an empty
run, or deselecting tests fail release validation. Explicit test paths,
`--ignore`/`--ignore-glob`, `--lf`, and `--sw` are also rejected: release mode
must collect the configured full test tree. Expected failures retain
their pytest semantics and are recorded with their reasons.

Each full run writes `target/e2e/release-<timestamp>.json`, with the source
revision, tracked changes, configured and checked-out peer revisions, running
container image IDs, and every test phase/outcome/skip/xfail reason. Archive it
with the release candidate; a failed report is diagnostic evidence, not a pass.
Use `--matrix-report PATH` to choose its location. Configured revisions alone
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
