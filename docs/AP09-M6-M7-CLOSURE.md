# AP09 M6 / M7 closure work

Both original milestones are verified: M6 on 2026-09-29 and M7 on 2026-10-01,
with positive/failure, authorization, PeerTube and browser evidence below.
The original gates and finite workload contract are unchanged. This is not
full AP09 completion or an M9 hardware/WAN/human-AT release certification.

## M6 — Creator/viewer experience

- [x] Verify catalogue/channel/subscription/search authority and bounded paging,
      including ownership changes, anonymous policy and a larger corpus.
- [x] Complete drafts/scheduling/reminders and recovery UX; retain existing
      live calendar and in-session camera/screen switching evidence.
- [x] Discoverable, HTML-first settings and guarded imports/source sync.
      Viewer P2P/history settings and one-time guarded file/ActivityPub imports
      and scheduled public ActivityPub/PeerTube channel/playlist copies are verified.
- [x] Complete stable-identity media editing/reprocessing/trim workflows.
      Replacement, retained-source reprocessing and exact trimming pass local,
      real PeerTube and desktop/mobile/no-JS browser verification below.
- [x] Private opt-in watch history/resume with deletion, bounded retention and
      accessible non-JS controls; never record merely opening a page.
- [x] Download/embed controls and authorization/cache/range matrix.
- [x] Initial live category/licence/warning and independent replay inheritance.
- [x] Remaining desktop/mobile Chromium/Firefox/WebKit and non-JS
      journeys, reflow, keyboard and accessibility checks. Real hardware/AT
      release evidence must stay explicitly separate where required by M9.
- [x] User documentation, inventory updates and original-gate sign-off.

## M7 — Operations and scale

- [x] Worker/storage options and native baseline installation invariants;
      optional DB-less native offload, two workers, crash/expiry/cancellation
      and actual PeerTube plus narrow JS/no-JS decoded output verification.
- [x] Redundancy/mirroring authority, retention and bandwidth controls.
  - [x] Asynchronous native live HLS/complete-recording checkpoints, missing
        primary segment/integrity fallback and replica-only recording recovery
        into independently decoded PeerTube replay publications.
  - [x] Direct finite replica-backed rewind generation and shared bounded remux
        admission, with exact ranges/full decode and narrow no-JS seeking.
  - [x] Same-publisher primary EIO/ENOSPC and lost-replica continuity, both-root
        full/EIO safe stop, retained-prefix recovery and current fresh-root drills.
  - [x] Independent native/PeerTube-source copier serving, signed CacheFile/Undo,
        source-offline first-party playback and fresh DB/root recovery. Stock
        PeerTube bootstrap and simultaneous hard-range-error limits are below.
- [x] Default-on PeerTube P2P, pre-contact opt-out, persistence, live/VOD/embed
      and fallback/fault/browser matrices; no address disclosure without notice.
  - [x] Native tracker whole-message signaling validation, canonical-check
        deadlines, bounded admission/queues and isolated fault/reclamation
        matrix, followed by actual unmodified PeerTube/native byte exchange.
  - [x] Wider live/device/cache-integrity and concurrent peer-delivery matrix.
- [x] Moderation/report/takedown propagation and privacy-preserving analytics.
  - [x] Canonical native origin refusal, acknowledged cleanup and attached-live
        revocation; real PeerTube and seven-project browser proof.
  - [x] Default-off private creator delivery analytics, owner/editor/privacy/
        retention/erasure/failure fences and no-JS controls.
  - [x] Actual forwarded PeerTube video Flag, warm domain-media block/restart,
        suspension/signed end/Delete and acknowledged paired cleanup/debt.
- [x] Acknowledged storage lifecycle/GC; consistent backup and actual restore.
  - [x] Native VOD and committed unfinished-upload fresh-storage recovery.
  - [x] Interrupted native live recording, reconnect and independent replays
        after fresh DB/root restore, on single and paired durable disks.
  - [x] Fresh DB/root remote-cache/mirror/offload recovery and failed disk/claim
        cleanup within the named M7 workloads; no arbitrary hardware/WAN claim.
- [x] Defined workload budgets and concurrency/resource/startup/seek/rebuffer/
      live-latency measurements; retained positive and failure evidence.
  - [x] Sustained mixed native origin resource/range/live-body workload.
  - [x] Local decoded native VOD startup/seek/steady playback and server-only
        embeds across all seven browser projects, with injected signaling failures
        in the six enhanced configurations.
  - [x] Larger/cold/WAN/device/live-latency, P2P-browser and multi-broadcast
        concurrency measurements, optional offload and broader failure budgets.
- [x] Operator documentation, inventory updates and original-gate sign-off.

No push, merge, deployment or PeerTube plugin ABI work is authorized here.

## M7 completion execution contract — 2026-09-30

The owner requested closure of the **entire original M7**, not another
standalone slice. The following finite workloads make its remaining broad
gates executable; they do not waive original functionality or represent
production capacity/hardware/assistive-technology certification.

| Gate | Remaining implementation and required local evidence | State |
| --- | --- | --- |
| Storage/worker scale-out | Native optional authenticated offload, leased/fenced jobs, private sources, retry/cancellation, no extra mandatory installation; two independent worker processes and crash/expiry tests, actual PeerTube outputs | Verified native single-writer/DB-less design, including orderly cancellation and fresh DB/root offload recovery |
| Live resilience | Replica-backed finite rewind without whole recording copies; bounded shared remux resources; healthy-replica publisher continuity under primary I/O failure; lost-copy/full-floor/restoration/session fencing and decoded playback | Shared software byte admission, primary/replica EIO/ENOSPC, both-root safe stop, preserved-prefix replay and single/paired fresh-root recovery verified; no physical-disk-QoS or guaranteed-RPO claim |
| P2P concurrency/faults | At least eight concurrent decoded viewers across actual PeerTube/native players, both origins; positive verified/uploaded bytes, corrupted/stale peer data, actual owned tracker TCP interruption/restoration, opt-out/no-JS and native fallback | Verified both-origin real RTC corruption/stale-byte rejection, no-JS/pre-contact persistent opt-out and eight concurrent decoders through actual tracker TCP loss/restoration, all eight reconnected; current supported browser/fallback matrix passes |
| Remote cache/moderation | Current remote publication/owner policy before cached VOD/live/integrity/range assets; cache warm then suspension/Delete/block, retry/expiry/cleanup and restart; no private caching or failed early refund | Verified warm VOD/live suspension/signed end/Delete, actual forwarded Flag, domain-media block/restart, paired cleanup debt and fresh DB/roots; current checksum-supplementation repeat passes |
| Redundancy | Optional bounded, expiring, consented federation-compatible mirrors with actor/ownership/signature, quota/retention/bandwidth and withdrawal safeguards; two independent instances and actual PeerTube CacheFile interoperability | Verified native/PeerTube-source copies, independent serving, signed CacheFile/Undo and fresh-root recovery; stock receiver limitations disclosed below |
| Performance | Predeclared budgets and retained decoded cold long-film seeks, constrained-network startup/waits, synthetic RTMP/browser-capture glass-to-glass latency, at least two simultaneous broadcasts and peer-viewer resource/origin-byte measurements | Cold/constrained ten-minute film, camera/screen enhanced/basic latency, both-origin eight-viewer body savings and native two-broadcast latency/resources verified; both-origin native p95 6.1/8.7 seconds passes all three live phases under unchanged limits |
| Recovery and sign-off | Consistent fresh DB/storage recovery of native plus warm remote cache/offload/mirror state, failed disk/claim cleanup and credential fences; retained original failures, current candidate identity, docs and original-row sign-off | All named restore workloads, current supported seven-project browser matrix, complete native regression, inventory and documentation verified; signed DCO checkpoint records this acceptance |

The single-writer database/ID design remains in force unless explicitly changed.
Native offload must not silently turn shared-database web clustering into a new
installation requirement. Real hardware/WAN/AT release observations remain M9;
the named M7 workloads themselves must pass here before M7 can close.

## Verified slices

### Verified final completion candidate — 2026-10-01

All **1,200 unique workspace library cases** pass across the complete run and
the focused correction of its one schema-contract mismatch: server 517,
ActivityPub 170, database 442, federation 71. The original full-run failure is
retained in `m7-final-workspace-libraries-20261001.log`; the unchanged remaining
1,199 cases passed there. The corrected schema assertion passes separately in
`m7-final-schema-contract-corrected-20261001.log`. Its exact allowlist now
explicitly reviews immutable checksum and signed CacheFile wire documents;
operational state stays in typed columns and all other unexpected JSON columns
still fail. This is not represented as a second complete workspace run.

All **68 changed-surface integrations** pass: proxy 24, live metadata 3,
distribution 7, mirrors 6, real native worker 28. The partial-tail descriptor
case independently decodes finite output while proving the source was neither
copied nor truncated. **22 shipped-JS/wire units + 7 real tracker-proxy units**
pass. Current strict workspace/all-target Clippy, formatting and online SQLx
cache checks pass. Evidence is under
`target/evidence/ap09-m7-completion-20260930/` (`m7-final-*` logs). Only the owned
test DB was backed up/migrated to 0130; the standing DB/service was not deployed.

The original seven execution-contract rows above are now signed off for these
defined local workloads. Native/PeerTube copies, source-loss/fresh-root serving,
policy/withdrawal/debt safeguards, native offload, disk resilience, capture,
both-origin concurrent P2P faults and stated budgets all have real evidence.
Stock PeerTube bootstrap/concurrent hard-range-error limits and the 256-known-
URI foreign-actor bookkeeping bound are disclosed, not unlimited-capacity or
universal third-party cold-failover claims. M8 compatibility/extensions and M9
independent release evidence remain open; no plugin work, push or deployment.

Current browser acceptance passes **44 applicable / 27 explicitly skipped /
zero failed** across all seven projects, 71 collected cases in **6.5 minutes**:
`target/evidence/ap09-m7-completion-20260930/m7-final-live-browser-audio-20261001.json`.
This is not 71 passes. Chromium and Firefox exercise camera/screen recording,
in-session switching, socket recovery, immediate device release, and bounded
audio-init failure/retry; Chromium also verifies deliberate DVR/keyboard return
live and fatal-player recovery. WebKit's tested capability/encoder-fallback and
calendar controls pass; unsupported capture scenarios and project-specific
journeys remain named skips. All five genuine no-JS authoring/calendar/rewind/
replay journeys pass. These engine simulations are not physical Safari/Android
or human assistive-technology certification.

The preceding browser receipt is retained: **35 passes / 5 Firefox failures /
25 skips**, `m7-final-live-browser-20261001.json`. Minimal standalone Firefox
Web Audio probes hung even after trusted clicks and with autoplay restrictions
removed; the headless image lacked an audio sink. A private test-only PulseAudio
null sink fixes the environment without replacing recording/browser APIs or
contacting host hardware. The client now separately bounds audio initialization
to five seconds, releases devices on failure, announces a localized error and
permits a real retry/external encoder. No production companion is introduced.
The final browser-serving executable SHA-256 is
`5fb689c84c8dc91af05fb83625c13e86ed825904093321ff56c3defc7dff1964`,
the pinned-engine test image digest
`sha256:903bdd608d8afcdd017eb68810abbbecb430ae5ccff20853bd72944cec0b1a58`.
Its only production changes after the two-origin latency receipt below are the
additive capture-audio deadline and localized field, not the RTMP/HLS/tracker
paths. Complete native-library/changed-integration sign-off is recorded above;
no gate is waived by this browser result.

The current two-origin live matrix passes **2/2 in 621.96 s**:
`target/e2e/ap09-m7-final-both-origin-live-20261001.xml`. Both run the same
frozen executable SHA-256
`4442adbe944f6952b4b8d693676532abe3cae274594b8f831e64680286884028`.
Each has two broadcasts, eight real native/PeerTube decoders, **eight active →
eight actual TCP interruptions → eight reconnected** (16 total opens), and
120-second steady / 15-second outage / 30-second restoration phases. Native
decoded pixel-clock p95 is **6.1 s for native origin / 8.7 s for PeerTube origin**
in all three phases, with zero decoder/integrity errors. Peak native-service/
direct-child RSS is **339,689,472 / 287,490,048 bytes**, mean CPU
**0.0435 / 0.0348 cores**, maximum FFmpeg children **2 / 1**, within unchanged
budgets. Measurement excludes PeerTube/test publishers/browsers/bridge; it is
not a physical hardware/WAN capacity claim. Receipts:
`video-live-concurrency/liveconcurrencybdbe0984-4jl032t4/` and
`video-peertube-concurrency/ptconcurrency5d298413-jqbpnaoe/`.
The tightened harness's all-eight reconnection predicate also passes when
replayed against both retained control receipts; it does not claim a new run.
The subsequent supported seven-project browser/full native sign-off is above.

### Earlier completion attempts — retained failures and corrections

The checksum-supplementation batch is **2 passes / 1 failure in 492.20 s**:
`target/e2e/ap09-m7-current-hash-live-cache-20261001.xml`. Both actual
warm-cache/abuse/domain-block/restart cases pass, including fresh DB/paired-root
recovery (`video-remote-cache/remotecache9b8b7ab5-0lw15qn6/` and
`remotecache0ac7f393-ndabtid2/`). The foreign live case now has eight continuing
decoders, verified peer transfers and **zero integrity errors**, but one returned
DVR receiver remains at **10.2-second p95**. This is a retained real native
latency failure, not a passing run. Its full observations are under
`video-peertube-concurrency/ptconcurrency5b6b0703-xomy5m7o/`.

The current return-live path uses HLS's current synchronization target rather
than clamping it to a stale loaded-buffer tail. Its configured edge delay is
**four seconds**, with **five seconds** maximum, leaving headroom for a foreign
origin's four-second segment publication; that is not a four-second
glass-to-glass claim. Deliberate DVR still disables automatic edge recovery.
The unchanged ten-second workload gate must pass in both origins' full measured
phases. Reference behavior: [HLS 1.6.16 latency controller](https://github.com/video-dev/hls.js/blob/v1.6.16/src/controller/latency-controller.ts).

Final native checks pass **517 library + 24 proxy + 7 distribution + 6 mirror
integrations**, **3 database mirror tests**, strict workspace/all-target Clippy,
formatting and online SQLx cache verification against the **owned test database
only**, backed up before migration 0130. The mirror offer/Undo-before-Create
matrix now also runs with one configured DB connection. Every generated optional
TOML table preserves required root settings. Built documentation/link checks
pass. Current browser and both-origin live reruns are not yet signed off.

Latest foreign mirror receipt: **1 pass / 1 foreign-live failure in 314.55 s**,
`target/e2e/ap09-m7-peertube-copy-stalled-outage-live-20261001.xml`.
The actual PeerTube-origin copier acknowledges ten assets, independently
decodes the complete graph, survives fresh DB/media-root recovery, refuses a
wrong wrapping secret and serves cold enhanced/basic native players. The
unmodified PeerTube player decodes three actual copy responses with the
primary media body unavailable; startup/seek are **32/493 ms**. Native
enhanced/basic startup is **567/71 ms**, seeking **61/59 ms**. Signed Undo
removes PeerTube's advertisement; physical acknowledgements precede zero
charge. Source:
`target/e2e/video-federated-mirrors/federatedmirror2c3b1194-peertube-bc48ic9d/`,
candidate SHA-256
`0c382129c4c1af4f204f4a84a4371436631423c6cfb70c8896ea306259eb0f1b`.

Stock PeerTube does not mirror its finite MP4 initialization request, so this
drill permits only that bounded origin bootstrap (under 256 KiB), not media
body requests. The injected primary read stalls for 500 ms before failing.
Immediate hard errors exposed PeerTube's file-URL-keyed redundancy map race
between concurrent ranges; those failures remain retained and are not claimed
as universal stock-player failover. No receiving-player code, random choice,
or fabricated media response is substituted. Plamenu's own restored copy
serves with its source process stopped, without that bootstrap dependency.

Foreign-origin live remains open. The diagnostic receipt
`target/e2e/ap09-m7-live-checksum-diagnostic-20261001.xml` identifies missing
new-segment hashes in PeerTube's delayed checksum document, not mismatched
peer data. The native cache now derives missing entries solely from complete
bytes fetched directly from the authorized origin, preserving supplied hashes
and current-source/session/policy fences. Its regression and full eight-decoder
tracker-loss verification must pass before the corresponding gate is signed.
That candidate targeted five seconds with six seconds maximum latency;
the later current correction and its remaining verification are recorded above.

The subsequent nine-case receipt is **3 passes / 6 failures in 1345.01 s**:
`target/e2e/ap09-m7-mirror-capture-peer-concurrency-bytes-20261001.xml`.
Camera and screen capture, immediate device release and fully decoded
independent replays pass. Native enhanced/basic p95 is **6.0/8.1 s** (camera)
and **5.9/9.9 s** (screen); stock PeerTube is observed separately. Source:
`target/e2e/video-capture-budget/capturebudgeta6a865f1-krv0u94f/`.

Both seed-inclusive eight-viewer, 1-MiB/s/250-ms real-TCP comparisons pass.
Native-source body emissions are **22,381,019 bytes off / 5,467,110 on**;
PeerTube-source **25,328,890 off / 6,340,471 on**. All eight sharing viewers
accepted peer segments, maintained real decoders and reported no page or
integrity errors. Scope is **Plamenu HTTP media body emissions including the
seed**, not foreign-upstream accounting. Mean native service CPU is
**0.023/0.033 cores**, peak RSS **253,390,848/228,999,168 bytes**, below the
predeclared three-core/768-MiB limits. Receipts are under
`target/e2e/video-peer-bytes/peerbytes8ca22741-native-3wi5ewl2/` and
`peerbytes700df179-peertube-f6_02bti/`; candidate SHA-256 is
`1453aecbaf58aae60ed7a581ed6bd991d2ae1f78725046dc0bfd7079b4e2313b`.

Six cases still failed. Both mirror copies reached ready (the native source
authenticated Create), but the fixture incorrectly read the authorized-fetch
CacheFile anonymously. Both malicious-wire cases changed and received actual
RTC bytes, but the event counter missed upstream `peer-closed` with a
`validation-failed` cause. Native-origin concurrency now passes its native
latency/decoder phase, but only seven sockets opened: a cold seed abandoned
sharing after an initial checksum 404. The foreign-origin fixture used mode 3,
in which PeerTube itself disables peer delivery. The corrections retain all
old failures and are being rerun, including a new fresh DB/root mirror restore
and signed post-restore Undo. These partial results do not sign off the full
concurrency, integrity, redundancy or recovery gates. The imported-native
revisioned playlist and exact signed sibling-rendition regression passes
**22/22** proxy integrations; traversal and arbitrary sibling requests remain
refused before origin transport.

The corrected **native-source mirror case now passes in full** on candidate
`bbe1b69c325de53dec1c6356038d9015ac683ff60f0f3aae1146185d31c57c5e`.
Eight assets acknowledge **968,345 bytes**, with a 4.068-second whole-job
elapsed time under the 1-MiB/s ceiling. An independently restored database and
media root retain all 11 stored files exactly, reject the wrong encryption
secret and preserve the original signed CacheFile. The original root is gone
and the source process is stopped during cold 320px playback. Enhanced/basic
startup is **622/71 ms**, seeking to 12 seconds **60/64 ms**, with continuing
decoders, no page errors, no enhanced-to-progressive fallback and no horizontal
overflow. Actual signed Undo reaches the source after restore; all asset
deletion acknowledgements precede zero charged bytes. Source:
`target/e2e/video-federated-mirrors/federatedmirror7a5d6b7b-native-0aedhfe_/`.
The concurrently running batch's PeerTube-origin mirror and four P2P/fault
cases are not yet signed off by this native-source result.

That batch completed **4 passes / 2 failures in 929.99 s**:
`target/e2e/ap09-m7-six-failure-fixes-20261001.xml`. Both actual-origin
malicious RTC corruption/stale-data cases pass checksum refusal and sustained
decoded fallback, including real PeerTube cross-product recipient bytes and
pre-contact persistent opt-out/genuine no-JS. Native corrupt recovery uses the
explicitly disclosed standard player at 480p; the other fault recoveries remain
240p. This is not falsely described as every fault preserving adaptive quality.
Receipts: `target/e2e/video-peer-integrity/peerintegrity22788830-native-1ltgdgby/`
and `peerintegrityb9fb5b13-peertube-1poymu8h/`.

Native-origin concurrency now passes **eight active sockets → eight actual
interruptions → eight reconnected sockets**, with real HTTP/RTC/decoders.
The 120-second steady, 15-second outage and 30-second recovery phases measure
**8.6-second p95**, under the unchanged ten-second native bound. Receipt:
`target/e2e/video-live-concurrency/liveconcurrency034da214-tkh9sy5b/`.
The whole-path body-emission comparison above includes shipped engine prefetch
behavior; it does not isolate the causal savings of RTC alone.

The foreign-source mirror attempt correctly withdrew when its canonical
publication changed during still-running PeerTube transcoding. Its preparation
now waits for owned jobs to finish. The foreign live fixture wrongly expected
a 240p quality selector on a single-rendition live; the next run uses actually
advertised quality and checks decoded height. Native edge recovery now uses
six/eight **seconds**, not three/four segments, to preserve its budget on
four-second foreign segments. Current-authority checks reuse the owned writer's
transaction connection; a one-slot pool no longer deadlocks. **515 library +
22 proxy + 7 distribution + 6 mirror integrations** and strict workspace/all-
target Clippy pass. Remaining original gates are still open pending full
foreign interoperability, multi-node failover and current browser sign-off.

2026-10-01: the native federation copier is now wired into the same executable,
with explicit operator API/source approvals, HTML creator consent, paced bounded
downloads, complete graph acknowledgement, permanently private asset keys,
current source/expiry/update fences and transactional signed Create/Undo.
Three database lifecycle tests and five server tests pass, including source
cascade retaining physical debt, replayed offers refusing resurrection, a held
writer surviving caller cancellation, normal HLS/no-JS range reads from a copy,
and private flat-key denial. A regression test first caught that flat-key leak;
the failing receipt is retained. The library run passes 515 tests and the
playback/distribution regressions pass 21 + 7 tests. These do **not** replace
the required real two-instance/PeerTube, fresh-root and browser mirror gates;
their dedicated drill is implemented and verification is pending.

After the reboot, the corrected seven-case batch is confirmed complete:
**2 passes / 5 failures in 747.32 s**. Both remote-cache cases passed, including
the fresh-root signed Flag, warm-domain block, restart and paired cleanup.
Camera native no-JS p95 improved to **8.1 s**, but screen p95 remained **11.3 s**;
the whole capture case is therefore still a failure. The current shared basic
remux joins two closed segments instead of three; fresh measurement is pending.
Native-origin concurrency failed the **native-only** latency assertion at
**10.5 s**; unlike the capture helper, this helper does not read foreign pixel
clocks. This remains a real native failure, not a PeerTube buffer explanation.
The next receipt also retains complete measured phases before assertions so
an over-budget failure cannot discard its clock observations. The
PeerTube-origin case failed before playback because the
harness incorrectly assumed one Docker network. It now selects the unique
network shared with the actual Caddy fixture. Malicious-wire verification now
starts with a genuinely fresh receiver; a media-buffer miss was insufficient
because the engine prefetches farther. The 65-case browser regression run was
interrupted by reboot, not passed. All failed receipts remain retained.

The missing mirror implementation has now begun: migration 0130, private quota
reservations, generation-fenced jobs, retirement debt, explicit HTML/API creator
permission and finite CacheFile vocabulary are implemented. **2 database
reservation/reclaim tests, 2 CacheFile wire tests, 512 library tests and 10
distribution/live-metadata integrations pass**. This is foundation evidence,
not end-to-end mirroring: copy/serve/withdrawal, authenticated offer handling,
actual PeerTube interoperability, mirror restore and browser/origin-byte gates
still remain. The standing database was not migrated or deployed.

The second five-case capture/integrity/cache batch is retained as **one pass
and four failures**, not sign-off. Same-root remote-cache reporting, operator
domain blocking, restart, paired deletion acknowledgement and re-fetch passed.
The fresh-root report reached PeerTube but was correctly refused: disposable
servers reused `plamenu2.local/actor` with different keys. The recovery drill
now uses a private retained test-only `doomed.local` instance identity; it
does not clear PeerTube's cache or replace a standing server's identity.

Camera measurements were native enhanced **4.6 s**, native no-JS **11.2 s** and
stock PeerTube **10.8 s** p95, with progressing decoders. Stop also failed its
immediate capture-track-release check. The current candidate sends a complete
shared MP4 fragment at its closed `mdat`, rather than waiting for the next
`moof`, and releases capture tracks before waiting for network acknowledgement.
These fixes still require fresh camera **and screen**, complete audio/video
replay and browser regression receipts. The native **10-second** target stays
unchanged. PeerTube 8.3's stock DEFAULT live mode retains five segments by
design (`hls-options-builder.ts`); its latency is reported separately from the
native target, without changing its player or mislabelling the advertised mode.
The initial over-broad three-player latency assertion remains a recorded
failure, not a retrospectively passing receipt.

Both-origin malicious-wire cases also failed their initial checks. The revised
drill must establish an actual accepted peer segment, then cold-seek outside
the receiver's buffered ranges before mutating copied outbound RTC media
bytes. HTTP completion now uses the same complete-segment checksum validator:
an HTTP fallback can contain bytes already received from a peer. Real rejection
counters and decoded HTTP recovery, not chunk counts alone, are required.
The wider native/PeerTube eight-viewer tracker-loss runs remain unverified for
this candidate. **M7 remains open**, including federated CacheFile mirroring
and dedicated matched origin-byte measurements.

### M7 shared live admission, paired loss/floors and current fresh-root recovery — 2026-09-30

Seven real PeerTube disk/recovery cases pass in the **707.24-second eight-case
batch**: primary ENOSPC/EIO, replica EIO, both-root ENOSPC/EIO, and single/paired
fresh DB/media-root restore. The eighth case, browser camera latency, failed
its declared no-JS p95 budget (**10.1 s versus 10 s**) and is retained as a
failure; it is not included in these seven passes. All eight fixtures used
candidate SHA-256
`dc0c6efc6a3fac56b71daeb75debb40cceb1359e468776703e232921ffb5046d`.

The software mirror byte schedule is shared across flat copies, checkpoint
copies and copy-format live input. Bounded input charges/metadata reservations
and a wall-clock re-encode share prevent one independent per-publisher timer
from multiplying the configured admission rate. The schedule has a two-second
grant horizon and owned reservations release at encoder exit. This is **not
physical device QoS** or zero-burst certification; details are in the storage
guide. Missing/replaced replica directory identity cannot count as a healthy
floor. Checkpoints fsync newly acknowledged immutable segments, rather than
re-fsyncing every old segment in a long DVR window on every tick.

Actual replica EIO retains the same encoder/session and three progressing
native enhanced/no-JS/PeerTube decoders, then publishes a fully decoded
**57.469-second** independent replay. Both-root ENOSPC/EIO stop on-air
authority in **1.014/1.014 seconds**, reap the encoder, refuse the old swarm/HLS
and recover a **6.037-second** last closed prefix after repair/restart.
An unreadable recording is preserved rather than classified as absent. The
finite descriptor replay path excludes incomplete tails without a whole-file
copy or source truncation; real native processing/full decode and oversized-
prefix refusal pass integration verification.

The configured static run passes **509 library + 58 native-video/live/worker
integrations** and strict workspace/all-target Clippy. An initial static command
omitted the disposable database URL: its database tests failed before executing,
and that log remains separate from the correctly configured passing run.
Protected evidence: `target/e2e/ap09-m7-capture-loss-restore-20260930-initial.xml`,
`target/e2e/video-live-floor/`, `target/e2e/video-live-disk/`,
`target/e2e/video-live-restore/` and the `live-budget-floor-*` logs under
`target/evidence/ap09-m7-completion-20260930/`. Exact failed media, backups,
escrow and the capture-budget failure remain private. No standing instance was
refreshed. **M7 remains open** for the remaining named broad gates.

### M7 real disk continuity, warm-cache restore and cold delivery — 2026-09-30

The complete five-case batch passes **5/5 in 455.29 s** against actual PeerTube
8.3.0. All fixture processes use candidate SHA-256
`373baeaea05e40e35ffa5a31e4c4f67646173248f7d4bef3d7efb2d9d1a4a6e5`.
**504 library + 54 native-video/worker integrations**, strict workspace/all-
target Clippy, formatting and the owned FUSE/TCP guard units pass. No standing
instance or schema was refreshed; no push/merge/staging deployment occurred.

**Primary EIO and ENOSPC:** an owned unprivileged FUSE fixture returns the real
errno from existing open writers (5/28), without filling the host filesystem.
The same native ingest encoder/session keeps writing an independently durable
replica. Actual enhanced/genuine no-JS 320px and PeerTube players sustain six
seconds healthy, 24 seconds fault and eight seconds repaired. Initial eight-
second decoder settling is recorded separately (including two initial no-JS
drops); there are zero newly dropped frames or decoder/integrity errors in the
measured phases. Finite replica rewind fully decodes audio/video during the
fault. After repair the longest surviving recordings become independently
decoded real PeerTube replays (**57.469 s ENOSPC / 56.470 s EIO**) with exact
ranges. Special `:`/`|` replica paths do not enter the tee mini-language.

Initial failures remain: absolute-startup drop counting, the genuine backwards
timeline regression after repair (fixed by newest closed-segment selection),
and seven repaired-phase drops during an overlapping compilation. The final
decoder phases ran without concurrent builds; the fault/repair phases still
require zero newly dropped frames, with initial settling retained separately.
Long final-recording selection no longer compares two truncated per-tick box
scan batches; >3000-fragment tests cover longer primary/replica and invalid
offsets. Replica descriptors open before the marker disables the legacy copier.
The narrow no-JS EIO screenshot was visually inspected. This does **not** close
shared live bandwidth/RPO, lost-copy/full-floor or wider recovery gates.

**Warm cache fresh-root recovery:** actual remote VOD caches and **76,676 bytes
of failed paired-deletion charge** restore into a fresh database and two new
media roots. Exact manifests/cache rows/debt and original federation identity
survive; wrong-secret escrow is refused. Retired direct keys remain inaccessible,
replacement bytes remain exact and replacement keys are not reused. Repair
acknowledges actual deletion from both restored roots before refund. The
restored consumer also passes real live warming, verified-channel suspension,
signed live end and signed Video Delete. The same-root baseline passes alongside
it. This is not federated CacheFile-mirror recovery or wider abuse certification.

**Cold 600-second film:** the real source is **197,363,398 bytes**. Four fresh
Chromium contexts actually seek to 570 s outside all pre-seek buffered ranges,
with new distant HTTP ranges and newly presented frames. Local enhanced/no-JS
starts are **372/181 ms**, seeks **314/267 ms**. A real shared **8-Mibit/s,
80-ms-minimum-header-RTT** TCP profile yields starts **2989/1077 ms**, cold seeks
**1681/1120 ms**. Each lane presents **180–181 frames in 12 s**, zero sampled
waiting, no decoder/page/overflow error and no tracker contact. Emitted origin
media body payload is separately counted, excluding headers/chunk framing and
signaling; it is not P2P savings. The constrained no-JS screenshot was visually
inspected. The predefined contract is in the scale guide; this is not arbitrary
WAN loss, hardware audio/camera, all-browser cold delivery or browser-capture
glass-to-glass certification.

Protected evidence: `target/e2e/ap09-m7-io-restore-cold-20260930.xml`,
`target/e2e/video-live-disk/liveio9acd9ed3-enospc-6ebn1irk/`,
`target/e2e/video-live-disk/liveio87b0bc84-eio-c_5a2qh3/`,
`target/e2e/video-remote-cache/remotecachecd3fe273-h65h42xu/` and
`target/e2e/video-cold/coldfilm8013f4a7-5xdy6g2m/`. Exact backup/media/leaf-key
escrow, failed primary recordings, acknowledged prefixes and failure receipts
remain private. **M7 remains open**, with its original remaining gates intact.

### M7 orderly offload cancellation and fresh-root recovery — 2026-09-30

The worker recovery drill now passes after killing the owned origin during a
real forced encoder run. The worker's orphan lease expires without a database
timestamp rewrite. A consistent database/media backup restores into a **new
database and media root**; wrong encryption escrow is refused and the correct
escrow passes audit. Source bytes and reservations survive. The original claim
expires naturally, generation **1 → 2** finishes through the native TLS worker
without hidden local fallback, and actual PeerTube accepts the independently
published replay with exact ranges and full audio/video `-xerror` decode.
This recovery drill does not add a browser startup measurement.

An ordinary confirmed HTML cancellation now signals the origin's active
processing future and sends the processor's existing bounded DELETE, rather
than waiting for orphan expiry. The measured retirement is **5.068 seconds**;
the private encode never completes/publishes. Aborted futures and origin crashes
retain the finite renewed-lease fallback. Authentication and publication fences
are unchanged. An independent transport-progress timer closes stalled output
readers as well as incomplete requests (15-second checks, 15–30-second idle
bound), preventing a stalled authenticated connection from pinning completed
worker admission for the four-hour whole-transfer deadline.

Both strengthened offload cases pass in the four-case fault batch; its two
initial live-disk failures are retained separately and are **not passes**.
Candidate SHA-256 for both successful offload cases is
`238a65424476e14d885f3a296965f7e742618c3f386f2d85be0f9dacba4e952d`.
Each owned origin/worker/recovery process uses a frozen copy of the running
executable, independent of later workspace builds. Protected evidence:
`target/e2e/ap09-m7-real-io-offload-20260930-initial.xml`,
`target/e2e/video-offload/offloadbf615e69-vu4dswsd/` and
`target/e2e/video-offload-restore/offloadrestoree674197d-vqz_6hxm/`.
Archives, key escrow, leaf TLS keys, sources and failures remain owner-only.
Earlier fresh-root passes and the premature-private-completion failure that
prompted orderly cancellation remain retained. Wider warm-cache/mirror restore,
live resilience, P2P fault and performance gates still keep **M7 open**.

### M7 native private encoding offload — 2026-09-30

Optional `[[video_offload]]` targets now use the same installed executable's
private `video-worker` mode. The default remains native local processing with
no additional service. Workers start before origin database/ID initialization
and need no origin encryption key or database credential. This preserves the
existing single-writer design, rather than introducing multiwriter clustering.
The architecture preference question received no answer; the existing
invariant and optional native-worker design were retained and announced.

Private token authentication, native TLS/system trust, connection/job/scratch
admission, finite uploads, thirty-second renewed leases and a four-hour job
lifetime are enforced. The origin independently validates output bytes,
digests, codecs, dimensions, generated paths/playlists, posters/storyboards and
HLS range hashes before its unchanged journal/claim/quota/publication fence.
Invalid or unavailable processors fall back to the next target/native lane;
original sources remain at the origin. Workers never publish or fetch source
URLs. Cancellation/expiry and crash scavenging keep scratch private and owned.

Two independent native processors (one real native TLS) pass actual PeerTube
8.3 federation and decoded playback, plus 320px native enhanced/genuine no-JS
playback, exact ranges and full FFmpeg audio/video `-xerror` decode. The initial
complete drill passes in **181.09 seconds**, candidate SHA-256
`d30dfba48f7042560d4474e7cf4dc061f7b9500876bf56d4ebbcbc64b4488639`.
It kills worker A **while a real forced encoder runs**, verifies that child
dies and the retained origin source is byte-identical, then completes the same
draft through surviving TLS worker B. Restart scavenges abandoned owned
scratch. Actual unrenewed lease expiry denies late renewal. Ordinary HTML
confirmed cancellation stops renewal and retires worker B's encoder and
reservation without any publication or public HLS. Positive cases explicitly
refuse silent local fallback.

Protected evidence: `target/e2e/ap09-m7-offload-20260930-browser.xml`,
`target/e2e/video-offload/offloadae6cd0f3-gzpgeyg1/` and
`target/evidence/ap09-m7-completion-20260930/offload-*.log` retain private
configuration/fixture leaf keys, source and decoded films, worker/origin logs,
screenshots and observations. The narrow no-JS screenshot was visually
inspected. Initial failures (tools-only configuration requiring a secret,
missing optional Python process library, thread-local child observation and
incorrect fixture cleanup method) remain retained; production authentication
and admission were not relaxed. Child observation now uses Linux procfs with
start-tick identity and no new dependency. A strengthened final seek check
requires readiness and completed seeking, not just `currentTime` assignment.

The final strengthened drill passes in **157.56 seconds**, candidate SHA-256
`dbe98a3d5a26876ffb01e5f1dac84b92186c0d688cd95d3005480011ceec3449`;
its running origin and both workers use the same artifact. The six browser
journeys have zero drops/errors/overflow, sampled starts **72–302 ms** and
decoded seeks **59–65 ms**, within the predeclared 10-second/3-second bounds.
These eight-second-film seeks may be buffered: they do **not** satisfy the
separate cold long-film gate. Final evidence is
`target/e2e/ap09-m7-offload-20260930-final.xml` and
`target/e2e/video-offload/offload5a3449fd-edgdqgu9/`. A frozen verified executable
is retained under `target/evidence/ap09-m7-completion-20260930/verified-offload/`.

**500 library + 54 video integration tests** pass, including unavailable-worker
native fallback with retained source/private-publication fences. Strict
workspace/all-target Clippy passes. Wider original M7 failover, redundancy,
both-origin faults, cold/capture performance and fresh-root recovery gates
remain open; this worker sign-off does not close them.

### M7 replica rewind, concurrent live and remote-cache retirement — 2026-09-30

The following sub-gates pass; **M7 remains open** under the full execution
contract above. No original offload, federated redundancy, publisher failover,
both-origin corruption, cold-film/capture or wider recovery gate is waived.

Replica rewind pins a finite selection of closed replica segments, with a
freshness/session/ownership fence, 512-MiB source ceiling, private hard links,
shared 1-GiB remux admission and final canonical authority check. It never
copies a whole growing recording or starts another ingest encoder. The
single/paired fresh-root actual PeerTube recovery cases both pass in **556.48
seconds**. In the paired case, the primary recording is moved into protected
fault evidence; the replica-built MP4 has exact 206/416 behavior and full audio/
video `-xerror` decode. Genuine no-JS Chromium at 320px decodes nine initial
frames, seeks to three seconds in a sampled **151 ms**, presents thirty total
frames and reports zero drops/errors/overflow. The pins are released. Its
recovery/rewind candidate SHA-256 is
`7cf79574cb81914103e4f7faa20656956886093dd917b0d6d9205b9a76bd4d6a`.

Two native-origin 320×180/15-fps broadcasts sustain **eight independent real
Chromium decoders** (four native, four actual unmodified PeerTube), over 120
seconds steady, **15 seconds actual native tracker TCP loss**, then thirty
seconds recovery. All eight keep decoding; all eight tracker connections are
actually interrupted and reconnect. Both broadcasts have positive validated
native peer bytes and uploads; measured phases have zero decoder/integrity
errors or dropped frames. Native pixel-clock p95 is **9.5 seconds** under the
predeclared ten-second bound. Peak server/direct-child RSS is **328,638,464
bytes**, average CPU **0.0386 cores**, with two ingest children; generators,
browsers, bridge and database/proxy resources are excluded and recorded
separately. The passing case takes **226.33 seconds**; candidate SHA-256 is
`36fda75349768c942bb70420801d3e15e7e9ecfd0890aa01d07842a353a6ad2d`.
Warm-up HTTP delays are removed before measurement. Aggregate bridge bytes
include protocol traffic: they are **not** an origin-byte savings measurement.
This is native-origin local RTMP proof, not the full both-origin/corrupted-peer
or browser-capture/hardware/WAN contract. The 320px screenshot was inspected:
the address-sharing disclosure, default-on control and rewind link are visible.

Remote cached playlists/ranges/live children/integrity now recheck current
public/channel/live authority after waits or origin I/O. Checksum UUID/URL
replacement is fenced separately. Restored orphan generated cache files are
not public without their acknowledged row. Live-cache count, URL, TTL and
ordering/index metadata are bounded; its shared-state tests serialize their
reset under normal parallel execution.

Remote range finalization has eight slots/512 MiB conservative scratch/copy
admission, a private known-size pre-write journal and a cleanup-row lock. Its
owned task retains single-flight/admission across caller cancellation. Fresh
entropy prevents late cleanup from deleting a refilled key. Migration 0129
captures retirement bytes atomically, even on FK cascade; charges remain until
the cleanup worker acknowledges removal, including both paired disks.

Actual PeerTube 8.3 VOD/live warmed-cache paired-disk E2E initially passes in
**80.25 seconds**, then the final checksum-fenced candidate passes in **189.05
seconds**, including its build: restart retains exact bytes; failure of one owned replica
key retains **76,676 bytes** of cleanup debt, refetch creates a distinct key,
repair acknowledges both old copies without deleting the replacement, and
current channel suspension/signed live end/signed Video Delete revoke warmed
assets. Candidate SHA-256:
`769256857c286b54143efc0f10cf34be93ca5179dff8de7dd7353e5bab7d7dbf`.
The final harness captures the running executable through `/proc`, and verifies
the restart uses the same artifact, rather than hashing a later rebuilt pathname.
This is same-root consumer restart/paired cleanup, **not fresh-root remote
cache/offload/federated-mirror recovery**.

Protected evidence:

- `target/e2e/ap09-m7-replica-rewind-20260930.xml` and
  `target/e2e/video-live-restore/liverestore4355d7d0-_qr8x2s4/` retain decoded
  rewind/browser JSON/screenshots plus fresh-root backup/escrow/fault media.
- `target/e2e/ap09-m7-live-concurrency-20260930-final.xml` and
  `target/e2e/video-live-concurrency/liveconcurrencyf93540de-4zxyha_l/` retain
  phase/frame/socket/resource/clock observations and narrow screenshot.
- `target/e2e/ap09-m7-remote-cache-20260930-final.xml` and
  `target/e2e/video-remote-cache/remotecache1db1ae3b-odz122jd/` retain the
  source film, current-origin playlist, private server log and cleanup result.
- `target/evidence/ap09-m7-completion-20260930/` retains all Rust, strict
  Clippy, SQLx, test-bridge and original E2E diagnostics. Intermediate bridge
  fixture failures cover repeated PeerTube login/session handling and cancelled
  warm-route/unroute drainage; they are not concealed product passes. The
  initial remote-cache failures held PeerTube master URLs that its still-active
  transcoding jobs rotated; the corrected fixture waits for **only its owned
  video's** jobs/current readable master/federated Update, without clearing the
  product's fetch budget. Older failed socket evidence can contain private
  PeerTube tokens; keep it private. New socket URLs omit query strings.

Production regression receipt: **490 library + 83 integration tests** pass
(21 HLS, 30 media, 27 native-video, five takedown); **29 database media/cleanup
tests** and **seven normal-parallel live-cache tests** pass. Strict workspace/
all-target Clippy, SQLx offline metadata check, format, built documentation
links and Python/Node syntax checks pass. The intermediate checksum test
initially supplied an invalid empty tracker fixture; the corrected real-shaped
fixture passes without relaxing the database constraint. No push, merge or
staging deployment occurred.

### M7 active live checkpoints and lost-primary recording recovery — 2026-09-30

Optional paired storage now checkpoints closed HLS segments and complete MP4
fragments with the existing native encoder and storage budget. One worker per
admitted publisher has a two-second tick, no backlog, a twenty-second operation
deadline and non-blocking writer admission. Busy/broken replicas report degraded
checkpoint transitions; they do not block ingest. No new companion service or
mandatory installation step was added.

Every copied segment is synced before playlist publication. Current/previous
windows and failed-copy pruning bound retention. Traversal/symlink/ownership
and free-floor checks apply to both roots. Replica HLS expires after thirty
seconds; stale/future primary playlists cannot repeatedly refresh frozen media.
Canonical current-owner/publication/session checks remain before primary or
replica reads. Integrity manifests use a surviving copied segment when primary
bytes are missing; native progressive playback retains its shared remux.

Recording replication scans bounded MP4 box headers, appends only complete
`moof`/`mdat` pairs and syncs data before a private committed-offset marker.
Uncommitted tails are excluded from recovery. Startup restores a missing primary
recording from its exact acknowledged prefix, then applies the existing durable
reconnect/expiry/replay fences. Successful/superseded recordings clean both
scratch copies; failed finalization retains recovery evidence. Unavailable
replica cleanup is retried by startup reconciliation.

Verification:

- **484 library + 62 integration tests pass** on final production source:
  nine new mirror/HLS boundary regressions, eleven existing paired-store units,
  thirty media, twenty-seven native-video and five takedown integrations.
  Includes incomplete/extended/invalid MP4 boxes, incremental append, interrupted
  tail exclusion, committed-prefix restoration, missing window members, prior
  playlist retention, busy writer/free floor/missing mount, paths/symlinks,
  recoverable/unknown-file reconciliation, stale/future replica rejection and
  replica-backed integrity hashes.
- **Two actual PeerTube 8.3.0 fresh-root recovery cases pass in 441.12 s**, zero
  failures/skips. Single-disk baseline still works. The paired case preserves
  four removed primary segments privately, verifies exact replica HTTP/hash
  bytes, and then loses the complete primary recording generation before
  database/root archiving. Fresh startup explicitly logs committed-replica
  restoration, refuses old generation/off-air tracker access, accepts original
  keys with compatible stable swarm identity, and produces two independent
  replays imported by PeerTube. Both video and audio decode with FFmpeg `-xerror`;
  exact ranges, primary-file replica failover, ledger refund and both-copy
  cleanup also pass. The quiescent backups/configuration escrow remain private.
- Real **320px Chromium JS/no-JS native decoders** advance another 45–46 frames
  over three seconds in each configuration, zero dropped frames/errors/overflow.
  JS requests two deliberately missing primary segment URLs successfully;
  no-JS uses the native progressive endpoint, not a script player. Screenshots
  and media responses are retained; both JS and no-JS screenshots were visually inspected.
  This is a targeted desktop engine/narrow-layout proof, not a seven-engine
  hardware/AT or latency/capacity sign-off.
- Strict workspace/all-target Clippy, SQLx metadata, formatting/syntax and
  rendered documentation/link checks pass. Initial inference, temporary-fixture
  lifetime and four Clippy diagnostics were fixed without suppressions or
  relaxed assertions; originals/diagnostic notes are retained.

Protected evidence:

- `target/evidence/ap09-m7-live-checkpoint-20260930/` (`rust-final.log`,
  `clippy-final.log`, `sqlx.log`, `peertube-initial.log`, `early-failures.txt`).
- `target/e2e/ap09-m7-live-checkpoint-20260930.xml`.
- Strengthened paired assertions additionally pass **1 selected case in
  147.57 s** (the already-passing single-disk baseline is explicitly deselected):
  `target/e2e/ap09-m7-live-checkpoint-20260930-final.xml` and
  `target/evidence/ap09-m7-live-checkpoint-20260930/peertube-final.log`.
  The test requires a real browser request for a deliberately missing segment,
  the no-JS progressive endpoint and no failed media responses; fixture:
  `target/e2e/video-live-restore/liverestoreaca59d39-jwd9lf1g/`.
- `target/e2e/video-live-restore/liverestore6a755e76-15mgitdu/` (single).
- `target/e2e/video-live-restore/liverestore26d0cebd-4m9pir3w/` (paired), including
  `result.json`, browser observations/screenshots, original fault media,
  replay MP4s, source/restored logs, database/media archives and separate escrow.
- Exact candidate SHA-256:
  `c8177b68b5e67bbf05aadc273d8e4288b0e82f7ecde4b29e3150ad4495d21b91`;
  built from `70f82c20` plus this recorded worktree diff.
  The identical executable is frozen at
  `target/evidence/ap09-m7-live-checkpoint-20260930/plamenu`.
  Standing local service refreshed once after an immediate zero-active-session
  check: PID `3068830`, `/proc/3068830/exe` has this exact SHA, `/ready` is 200.
  PeerTube reports 8.3.0. Owned fixture servers/encoders are stopped; free disk
  remains approximately 135 GiB. No source/media/archive/audit evidence was
  removed; only newly created empty mistaken test-output directories were removed.
  No push, merge or staging deployment occurred.

These are asynchronous checkpoints, **not guaranteed live RPO, seamless
publisher/disk failover, direct replica-backed no-JS rewind generation or M7
completion**. Busy durable writes can delay checkpoints. Derived rewind clips
remain primary scratch. Wider offload, federated/multi-node redundancy,
concurrent peer/browser delivery and remote-cache/failure matrices stay open.

### M7 decoded browser performance and signaling-outage fallback — 2026-09-30

The separate browser contract is defined in
[VIDEO_SCALE_VERIFICATION.md](VIDEO_SCALE_VERIFICATION.md) and implemented by
`accessibility/tests/video-playback-budget.spec.mjs`. Ordinary HTML forms publish
a real 24-second 640×480/10 fps H.264/AAC moving-pattern film. Each of seven
configurations measures watch playback and an explicit `p2p=0` server-only
embed: **fourteen decoder measurements**, not HTTP-body timing or fabricated
media counters. Application scripts remain disabled in the no-JS project.

All **13 browser tests pass with zero retries/skips** (seven budget journeys
plus six unknown-swarm rejection checks), in **3.2 minutes**:
`target/accessibility/ap09-m7-decoded-budget-20260930/automated-verified.json`.
The supported pinned browser/encoder image is
`sha256:4a8e6da4796a64a002e53507e3bcfe05f18747bd93cc5b405013890b69d16f11`.
Per-lane JSON, signaling records and 320px screenshots are retained under
`target/accessibility/ap09-m7-decoded-budget-20260930/verified-traces/`.

| Measurement, across all fourteen lanes | Observed result | Predefined bound |
| --- | --- | --- |
| Moving decoded/presented startup | max **2.077 seconds** | ≤10 seconds |
| Local seek to 16 seconds | max **258.6 ms** | ≤5 seconds |
| Frames during eight-second steady window | min **78** | ≥30 |
| Observed steady waiting | **0 ms** | ≤2 seconds total/individual |
| Native reported dropped frames | **0** | retained measurement, not a production guarantee |

Each of the six enhanced configurations records **four native-tracker client
connection attempts**, intercepted and closed by Playwright's context-level
routing installed before any navigation. This is controlled client fault
injection, **not a real wire outage**;
origin media fragments are delayed 1.5 seconds, initialization is immediate,
and all decoder budgets still pass. A constructed peer engine alone is **not**
accepted as evidence of a signaling attempt. Explicit server-only embeds remove
the media delay and make **no new tracker contact**. No-JS uses progressive MP4,
has no peer engine/socket and also decodes, seeks and advances frames.
The six enhanced cases retain actual `requestVideoFrameCallback` timestamps and
waiting/playing intervals. No-JS cannot run page callbacks, so externally sampled
native frame quality (total minus dropped), readiness and timeline provide
sampled upper-bound timings; three new frames and an advancing target timeline
verify seeking. Sampling gaps are capped at 250 ms. These methods are not
silently presented as equivalent event-exact rebuffer accounting.

The Chromium narrow screenshot was visually inspected: a real decoded frame at
16 seconds, quality/ALT controls, checked sharing choice and explicit address
disclosure fit 320 CSS pixels. No horizontal overflow is asserted in every lane.
This is decoder/reflow evidence, **not a new Axe/WCAG/real-device/AT attestation**.
Playback is muted; hardware audio and native-control keyboard UX are not claimed.
The short-film seek may already be buffered: do not call it a cold long-film
seek, WAN, live glass-to-glass, concurrent WebRTC, or production capacity result.

Initial failure evidence is retained. Page callbacks do not execute with JS
disabled, despite privileged observation/play evaluation working; the test now
uses external native-state sampling without enabling application scripts. Later
runs correctly failed their new positive signaling assertion: page-level routing
registered after the creator journey did not intercept those connections. A
separate real-browser route probe and context-level routing before navigation
resolved the harness issue; all six configurations now retain failed socket URLs.
No production permission, checksum, playback budget or test assertion was relaxed.

The local standing service was refreshed only after confirming **zero active
native sessions**. `/ready` returns 200; its running executable and protected
tracker candidate both match
`cbf5b3036dae3806314803b7a3ad92d3e7aecd7c83f521f50ef47fec3d30f288`.
PeerTube's actual config still reports **8.3.0**. Approximately **135 GiB** remains
free; owned fixture processes/temporary media are cleaned by their harnesses,
while source, backups and failure evidence are preserved. No push, merge or
staging deployment occurred. Original M7 offload, active-live/DVR/federated
redundancy and broader scale/fault gates remain open.

### M7 native tracker abuse and isolated fault recovery — 2026-09-30

Confirmed gaps: signal validation previously happened during recipient relay,
after registration/acknowledgment; an invalid later offer could therefore relay
earlier offers, and unused malformed offers were not checked. Database authority
lookups, failure writes and keepalive writes also lacked a deadline. Ordinary
outbound writes alone had a five-second limit; idle sockets could occupy global
admission indefinitely despite ping/pong activity.

Every offer/answer is now validated **before** indexed authority lookup,
registration, acknowledgment or relay. Combined VOD/on-air-live authority checks
fail closed after two seconds; every write path is bounded at five seconds.
Unannounced sockets expire at 15 seconds and successful announcements renew a
180-second deadline, accommodating the 120-second advertised interval. Binary
messages and oversized frames close. No new daemon, dependency, stale authority
cache, viewer identity persistence or optional installation step was introduced.

The existing 2,048/4/128 socket/swarm/peer ceilings, 16 offers, 64-KiB messages,
16-KiB signals, eight-message queues and ninety-announces/minute limit remain.
Five focused units cover complete-message validation, valid byte-range binary
IDs, event/offer limits, cross-swarm answer isolation, non-blocking full queues,
and exact production admission/reclamation at 2,048 guards. The latter is a unit
test, **not** a 2,048-connection network-flood benchmark.

The uniquely owned real-instance drill publishes two real native ladders and
subscribes actual PeerTube. Two recipients observe **no partial signal or ack**
from a malformed two-offer announce. Valid binary-ID offers/answers relay;
cross-swarm targets and identity conflicts are refused. Four-swarm, offer,
signal/frame and ninety-announcement limits pass. **128 simultaneously admitted
peers** refuse a 129th, then reclaim stopped/disconnected identities without
sleep/retry. An unannounced connection closes in **15.004 seconds**. An exclusive
lock of only the fixture rendition table fails authority in **2.008 seconds**,
then playback/signaling recover after releasing that lock. Current-owner
suspension refuses attached and new peer announcements before relay.

Finally, unmodified PeerTube and native browsers exchange the real film after
all faults: **78,669 bytes uploaded to PeerTube**, **204,780 SHA-256-validated
bytes received from it**, zero validation errors and **553 actual PeerTube
decoded frames** at 18.52 seconds. The existing slow/unseeded browser startup
also decodes in 8.688 seconds. Report:
`target/e2e/ap09-m7-tracker-20260930-final.xml` (**1 passed, no skips**, 113.32 s);
protected result/log/movie/browser evidence:
`target/e2e/video-tracker/trackerlimits86137fcb-ab5mck2z/`.
Exact executable SHA-256:
`cbf5b3036dae3806314803b7a3ad92d3e7aecd7c83f521f50ef47fec3d30f288`, retained
as `target/evidence/ap09-m7-tracker-20260930/plamenu`.

The non-fail-fast Rust run passes **475 library plus 62 integration tests**
(media 30, native video 27, takedown 5); strict workspace/all-target Clippy and
SQLx metadata checking pass. Current-source logs use
`target/evidence/ap09-m7-tracker-20260930-{rust-current,clippy-final,sqlx}.log`.
Earlier successful pre-final-layout tracker runs are retained, as are the initial
Rust harness errors (missing `DATABASE_URL` and one wrong test-target name) and
resolved strict-Clippy diagnostics; neither was papered over or counted as a
passing regression run.

Already exchanged public media and ICE signaling cannot be recalled. Each
announcement rechecks canonical authority; this is **not proactive zero-delay
revocation of every idle WebRTC connection**, slow-reader/full-network DoS,
multi-peer browser scale, remote-cache faults or completed M7. Original gates
remain open.

### M7 sustained mixed native origin delivery — 2026-09-30

The explicitly opt-in five-minute workload passes its **predefined** budgets,
documented in [VIDEO_SCALE_VERIFICATION.md](VIDEO_SCALE_VERIFICATION.md).
One real 640×360/25 fps H.264/AAC publisher serves **32 simultaneous progressive
live viewers plus 16 concurrent range clients**. Finite ranges use the native
file offered by actual PeerTube. Anonymous creator delivery accounting stays
enabled. The measured interval begins only after all live viewers join, lasts
**300.78 seconds**, and contains zero failed/incorrect range requests or early
live exits. It is not a short request burst extrapolated into sustained scale.

| Measurement | Result | Predefined ceiling/floor |
| --- | --- | --- |
| Exact 64-KiB ranges | 4,816 | ≥4,000; zero incorrect/failed |
| Local range-response p95 | 7.10 ms | ≤1 second |
| Slowest first live body | 2.80 seconds | ≤20 seconds; no HLS wait/retry in test |
| Longest ongoing live chunk gap | 2.05 seconds | ≤15 seconds |
| Shared remux children throughout | 1 | exactly 1, independent of viewers |
| Peak server/owned-media-child RSS | 393.83 MiB | ≤512 MiB |
| Peak server growth from prepared baseline | 104.83 MiB | ≤256 MiB |
| Last-quarter vs first-quarter mean server RSS | −1.76 MiB | ≤32 MiB growth |
| Mean server/owned-child CPU | 0.054 cores | ≤2 cores |
| Owned media-tree growth | 87.07 MiB | ≤2 GiB |
| All 32 attached consumers' EOF after suspension | ≤1.02 seconds | ≤10 seconds |

Delivered response bodies total **315,621,376 finite-range bytes** and
**1,401,899,482 live bytes**. The test discards streams instead of recording
large files. A real administrator suspension terminates every attached body;
new VOD ranges and progressive live requests are refused. One-second samples
include Linux children spawned by every Tokio task, not just the main task.
The external sample publisher is measured separately (0.052 cores, 59.74 MiB
peak RSS). Caddy, PostgreSQL and test clients are **not** included in service
CPU/RSS; these are local origin-body budgets, not full-machine or WAN capacity.

All four combined real-instance cases pass, with no skips, in **683.93 seconds**:
analytics/concurrent live startup, single-disk recovery, paired recovery and
sustained delivery. Report:
`target/e2e/ap09-m7-analytics-recovery-scale-20260930-final.xml`.
Protected measurements and samples:
`target/e2e/video-delivery-scale/deliveryscale17c3d260-_k7m12ac/result.json`.
Executable SHA-256:
`6d0c56738e537beec28596e99e9daf19875aec9576c98fe4fe1a05086d3403b2`.
The exact tested executable is retained under protected
`target/evidence/ap09-m7-20260930/plamenu` and refreshed into the local standing
service only after confirming zero active native sessions. `/ready` returns
200 and migration 0128 is applied. A read-only Chrome smoke follows existing
channel management to Delivery analytics and confirms collection remains off,
retention 30 and accessible ordinary forms; no standing preferences/posts were
changed. The running executable matches that SHA-256. No staging deployment
or remote push occurred.
Disk free space remains about **136 GiB**; backups and failure evidence are
preserved, with no material standing media/source/database deletion.

This is meaningful sustained native HTTP delivery/seek-range/resource evidence,
**not decoded browser startup/seek/rebuffer, live glass-to-glass latency,
P2P/browser concurrency, multiple broadcasts, offload or production capacity**.
Those original PERF01/PERF02 and wider M7 gates remain open; neither milestone's
criteria are narrowed to this workload.

### M7 interrupted live whole-store disaster rehearsal — 2026-09-30

Both **single-disk and paired-disk** disposable-instance cases pass. An actual
recurring RTMP encoder produces complete rolling HLS segments and fragmented
recording data before its owned server is crashed. The remaining writer is
quiesced; the database and entire primary tree are archived from that window,
plus the complete paired replica where enabled. Configuration/encryption-root
escrow is separate and protected. Original fixture directories are removed,
then exact manifests are restored into new directories and a fresh `template0`
database; this is not merely restarting against the old live directory.

The original key audit, credentials and Video UUID survive. Startup marks the
orphan interrupted, old generation HLS refuses bytes, and off-air tracker
announces fail. The original ingest key starts generation two. PeerTube 8.3
derives swarms from the stable Video UUID/resolution, not a recording session;
the compatible hash resumes only on air while old HLS paths remain refused.
Both the crashed recording and the later stopped recording become independent
replay Videos, fully decoded with video **and audio**, imported by surviving
PeerTube with exact 64-byte ranges. Successful recovery removes the crashed
scratch; exact durable-key cleanup acknowledges its original broadcast ledger.

Paired completed replays have identical copies. Removing only each fixture
primary movie key still serves PeerTube's exact range from the replica. Delete
removes both copies before releasing that key's charge. Active live/DVR scratch
is deliberately included in the whole-primary backup, **not claimed mirrored**.
The two roots in this test are logical directories, not independent hardware
failure domains or multi-node serving.

Evidence: the two live-restore cases in
`target/e2e/ap09-m7-analytics-recovery-scale-20260930-final.xml`; protected
`target/e2e/video-live-restore/liverestore120526f5-k5mbi3qz/result.json`
(single) and `liverestore650bedbe-4x0qpbe1/result.json` (paired), with archives,
separate escrow, logs, exact manifests and decodable replay samples.
Tested executable SHA-256:
`6d0c56738e537beec28596e99e9daf19875aec9576c98fe4fe1a05086d3403b2`.
These private backups must not be published.

Original failures are retained: the first test wrongly required a new swarm
hash on reconnect (contradicting the unmodified PeerTube player); the next
checked the independent replay-media ID rather than the actual original
broadcast's storage ledger and therefore did not wait for cleanup. Corrected
tests assert the real protocol and exact charged key; no product permission,
storage-acknowledgement or reconnect gate was relaxed. Wider remote-cache,
offload, P2P fault and production-disaster scope remains open. Operator
instructions are in [Backup and restore](operators/backup-and-restore.md).

### M7 private creator delivery analytics and immediate live startup — 2026-09-30

Current channel owners can opt into anonymous daily UTC origin-delivery
aggregates through **Manage video channels → Delivery analytics**, including
ordinary no-JS/keyboard forms. Owners and accepted editors can read; publishers
and former owners cannot. Collection defaults off. One-to-ninety-day retention,
confirmed erasure, immediate disable/delete hiding, current ownership and
moderation fences, operator disable and private cache policy are implemented.
There are no viewer identifiers, individual events or inferred unique views.
The numbers are emitted origin body bytes/responses, not decoded watches,
confirmed wire delivery or P2P estimates. See [VIDEO_ANALYTICS.md](VIDEO_ANALYTICS.md).

Streaming accounting never reads ahead. It bounds buckets and live permits at
4,096 each, batches SQL rather than writing per chunk, and applies short
eligibility and persistence deadlines. Erasure invalidates only that channel's
already-open counters; persisted policy generations also refuse stale batches
from another collector. Database failure drops a best-effort batch without
failing playback or uncertain-commit double counting. When no channel opts in,
there are no additional media-request eligibility queries.

All **seven desktop/narrow/no-JS browser projects** pass with no retries/skips:
native publication, discoverable navigation, keyboard opt-in, retention,
actual range-delivery totals, confirmed erase preserving collection, disable,
320px reflow/44px choices, screenshots and Axe where scripts are enabled.
Evidence: `target/accessibility/ap09-m7-analytics-20260930/automated-final.json`.
The current-source repeat also passes all seven in 3.5 minutes, with the
updated video-title label: `current-source.json` and `current-source-traces`
in the same directory. Its narrow-screen screenshot was visually inspected.
The first browser run's incorrect ambient-cookie API-authentication assumption
is retained; final tests use the existing explicit bearer contract, without
weakening authentication.

The actual PeerTube/encoder test passes in **189.37 seconds**:
PeerTube imports the native film and offers its origin range; opt-out records
nothing; opt-in records the exact 32 delivered bytes; the real native encoder's
progressive body contributes 35,577 bytes; private refusal, erase, disable and
continued playable ranges pass. Evidence:
`target/e2e/ap09-m7-analytics-20260930-native-ready-final.xml` and protected
`target/e2e/video-analytics/analytics45917fb0-j426_03z/manifest.json`.
Tested executable SHA-256:
`8293e986fec21ea3c719f6b5888ceaaf238655be8455355bcff5a7cce46a6ec7`.

The final current-source test additionally starts **four simultaneous first
viewers** without waiting for HLS: all receive initialization, exactly **one**
remux child is retained, and 142,308 live body bytes are accounted. Evidence:
the analytics case in `target/e2e/ap09-m7-analytics-recovery-scale-20260930-final.xml`
and `target/e2e/video-analytics/analytics38a52af7-cptnf27m/manifest.json`, using
SHA-256 `6d0c56738e537beec28596e99e9daf19875aec9576c98fe4fe1a05086d3403b2`.
The initial concurrency test inspected only Linux's main-task children;
the corrected inspection includes every Tokio task's child list, without
printing credential-bearing publisher arguments or changing product limits.

This test requests progressive playback immediately when ingest becomes
on-air, and reproduced a real 404: the master exists before the first atomic
child playlist/segment. FFmpeg's input HTTP reconnect options do not cure its
HLS-child failure. The fixed shared session waits for a complete safe native
segment within the existing twenty-second initialization deadline, rechecking
canonical authority and exact generation. Concurrent initial viewers retain
one pending session rather than replacing it before initialization. No viewer
delay/retry was inserted into the test and no encoder dependency was added.
Original startup failures and server diagnostics remain in the same evidence
family. This closes a concrete creator analytics slice and live-start defect,
not the wider M7 offload/redundancy/fault/sustained-workload gates.

The non-fail-fast Rust run passes **469 library and 75 integration cases**
(media, native Video, analytics, distribution and takedown). The final library
repeat passes **470 cases**, including exact safe-segment readiness and stale
session-exit fencing: a dying task cannot remove its newer replacement.
Logs: `target/evidence/ap09-m7-analytics-20260930-rust-native-ready.log` and
`ap09-m7-analytics-20260930-library-generation-final.log`. Final strict
workspace/all-target Clippy and SQLx metadata checks pass. The first strict
run's documentation-backtick warning is retained and corrected, not suppressed.

### M7 optional paired native disks — 2026-09-30

`[media_mirror]` is an opt-in second native disk, not a new mandatory service.
The default installation and one-to-four native worker choices remain unchanged.
Paired ownership markers, independent/non-nested roots and existing empty
replica admission prevent silently adopting unrelated files or missing mounts.
Startup seeds existing flat keys before serving, compares content rather than
just lengths, and can resume an interrupted seed. CLI account management does
not rescan the entire store.

Durable writes sync both copies; replica replacement is private and atomic.
One serialized byte budget covers copies and receiving upload chunks, with
both free-space floors and a second measurement after primary allocation.
Reads and range serving can fall back without buffering the entire movie.
Missing/short committed upload prefixes recover from the surviving disk;
uncommitted tails are overwritten on retry. Cleanup must acknowledge both disks
before quota is refunded. Authority checks still precede either copy.
Cancellation removes its temporary file; crash scavenging touches only exact
owned staging names older than 48 hours, never recent/unrelated files.

**Eleven storage units** pass, including copy/append budgets, seed repair,
missing markers/mounts, physical deletion failure/retry, missing/short prefixes,
traversal/symlink refusal and cancellation/stale-file cleanup. The complete
library passes **464 cases** before the final free-space remeasurement, with
all eleven storage units rerun after that change. Media/distribution/takedown
integrations pass **42 cases**, including actual paired-disk authority and
cleanup-debt retention. Strict all-target Clippy, SQLx metadata, formatting,
Python/JavaScript syntax and rendered documentation/link checks pass.

The final actual PeerTube/disposable-instance case passes **in 97.11 seconds**:
native ladders/captions/posters/storyboards/integrity hashes seed byte-identically;
removing primary files still serves every asset and the actual PeerTube 32-byte
file range; suspension refuses them anonymously and with author cookies;
unsuspension restores them; both receiving prefixes survive a process crash at
the acknowledged offset; publication Delete removes both copies and refunds
only its ledger, preserving the subscriber's unrelated avatar.
Evidence: `target/e2e/ap09-m7-native-mirror-20260930-ack-final.xml` and protected
`target/e2e/video-mirror/mirror7ac72244-w81mrans/manifest.json`.
The tested executable SHA-256 is
`8e0b4ea3736e68ba6c4170e136b012e5e73dcfa3684f95fc241fc3db86bdce3c`.
Earlier failures retain the test-only
session attribute/account-column mistakes and the incorrect expectation that
deleting one publication also deletes the subscriber's avatar. They are not
described as product fixes.

Scope is **durable flat keys**, including completed media and sources/upload
prefixes. Active live recordings, rolling live/DVR directories, multi-node
serving, remote-worker offload and federated redundancy still retain wider M7
scope; no full redundancy or disaster-recovery milestone is claimed. Logical
quota is charged once while physical durable storage is approximately doubled.
Replication is not backup. Operator controls and safe retention/recovery
boundaries are documented in [VIDEO_STORAGE.md](VIDEO_STORAGE.md).

### M7 already-connected live revocation — 2026-09-30

The shared fragmented-MP4 remux rechecks publication authority and on-air state
every five seconds, with a two-second database deadline. Revocation or an
unavailable authority read closes all attached response streams before queued
fragments are drained, and stops the shared child. Waiting viewers cannot
receive retained initialization after closure. The check is per broadcast,
not per viewer; the existing eight-remux and bounded-fragment ceilings remain.
This stops new delivery, not already buffered bytes in a viewer's player.

Actual RTMP, RTMPS and HTTPS-FLV key-rotation tests pass, including an already
connected progressive viewer reaching clean EOF within ten seconds, the
revoked publisher stopping, and a new key starting generation two with both
independent replays saved. The initially refused HTTPS test exposed the
rebooted local Caddy's address changing from the explicitly trusted address;
only that local /32 was corrected, without broadening production trust.
Evidence: the three passing transport cases in
`target/e2e/ap09-m7-live-termination-20260930-final.xml`.
The serving executable SHA-256 is
`24ff6a104038d5750506a64535b5400f4dc03445a76dfba9a75599dee545ad60`.

All **463 library tests** pass with the configured disposable SQLx test
database, including four live-stream units and Russian catalogue coverage.
The broad run caught three omitted existing preview/poster translations; these
are supplied rather than weakening catalogue completeness. Log:
`target/evidence/ap09-m7-library-20260930-final.log`.

PeerTube-origin live P2P's wider run also exposed an unreliable assumption that
two independently selected Auto renditions and live edges must seed one
another. Real quality controls now align a 360p rendition, and the receiving
viewer seeks into the seeder's existing window. Production latency and HTTP/
P2P deadlines are unchanged; verified received bytes and reverse uploads are
still required. That regression passes in
`target/e2e/ap09-m7-mirror-live-peers-20260930-final.xml`; the mirror case in
that same report initially fails on a test-only account-column name and is
not counted as passing storage evidence.

Desktop/narrow Chromium, Firefox and WebKit creator/capture controls and the
Chromium no-JS recurring encoder/replay journey pass **all seven cases, with
no retries/skips**, using a fresh verification account:
`target/accessibility/ap09-m7-live-termination-20260930/isolated-controls.json`.
The narrower capture/replay run passes both Chromium captures and the no-JS
encoder; its four deliberately unsupported projects are not counted as
coverage. The first broader run records five passes and two WebKit process
crashes while navigating the accumulated developer home feed, before live
authoring. The fresh-account run does not establish the cause of that crash or
claim it fixed. Its report is retained as `full-controls.json` in the same folder.
An isolated repeat reproduces the accumulated-feed crash and retains its trace
in `developer-feed-repro-traces`. Read-only rendering isolation also reproduces
it with JavaScript disabled and Playwright-routed media requests aborted.
That interception does not establish that native GStreamer requests were
blocked. Removing video sources makes the host no-JS case render.

The subsequent diagnosis finds **missing host GStreamer `autoaudiosink`**:
WebKit stderr reports the missing element and null-instance GLib failures.
The valid native MP4 decodes independently. In the project's supported pinned
browser image, the same accumulated feed passes real decoded playback at 320px
**with and without JavaScript**, without a process retry, overflow, changes to
posts/preferences or production source suppression. Aggregate evidence:
`webkit-feed-supported.json` in the same folder; image ID
`4a8e6da4796a64a002e53507e3bcfe05f18747bd93cc5b405013890b69d16f11`.
Explicit Linux host-mode tests now preflight the audio sink before creating
fixtures (four preflight regressions). No production runtime dependency or
host package installation is introduced. The original host failures/traces
remain evidence of the unsupported test environment, not a product fix.

Strict all-target Clippy and the 42 media/distribution/takedown integrations
pass, including mirror failover keeping native authority and a failed replica
deletion retaining its cleanup job until repair. These storage tests are not a
claim that active live/DVR scratch is replicated.

These are operations sub-gates, not a complete M7 sign-off. Wider storage,
analytics, backup/fault and sustained-workload gates below remain open.

### M7 origin revocation and acknowledged cleanup — 2026-09-29

`public_native_videos` centralizes the canonical status, original author,
uploader, channel and current-owner checks for native MP4/HLS/captions,
integrity metadata, live/DVR session metadata and tracker allowlists. Ownership
handoff preserves signatures without leaving playback governed by the former
owner. Private/tombstoned statuses and suspended/deleted actors fail closed;
unsuspension restores retained files. Remote Video audience/channel moderation
also participates in cached/proxied playback. Ordinary Note attachment audience
semantics are unchanged. Error responses carry `private, no-store`.

Deleting a key now revokes it at the origin in the same committed cleanup
queue state, before any physical deletion. Failed jobs are never discarded:
after ten attempts, a burst backs off for a day while retaining revocation and
storage debt. Repairing the store lets that same key complete; quota refunds
still require acknowledged removal. Four new regressions cover failed physical
deletion, exhausted/repaired storage, current-owner suspension after transfer,
private/tombstoned VOD and live/DVR/tracker authority. They reproduce the three
original gaps before the fix and pass after it. Initial fixture mistakes (an
invalid synthetic JPEG and omitted positive DVR limit) are retained separately
from those reproduced defects.

The media/native Video/distribution/live-metadata/status-cleanup regressions
pass **74 cases**. HLS/report/admin-report/new-takedown regressions pass
**44 cases**, and database cleanup passes **4 cases**. Strict all-target
Clippy, SQLx metadata and formatting pass. The wider run additionally exposed
the first-party live fallback hiding a declared remote poster before caching;
declared thumbnails now use their guarded proxy, with a built-in poster only
when no thumbnail exists. All three live-state serialization cases pass.

Actual PeerTube 8.3: **3 passed in 116.79 seconds**, including native processing
and report review/resolution, acknowledged owner handoff, administrative
suspension refusing every native origin lane with and without author cookies,
the actual PeerTube file offer refusing ranges, reversible exact-byte recovery,
acknowledged original-author/current-owner Delete, distribution regression and
PeerTube-origin live byte exchange in both first-party players. Evidence:
`target/e2e/ap09-m7-takedown-20260929-initial.xml`.
All seven desktop/narrow/no-JS browser projects pass decoded standalone and
independent-origin embedded playback, private refusal, downloads/ranges,
keyboard/manual clipboard, 320px reflow and Axe:
`target/accessibility/ap09-m7-takedown-20260929/automated.json`.
Frozen serving executable SHA-256:
`fe9feff77abf7c8b6cdb9e88f05f12bd48b57fed40d6c81418d456c68226ce6a`.

This closes the reproduced origin/cleanup defects, not M7. Continuing-stream
termination (subsequently verified above), wider remote-cache/takedown faults, storage/offload/redundancy,
creator analytics and sustained resource/concurrency budgets remain open.
Previously fetched public bytes and independent caches cannot be recalled.

### M6 original-gate sign-off — 2026-09-29

The original creator/viewer milestone is verified. This includes discovery,
native and real PeerTube channel libraries/subscriptions/search, drafts and
recovery, VOD/live scheduling and calendar reminders, the previously verified
in-session camera/screen switching, settings and guarded imports/source sync,
stable-identity replacement/reprocess/trim, private history, embeds/downloads,
and desktop/narrow Chromium/Firefox/WebKit plus native no-JS journeys.

Catalogue requests now bound each filtered scan to four 1,000-candidate windows
and retain an Older cursor across entirely hidden windows. Current channel
ownership participates in visibility after transfer. Three server integrations
cover 5,550 matches, ownership/suspension, anonymous policy and independent
preview authority; four database discovery regressions pass. The native Video
(27), scheduled-status (15), distribution (7) and live-metadata (3) integration
regressions also pass. A large filtered HTML request, including rendering, took
about 860 ms on this development host; this is not the sustained M7 workload.

The six-case actual PeerTube 8.3 matrix passes in 96.50 seconds: fresh and
backfilled channel attribution, authenticated global/scoped/followed searches,
block/unblock, once-only due publication, native HTML reschedule/cancel,
collaborative ownership lifecycle, and distribution Create/Update/Delete.
Evidence: `target/e2e/ap09-m6-closure-20260929-final.xml`.

The complete browser run finishes all 76 cases without retries/skips. Seventy
pass, including every upload/audio-only/metadata/recovery/scheduling/channel/
calendar journey. Six catalogue cases expose an incorrect test assumption that
the ordinary pager appends rather than navigating. The corrected seven-project
catalogue run passes all seven cases: automatic prefetch, keyboard HTML paging,
all 50 unique visible identities behind 5,500 hidden rows, no title leaks,
320px reflow, screenshots and Axe. Retained reports:
`target/accessibility/ap09-m6-closure-20260929/final/automated.json` and
`target/accessibility/ap09-m6-closure-20260929/catalogue-final/automated.json`.
The initial run also retains the ambiguous transcript selector and off-screen
WebKit autoplay failures; final journeys scope the intended disclosure and
bring the preview into view before playback. No product guard was weakened.

The frozen serving executable SHA-256 is
`30a3a7ce3d93688e5b5aeeecb22a41135240bca8cdf7a1d6f691fafd4917f309`.
Strict all-target Clippy, SQLx metadata, formatting, JavaScript/Python syntax,
rendered documentation and local documentation links pass. The standing
first-party viewer and sharing page were visually inspected; existing media and
preferences were not changed. User workflows are documented in
[`VIDEO_PUBLISHING.md`](VIDEO_PUBLISHING.md),
[`VIDEO_VIEWING.md`](VIDEO_VIEWING.md) and
[`VIDEO_IMPORTS.md`](VIDEO_IMPORTS.md).

This closes M6's stated functional gate. Real device/assistive-technology
release checks retain M9 scope; sustained capacity and wider operations/fault
matrices retain M7 scope. Neither milestone's acceptance criteria were removed.

### Creator download/embed controls — 2026-09-29

Videos → Download and embed settings provides ordinary HTML controls before
publication and on existing Videos. Downloads and first-party frames may be
disabled independently without breaking ordinary playback. These flags control
offers, not DRM or recall of already fetched public bytes. Live broadcasts do
not offer a finite download; independent replay episodes inherit the initial
choice once. Creator API mutations require media scope, and published changes
also require status scope, current channel authority and transactional outbox
delivery by the immutable original signer.

Viewers receive a range-capable MP4 attachment and manually selectable embed
code, including server-only `p2p=0` code. No-JS needs neither clipboard access
nor a script for playback. Enhanced clipboard failures preserve manual selection
and announce their outcome. Default frame dimensions fit a 320px host. Private,
deleted, suspended and disabled frames/downloads are refused even with cookies;
denials and private management responses are not shared-cacheable.

Distribution/replay integration: **10 passed**, plus the finite-download live
lifecycle unit. Strict all-target Clippy and SQLx metadata checks pass. Actual
PeerTube first Create and later enable/disable Updates, stable UUID, decoded
playback, captions/HLS, exact MP4/ranges, cookie-bound frame refusal and signed
Delete: **1 passed in 28.69 seconds**. Evidence:
`target/e2e/ap09-distribution-20260929-initial.xml`.

Seven canonical desktop/narrow Chromium/Firefox/WebKit and Chromium no-JS
projects pass, with zero retries/skips:
`target/accessibility/ap09-distribution-20260929/local-host-final/automated.json`.
Each includes actual decoded playback in copied frame code served from an
independent local HTTP origin, native upload/publication, disable/re-enable,
32-byte ranges, keyboard/manual copy, clipboard success/denial, reflow and Axe
where scripts are enabled. Screenshots include `copied-iframe-decoded-320px.png`.
Candidate executable SHA-256:
`f2e849a5810c261857f5524f1630245e817a9fbf5272980b0da17655c9230fce`.

Earlier failures are retained: one Firefox subpixel measurement of an actual
44px target; a synthetic first-party host blocked by the intended frame CSP;
and a synthetic public-origin host blocked by Chromium local-network protection.
The final harness uses a real independent localhost origin. Product CSP and
browser network security are unchanged. This closes the download/embed gate,
not the remaining M6/M7 gates or full PeerTube REST compatibility.

### Scheduled native source synchronization — 2026-09-29

Videos → Scheduled video sources supplies ordinary HTML channel selection,
explicit periodic-copy consent, status, pause/resume/check/remove and private
review navigation. One supervised native discovery worker consumes bounded
public ActivityPub channel outboxes or Playlists; no companion downloader,
automatic publication, follows or source-identity impersonation is introduced.
PeerTube element IRIs are dereferenced once; complete Videos are deduplicated,
not guessed playlist clips. Page-and-entry cursors advance large/duplicate-heavy
feeds. Metadata is bounded to four collection documents, twenty candidates,
forty-eight requests, eight MiB and forty-five seconds per check.

Allocation, full source-ceiling quota reservation, metadata, download registration
and deduplication commit together. Pause/revocation/suspension/lease generations
fence allocation; network lease renewal never suspends a database transaction.
Copies already queued remain independently cancellable private drafts. Five
sources/account and 10,000 remembered identities/source are hard bounds; history
survives cancelling a copy and pauses at its cap instead of silently forgetting.
Status JSON discloses no source URL, CSRF token, identity or generation.

Candidate source/native-publication/native-worker integration regression:
**63 passed** (10 + 27 + 26); one-time importer regression **5 passed**.
Strict workspace/all-target Clippy, SQLx metadata, formatting/syntax and rendered
documentation/link checks pass. The standing browser confirms discovery,
native controls and consistent styling without modifying existing media or
preferences. Frozen candidate SHA-256:
`8e6ba1ad783bb230bea7f1f2ec4de7b8c5813e87122c4575d19e2ea4ecb29043`.

Actual PeerTube channel/playlist E2E: **2 passed in 279.68 seconds**, including
new source Videos after pause/resume, exact fragmented MP4 bytes, independently
encoded private previews/ranges, repeated-check deduplication, no automatic post,
confirmed source removal preserving copies, explicit new-identity publication
and acknowledged signed Delete. Evidence:
`target/e2e/ap09-source-sync-20260929-final.xml`.

The pinned seven-project desktop/narrow Chromium/Firefox/WebKit plus Chromium
no-JS matrix passes every journey with no retries/skips: native consent and
filled validation, pause/resume/remove, injected status-503 failure/manual
refresh, actual decoded private previews, cancellation, 320px reflow, Axe and
forced-colour/reduced-motion checks. Evidence:
`target/accessibility/ap09-source-sync-20260929/final/`.

This closes the scheduled ActivityPub-source slice, **not M6/M7 or arbitrary
website import parity**. Retained first failures:
an immediate post-restart preflight saw 502 before readiness; real PeerTube
licence identifiers lack a verified versioned URI, so the test must not expect
a silently invented 4.0 grant; PeerTube channel deletion requires its name/UUID,
not its numeric REST creation ID. Only the two empty dedicated source fixture
channels were removed after resolving those exact targets. Source/security rules
were not weakened. User guide: [`VIDEO_IMPORTS.md`](VIDEO_IMPORTS.md).

### Private viewer history — 2026-09-29

Seven Rust integration regressions cover default-off/local-only storage,
author/channel/owner authorization, duration/live/draft rejection, 1,000-entry
bound, keyset pages, expiry, media cascade, concurrent opt-out, rollback,
OAuth scopes, private cache policy, CSRF and confirmed HTML clearing.

Real native/PeerTube history E2E: **2 passed**; metadata Update preserves the
position and current title/media identity, native range delivery works, and
Delete removes history visibility. Evidence:
`target/e2e/ap09-video-history-20260929.xml`.

Seven pinned Chromium/Firefox/WebKit desktop/narrow plus Chromium no-JS browser
journeys pass actual decoded resume, HTML authoring/manual positions/forget/
disable, keyboard navigation, 320px reflow and Axe/reduced-motion/forced-colour
checks. Evidence: `target/accessibility/ap09-history-20260929/`.
Slow/failed saving regression coverage includes manual retry, one bounded queued
manual correction and automatic-pause saving, rather than recording page views.

Initial Rust fixture lacked a required publication poster and was corrected;
two accidentally overlapping runs of this same test binary collided in SQLx's
test-database setup. A subsequent single full run passed **7/7**. No production
validation or permissions were weakened. User documentation:
[`VIDEO_VIEWING.md`](VIDEO_VIEWING.md).

This closes the history/resume slice, **not either milestone**. The remaining
M6/M7 gates above are unchanged.

### Durable P2P preferences and bounded VOD recovery — 2026-09-29

HTML-first account settings now persist the default-on P2P choice across
signed-in browsers and read-only public embeds. Enhanced player saves are
serialized/coalesced, report failure and preserve unrelated history settings.
An explicit account choice outranks stale browser storage; an unconfigured
schema default does **not** silently reset a legacy browser opt-out. Failed
preference reads fail closed. Anonymous embeds can force server-only with
`?p2p=0` independently of third-party cookies. Cookie preferences never widen
content authority or expose identity, CSRF tokens or private history in frames.

Eight Rust integrations pass API scopes, partial updates, rollback, CSRF,
cache headers, anonymous/private embed authority and explicit preference intent.
Ten shipped-JavaScript checks pass default/legacy choice precedence and bounded
VOD recovery. The latter stops the peer engine and switches once to the
standard MP4 after a fatal adaptive error, fifteen-second failed startup or a
five-second buffered zero-frame decoder stall. Late first bytes get a decoder
grace period; paused/hidden/detached players and live recovery remain separate.

Seven browser projects pass actual decoded first-party/embed playback,
fresh-browser persistence, cross-origin cookie-free server-only framing,
manual/queued/failed saves, ordinary HTML/no-JS workflows, 320px reflow, Axe,
keyboard, reduced-motion/forced-colour and injected HLS-503 recovery. Evidence
is retained under `target/accessibility/ap09-p2p-preferences-20260929/`.
Two consecutive complete matrices pass, including the final frozen source in
`final/`; neither run used test retries.
Earlier failures are retained: malformed fragment-bearing embed test URLs,
synthetic-host loopback permission denial, native WebKit Space-key mismatch and
a real buffered-zero-frame MSE stall. The stall led to bounded standard-player
recovery; no privacy or frame security rule was relaxed to obtain a pass.

The actual PeerTube 8.3 regression retains default-on byte exchange and decoded
desktop/mobile seek: **455,497 bytes to PeerTube, 204,780 validated bytes from
PeerTube, zero checksum errors**. History/native-publication E2E: **3 passed**;
`target/e2e/ap09-p2p-preferences-20260929.xml`. The final frozen-candidate run
is recorded separately. User/API contract: [`VIDEO_VIEWING.md`](VIDEO_VIEWING.md).

Final frozen-candidate combined history/native-publication/restore E2E:
**4 passed in 176.26 seconds**;
`target/e2e/ap09-p2p-recovery-and-restore-20260929-final.xml`. Candidate SHA-256:
`c832cb2c5881299f8827bb75471e4cf2cdf0dcc466731a7dad7b54b315e3b8ec`.
Strict workspace/all-target Clippy, SQLx metadata check, formatter, syntax checks
and the rendered documentation build pass. The first-party settings page was
also inspected in the standing browser at 320px; temporary viewport override
was reset, and the existing developer account's preferences were not changed.

This verifies the settings, VOD/embed persistence and VOD startup-fallback
slices. Wider live/embed, tracker abuse/concurrency and release/device/AT
matrices remain open; **M6 and M7 are not signed off**.

### Native video backup and actual restore — 2026-09-29

The disposable-instance drill stops its writer, captures a PostgreSQL dump and
the entire media tree, separately escrows the matching configuration/encryption
root and restores into fresh `template0` database/media storage. It verifies
every file before startup, stable Video/media identity, every native HLS
rendition and integrity manifest, captions/posters/storyboard, exact MP4 ranges,
private-source denial, old password/OAuth/cookie credentials, private history/
P2P preferences and clipped playlists. A committed partial upload resumes,
finishes native processing and refunds reservations only after acknowledged
cleanup. Wrong-secret audit fails; the restored original keys deliver an Update
accepted by a surviving PeerTube subscriber without changing its remote UUID.

The strengthened run passes in **150.31 seconds**, including build/cache wait;
`target/e2e/ap09-video-restore-20260929-final.xml`. Protected recovery sets,
candidate/archive SHA-256 manifests and logs are retained in
`target/e2e/video-restore/`. They contain private data and federation secrets:
**do not publish the backup or escrow**. First failure found the local seeded
`template1` was not empty; fixture creation/restoration now uses `template0`
without changing or deleting that template. Native VOD/upload recovery is
verified; live-session/cache/offload and production disaster matrices remain
separate open work. Operator procedure:
[`backup-and-restore.md`](operators/backup-and-restore.md).

### Guarded native source imports — 2026-09-29

Videos → Import a video offers ordinary HTML URL authoring into a private,
quota-reserved native draft. It copies a finite public file or public recorded
ActivityPub Video, including PeerTube 8.3's fragmented MP4 links inside HLS
metadata. No companion downloader, crawling, scripts, credentials, playlists,
live imports, automatic publication or identity impersonation is introduced.
Processing/review/captions/chapters/posters/scheduling reuse the native pipeline.

The importer streams to a unique temporary file, reserves the remote ceiling
rather than the tiny HTML form's length, preserves DNS/redirect/byte/stall guards
and uses a separate one-hour finite-import deadline without widening ordinary
federation/cache timeouts. One in-process source download runs at a time. Durable
two-minute leases/generations fence restart/retry and the short storage commit;
five-second renewal checks stop cancelled/revoked/suspended work. Failed downloads
require explicit retry; completed downloads discard their saved source URL.
Originals stay private and cancellation cannot refund unacknowledged deletion.

Five Rust integrations and one URL/public-object unit check pass: quota bounds,
private/cache/CSRF/rights/ownership/OAuth gates, exact source bytes, no automatic post,
failure/retry, crash reclaim, stale claims, current-owner suspension, collaborator
revocation and actual in-flight controller cancellation/scratch cleanup.

Real PeerTube direct-file and ActivityPub imports both pass native processing,
exact source integrity, private preview/ranges, caption authoring and separate
new-identity publication received by PeerTube with category/licence/captions,
P2P metadata and acknowledged signed Delete: **2 passed in 42.15 seconds**;
`target/e2e/ap09-source-import-20260929.xml`.

All seven Chromium/Firefox/WebKit desktop/narrow plus Chromium no-JS projects
pass actual native playback, keyboard consent/validation/retry, metadata
retention, private review and confirmed cancellation, 320px reflow and Axe:
**7 passed in 59.3 seconds**. Evidence:
`target/accessibility/ap09-source-import-20260929/final/`. The standing browser
form was inspected at 320px; viewport override was reset. PeerTube source tests
use an explicitly isolated local-peer transport opt-in; production SSRF defaults
are unchanged. Earlier failures are retained: an exact-label test selector,
over-fast no-JS refreshes followed by waiting on an intentionally static page,
metadata entity-shape/catalogue test assumptions and a readiness preflight race.
The repeated wrong-catalogue E2E was interrupted with normal fixture cleanup;
no content authority, timeouts or test retry count was relaxed to obtain a pass.

The final authority fence requires `write:media` even when an OAuth token is
transported as a cookie; read-only and unrelated granular tokens cannot allocate
quota or make origin requests. The strengthened five-test integration run passes
in 5.99 seconds. Final frozen-candidate real PeerTube E2E: **2 passed in 39.20
seconds**; `target/e2e/ap09-source-import-20260929-final.xml`. A second complete
seven-project matrix passes in **58.6 seconds** with no retries; final evidence:
`target/accessibility/ap09-source-import-20260929/final-scope-fence/`.
Strict workspace/all-target Clippy passes.

Final frozen native binary SHA-256:
`365cb677136b2b1ba390bfdc28127f9d148f2d6d4bb8bc445df7e1caa5a581c9`.
User guide: [`VIDEO_IMPORTS.md`](VIDEO_IMPORTS.md). Scheduled source sync and
stable-identity media replacement/reprocess/trim remain open. This is the
one-time import slice, **not M6 or M7 completion**.

### Stable-identity native film replacement — 2026-09-29

Videos and the metadata manager expose ordinary HTML private-candidate review
and explicit application. Upload/import, processing, native preview, visible
descriptions/transcript and timed captions work without scripts; existing
resumable upload and status enhancements remain available. The old film serves
until application, which retains the media/status IDs, UUID, original author,
channel, public title/description/audience/metadata and all social references.
Candidate film/poster/alternatives/caption languages/chapters replace the old
content. Current channel authority, candidate readiness/ownership, quotas and
the reviewed content revision are rechecked under the adoption fences. There
is no encoding, movie copy or network request inside that transaction.

Superseded files/raw source stay charged until acknowledged cleanup. Adopted
keys and their journal/reservations are moved, not deleted/recreated. HLS and
checksum URLs carry content revisions; pinned old requests cannot obtain new
metadata. Existing buffered bytes cannot be erased from another viewer's
device. Private history hides positions for superseded content and rejects a
stale player's save; metadata-only edits do not reset positions. API and HTML
application require both write scopes; scoped OAuth cookies cannot bypass them.

Five Rust replacement integrations and eight history integrations pass
atomicity, forced outbox rollback, concurrent/stale/cancelled applications,
editor/publisher grants, suspension, immutable storage debtor, quota refusal,
caption-language transfer, cleanup acknowledgement, private cache/CSRF/scopes,
revisioned HLS and history. A genuine-thumbnail regression covers drafts that
have no compatibility JPEG before replacement. Shipped-JavaScript units:
**13 passed**, including equal-length outdated fragments failing the actual
checksum validator, missing keys and incorrect byte ranges.

Final frozen-candidate real PeerTube 8.3 E2E: **1 passed in 117.89 seconds**;
`target/e2e/ap09-video-edit-20260929-final.xml`. Update retains the remote UUID
and URL, changes duration/captions/film/integrity metadata, and preserves local
replies/favourites/bookmarks. An actual old first-party seeder offers 124,295
bytes but its incompatible size declaration is refused before payload
acceptance; the receiver decodes the new blue frame with zero old validated
bytes. PeerTube's loader exposes this as `peer-closed` with a nested
`bytes-length-mismatch` cause. Equal-length corruption is covered separately
by the shipped-validator units, not claimed as an observed real-peer payload.
The two current products then exchange **332,263 bytes to PeerTube** and
**104,620 checksum-validated bytes from PeerTube**, with zero integrity errors.
Delayed, unseeded first-party Auto startup also decodes in 8,700 ms.

Earlier failures are preserved, not counted as passing evidence: short fixtures
never requested peer fragments beyond the 15-second urgent-HTTP window; the
loader's nested error cause was initially missed; a real AVIF-only Update was
silently filtered by PeerTube. Confirmed replacement now prepares the reserved,
padded JPEG before adoption, retaining the first-party natural-aspect AVIF.
PeerTube's actual installed validator accepts the corrected public Video.
A playback test now selects the seeded quality before awaiting decode under
its deliberately slow origin, and leaves initialization undelayed. A mistakenly
selected local random-port browser fixture was interrupted and normally cleaned
up: its canonical media URLs targeted port 443. The canonical container fixture
is used for the final matrix. Narrow WebKit also exposed a same-URL form-submit
test race: the waiter must observe the POST acknowledgement before navigating,
not conclude that an already-matching URL means submission has finished.

Strict workspace/all-target Clippy, SQLx metadata check, formatter, JavaScript/
Python syntax and rendered docs build pass. Frozen native binary SHA-256:
`3030db3bf409e76d815f83e7cfd5087ca0fb26268a6c62f13ffde0789bd2f014`.
Final canonical-container browser matrix: **7 passed in 1.2 minutes**, without
retries, across Chromium/Firefox/WebKit desktop/narrow and Chromium no-JS.
Actual private/new-film decoding, caption attachment, visible descriptions and
transcript, keyboard/native-required confirmation, unchanged permalink/title,
consumed-draft denial, 320px reflow and Axe pass. Screenshots/report/log:
`target/accessibility/ap09-video-edit-20260929/final/`. The standing browser's
published-list link and empty-candidate guidance were inspected read-only;
existing developer media/preferences were not changed.
User/API guide: [`VIDEO_EDITING.md`](VIDEO_EDITING.md). Reprocess/trim and the
other original M6/M7 gates remain open; **neither milestone is signed off**.

### Retained-source reprocessing and exact trim — 2026-09-29

Published native films now offer ordinary HTML reprocess/trim preparation and
private alternative-text review; API preparation/status/retry/cancellation uses
the same durable receiving/processing lane. An independent quota-charged source
copy and output set leave the published film available. Current channel access,
source revision, candidate provenance and confirmation fence adoption. A later
reprocess retains its previous trim window; a later trim can only narrow it.
Reprocessing preserves viewing positions while refusing stale-player writes;
trimming hides old positions. Captions in every retained language and chapters
are intersected/rebased; malformed timing fails closed. Untimed alternatives
are cleared for trim review, not presented as descriptions of different footage.
Both supported interface languages describe these consequences explicitly.

Exact frame/audio decoder regressions prove an inside-long-GOP cut excludes
preceding/following footage and selects the intended tone. Forced reprocessing
actually encodes rather than remuxing otherwise-copyable H.264. Newly encoded
edits force four-second keyframes so their copy-top adaptive stream no longer
contains the encoder's default 25-second segments. A controller cancellation
test covers a blocked source read and scratch removal. A real lease/lock stall
found during development was fixed by completing DB preflight before entering
the download/renewal select loop; no publication lock spans a movie copy.

Focused Rust integration suite: **48 passed**, plus **4** timeline/parser units.
Shipped JavaScript preference/recovery/integrity checks: **13 passed**. Strict
workspace/all-target Clippy, SQLx metadata check, formatting and syntax pass.
Canonical browser matrix: **14 passed in 3.9 minutes**, without retries, across
Chromium/Firefox/WebKit desktop/narrow and Chromium no-JS. Actual private/public
decoding, both edit operations, saved alternatives, captions, required keyboard
confirmation, stable identity, 320px reflow and Axe pass. Protected evidence:
`target/accessibility/ap09-video-source-edit-20260929/localized-final/`.

Real PeerTube 8.3 combined replacement/reprocess/trim E2E: **2 passed in 204.84
seconds**; `target/e2e/ap09-video-edit-and-source-edit-20260929-bounded-final.xml`.
Reprocess/trim Updates retain the remote UUID, title and URL, change the duration
and both caption languages, and keep the old film private until application.
The trimmed film exchanges **216,492 bytes to PeerTube** and **509,839 validated
bytes from PeerTube**, with zero integrity errors. The replacement independently
rejects the running old seeder and exchanges 104,620 bytes in each direction.
Unseeded first-party Auto playback survives four-second origin delay in 8,679 ms.
Frozen serving/CLI candidate SHA-256:
`0923418646340212a97d35b7977151a57048ab15876e6590d9cdaf430505e93f`.
An additional final run with actual PeerTube decoded-pixel assertions passes
**2/2 in 203.14 seconds**;
`target/e2e/ap09-video-edit-and-source-edit-20260929-pixel-final.xml`.
PeerTube decodes the new replacement's blue pixel `[0,14,253,255]` and the
trimmed-only blue pixel `[7,16,255,255]` after 18.5 seconds at normal speed
(187/183 decoded frames respectively); both retain positive bidirectional
segment byte counts and zero checksum errors. This is decoded imagery in the
peer's own player, not just correct metadata or a successful segment request.
The standing first-party editing disclosure and styled labels were also
inspected read-only in the browser; no existing film or preference was changed.

Failures remain evidence, not passes. Early browser checks incorrectly equated
the native MP4 duration with its existing B-frame-offset MediaSource timeline;
native duration and decoded-frame cut checks stay strict. Prefilled implicit
textarea labels obscured accessible control names and were changed to explicit
labels. Four-second artificial HTTP delay on PeerTube's stock three-second
no-first-byte deadline produced a traced 20-second rebuffer gap, not a codec
failure: playback then resumed at normal speed with decoded frames. Baseline
cross-product exchange now stays below that deadline (1.5 seconds), preserving
real bidirectional byte checks; first-party >3-second recovery is tested
separately. The broader slow-origin PeerTube fault matrix is still M7/M9 work.
Logs/failed XML are retained under `target/e2e/`.

This closes the original editing workflow gate (VOD08), not M6 or M7. Source
synchronization, the remaining creator journeys and operations/scale gates above
remain open. User/API guide: [`VIDEO_EDITING.md`](VIDEO_EDITING.md).

### Initial live labels and independently labelled recordings — 2026-09-29

The ordinary live-creation form now offers category, explicit licence,
content warning and sensitivity. Invalid schedules/labels return a filled HTML
form with an alert; title/description/channel/audience and reusable/playback
choices are preserved. Validation occurs before allocating a broadcast/key.
The initial signed Create carries the same terms and warning as dereference,
client entities and later on-air/replay updates. Unspecified grants no reuse
rights. One-shot recordings retain their original labels; recurring recordings
snapshot terms when privately prepared and preserve warnings through retry,
without merging independent session/publication identities.

Cookie-backed OAuth scopes remain granular: creating requires both media and
status writes, rotating/uploading requires media writes, and changing schedule/
playback announcements requires status writes. Current channel authority,
CSRF and browser WebSocket origin checks remain required. The stronger private
live-page `no-referrer` policy survives global response headers; ordinary pages
keep their original policy. Filled textarea labels have explicit associations.
Error-text contrast now passes both light and dark themes without changing
danger-button or decoration colours.

Focused Rust verification: **40 integrations** (initial labels, durable
recurring snapshot/retry/publication, native live/video, scope/CSRF, actual
WebSocket upgrade and security-header regressions), plus the DB recurring
session test. All **seven** Chromium/Firefox/WebKit desktop/narrow and Chromium
no-JS metadata/validation journeys pass keyboard, 320px reflow and complete
light/dark reduced-motion Axe checks where scripts are enabled. Evidence:
`target/accessibility/ap09-live-metadata-20260929/final/`. The standing
first-party form was inspected read-only; no existing broadcast was changed.

Earlier failures remain protected evidence: a real low-contrast error panel,
WebKit descendant styles not yet recalculated after a theme switch, an outdated
test assertion omitting calendar fields, and a changed disposable proxy address
rejected by exact-host trust. Production trust was not widened. The canonical
browser fixture separately exposes native ingest on its isolated host-side
address; encoder tests do not confuse the TLS container's localhost with the
host's localhost. Broader authoring/device/AT and operations gates remain open.

Final standing-candidate transport matrix: **6 passed in 391.94 seconds**.
Real RTMP, TLS-certificate-verified RTMPS and HTTPS-FLV each pass one-shot
and interrupted/reconnected recurring sessions. Both independent recordings
retain their exact category/licence/warnings and playable/range-served MP4s;
PeerTube 8.3 receives on-air/replay transitions and the same labels. Evidence:
`target/e2e/ap09-live-metadata-and-replays-20260929-final.xml`; original failed
run is retained separately. Frozen binary SHA-256:
`67fca200a9dd39557570d5a8976306c69a7528eeab324e6706f98bbd1486c3a4`.
Workspace/all-target strict Clippy, SQLx metadata check, formatter and syntax
checks pass. Documentation renders and passes the built-link check; previously
linked channel/playlist and closure pages are now included in the book.

Final combined canonical browser run: **16 passed, four Chromium-only
recording cases skipped in Firefox/WebKit**, no retries, in 1.1 minutes. All seven
metadata/validation journeys, six capture-choice/encoder-fallback checks, two
desktop/mobile Chromium one-shot browser recordings and the actual no-JS
external-encoder recurring recording/replay journey pass. Evidence:
`target/accessibility/ap09-live-metadata-20260929/final-combined/`.
The general-purpose test encoder was absent from the upstream browser image
(`ENOENT`); the cached test image now includes Ubuntu's FFmpeg 6.1.1. Its image
ID and encoder version are recorded in the report. Only Ubuntu repositories
are needed for that layer. No production installation/runtime dependency was
added; browser engines retain their original pinned base.
