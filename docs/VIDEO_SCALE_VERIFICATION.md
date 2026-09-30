# Native delivery scale verification

These are reproducible **local workload acceptance budgets**, not advertised
production capacity, WAN latency, or a comparison against other products.
Keep browser decoded-start/seek/rebuffer, P2P/browser concurrency, higher
resolution and multi-node tests separate. Do not turn HTTP-body arrival into
a claim that a browser decoded a frame.

## Sustained mixed origin workload

Before running the workload, the native baseline acceptance bounds are:

- One 640×360/25 fps H.264/AAC native publisher and **32 concurrent progressive
  live responses**, plus **16 concurrent finite-range clients**, for at least
  **300 seconds** after all live clients have joined.
- Every initial live response succeeds, receives `ftyp`, and joins within the
  existing **20-second** initialization bound. No test-side wait for HLS or
  retry of failed startup. Every client emits further bytes; no gap between
  chunks exceeds **15 seconds**, excluding intentional termination.
- VOD bytes offered by actual PeerTube remain exact under concurrent seeks;
  at least **4,000 64-KiB ranges**, zero failed/incorrect requests, local p95
  response time **≤1 second**. This measures range-response latency, not
  decoded seek time.
- Exactly **one live remux child** irrespective of viewer count. Native ingest
  is counted separately. Service-process plus owned-media-child peak RSS
  **≤512 MiB**, server growth from the prepared baseline **≤256 MiB**, and
  last-quarter server RSS exceeds first-quarter mean by **≤32 MiB**.
- Mean service-process/owned-child CPU **≤2 cores** (200% of one CPU).
  Report origin response body bytes, not P2P savings or confirmed wire bytes.
- Total owned media-tree growth **≤2 GiB**; sustained DVR remains bounded by
  the chosen five-minute rolling window. The continuous replay recording is
  separately accounted for and is not described as bounded DVR scratch.
- Creator opt-in anonymous delivery accounting remains enabled throughout;
  its bounded batches continue without breaking delivery. After owner
  suspension, all attached progressive consumers reach EOF within **10
  seconds**, while new VOD/live requests are refused.

Run this explicitly against the disposable-instance/real-PeerTube E2E harness:

```sh
AP09_SCALE_VERIFY=1 e2e/.venv/bin/pytest -q -s e2e/tests/test_video_delivery_scale.py
```

The drill owns its unique database, media directory, encoder and loopback
listener. It never restarts or changes the standing development instance.
Clients discard streamed bytes after validation; no full stream capture or
large trace is created. The aggregate resource log and executable/workload
identities remain in protected `target/e2e/video-delivery-scale/` evidence.
The sample publisher's CPU is reported separately from service CPU; generating
a test pattern is not origin delivery. System/reverse-proxy/PostgreSQL totals
must not be silently included or excluded when interpreting these numbers.

## Decoded browser budgets

`accessibility/tests/video-playback-budget.spec.mjs` defines a separate local
decoder contract **before measurement**, using a real 24-second 640×480/10 fps
H.264/AAC moving-pattern film published through ordinary creator forms:

- Presented moving frame within **10 seconds** of requesting playback. A
  preload/poster frame at time zero does not count. Enhanced measurements use
  the real browser's `requestVideoFrameCallback`, not a response or simulated
  counter. No-JS uses external samples of native decoder/frame/timeline state.
- At least **30 presented frames over eight seconds** of steady playback,
  at most **two seconds** of total waiting and longest individual waiting.
- A seek to 16 seconds presents a frame from that position within **five
  seconds**; neither `currentTime` assignment nor `seeked` alone is proof.
  This short-film seek may be buffered; it is not a cold long-film seek test.
- No decoder error and no horizontal page overflow at 320 CSS pixels.
- Repeat in Chromium/Firefox/WebKit desktop/narrow viewports and Chromium
  with application JavaScript disabled. Where the peer engine is active,
  watch playback must attempt a tracker WebSocket; Playwright routing
  intercepts and closes that attempt. This is controlled client fault injection,
  not a wire-outage benchmark. Routing is registered before any navigation. Origin
  media fragments have a 1.5-second injected delay (initialization is immediate)
  so a small fast film cannot bypass the fault through instantaneous delivery.
  Native-only engines remain server-delivered; do not call them a peer fault.
  Server-only embeds remove that delay, use the real `p2p=0` endpoint and must
  never open a new tracker socket.

Page callbacks are disabled in a genuine no-JS browser. The no-JS case reads
native frame quality (total minus dropped), advancing timeline, readiness and
seek state externally at a nominal 50-ms interval, with a **250-ms maximum
sample gap**. Its startup/seek values are sampled upper bounds; waiting is
sampled readiness, not callback-exact event accounting. Three newly presented
frames plus an advancing target timeline fence no-JS seeking. The test does
not enable application scripts or replace playback data.
It measures decoding, **not native-control keyboard accessibility**. Playback
is muted to isolate decoding from autoplay/audio permission; hardware audio,
assistive technology, live glass-to-glass and WAN performance remain separate.
Retained JSON attachments include startup/seek timings, presented and dropped
frames, waiting intervals, source, dimensions and actual steady duration.

```sh
python3 scripts/run-accessibility-tests.py video-playback-budget.spec.mjs
```

## Native tracker limits and fault recovery

### Concurrent decoded live / actual tracker transport loss

`AP09_CONCURRENCY_VERIFY=1 e2e/.venv/bin/pytest -v
e2e/tests/test_video_live_concurrency.py` owns two 320×180/15 fps H.264/AAC
RTMP publishers and eight simultaneously playing Chromium decoders: four
native and four actual PeerTube, split across the broadcasts. All eight must
advance at least ten presented frames/second and half a media second/second
over 120 seconds of steady playback, 15 seconds of actual tracker TCP loss,
and 30 seconds after restoration, without decoder or integrity errors.
Both broadcasts must receive validated native peer bytes and native uploads
must be positive. Controlled 1.5/4-second HTTP delays establish peer delivery
during warm-up only; delays are removed for the measured phases.

The owned TCP bridge actually aborts at least eight attached native tracker
connections and refuses new tracker connections until restoration. Canonical
TLS, native tracker code, HTTP media and WebRTC are unchanged. Ordinary backend
HTTP uses Connection: close to keep pooled requests from hiding upgrades;
this bridge overhead is not described as production network performance.
Actual reconnects and uninterrupted decoder progression are required.

Source pixels contain a 32-bit decisecond wall clock. Externally reading that
clock from real decoded native frames measures synthetic local RTMP
glass-to-glass latency with 100-ms quantization: p95 must be ≤10 seconds,
with at least two valid samples/second across the four native decoders.
This is not hardware camera/audio, browser-capture, WAN or real-device proof.
Resources are sampled once/second: server plus direct owned media children
peak RSS ≤768 MiB, average CPU ≤3 cores, and at most four FFmpeg children
(two ingest and at most one shared remux per broadcast). Test publishers,
browser driver and bridge are recorded separately, excluded from these figures.
The entire browser drill has a finite 420-second deadline. All bounds are
declared before the initial run; failures are retained, not silently retried.


Peer signaling is in-process and bounded: **2,048 admitted sockets**, **four
eligible swarms per socket**, **128 peers per swarm**, **16 offers per
announce**, **64-KiB messages**, **16-KiB serialized signals**, and **eight
queued outbound messages**. Full queues drop peer signals, not allocate more
memory or block other peers. Ninety announces per minute limit repeated work.
The advertised 120-second interval fits the 180-second successful-announce
deadline. A connection must first announce within 15 seconds; ping/pong traffic
cannot retain an unannounced/idle slot indefinitely. Ordinary relay, failure
and ping writes each have five seconds, and combined VOD/live canonical
authority checks have two seconds. A database error/timeout refuses signaling;
there is no stale-public-allowlist fallback.

Every signal, including unused offers, is validated before database lookup,
peer registration, acknowledgment or any relay. Binary-string IDs remain
PeerTube-compatible (20 codepoints in the byte range). Duplicate identities
on other sockets and cross-swarm answer targets cannot impersonate peers.
Stopped/disconnected peers release registry entries and admission guards.
Unsupported binary messages and oversized frames close the connection.

`e2e/tests/test_video_tracker_limits.py` publishes actual native ladders into
a uniquely owned disposable database. It checks whole-message rejection with
two attached recipients, valid binary-ID relay, cross-swarm isolation,
identity conflicts, four-swarm/offer/frame/rate limits, 128 simultaneous peers
and reclamation, first-announce expiry, an exclusive lock of **only that
fixture's** rendition table, canonical suspension, and decoded byte exchange
between the unmodified PeerTube player and native player after the faults.
The full 2,048 ceiling and guard reclamation are unit-tested against the exact
production admission algorithm; this is **not** a 2,048-connection network
flood or concurrent WebRTC capacity benchmark.

Already exchanged public peer bytes cannot be recalled. Reannouncement always
rechecks canonical authority; idle peers expire rather than polling SQL per
socket. Do not interpret this as instantaneous recall of ICE signaling or
proactive zero-delay teardown of every idle WebRTC connection after moderation.

## Interpretation

The eight-decoder/two-broadcast native-origin workload passed on 2026-09-30:
eight actual tracker sockets were interrupted and reconnected, while every
native/PeerTube decoder continued through all phases; native pixel-clock p95
was 9.5 seconds, peak service/direct-child RSS 328,638,464 bytes, with two ingest
children. The closure record pins candidate/evidence and retains intermediate
harness failures. Bridge-forwarded aggregate bytes are not dedicated origin
body accounting or proof of P2P savings. PeerTube-origin concurrency/corrupted
peer data and browser-capture latency are still separate required gates.

Pass/fail evidence must be added to the AP09 operations record after the run.
Defining this workload does not mark PERF01, PERF02 or M7 complete. A default
debug executable on one development host does not establish production fleet
limits. The separate local decoder slice does not close larger/cold/WAN/device
startup/seek/rebuffer, live latency, slow-client faults, browser/P2P concurrency
or wider independent storage/offload gates; they still need their own retained
measurements.
