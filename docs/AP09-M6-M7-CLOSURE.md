# AP09 M6 / M7 closure work

Goal: close both original milestones, with positive, failure, authorization,
PeerTube and browser evidence. This checklist is a work plan, not a sign-off.
The milestone and inventory acceptance gates remain unchanged. M6 is signed
off below; M7 remains open until its original operations gates are verified.

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

- [ ] Worker/storage options and native baseline installation invariants.
- [ ] Redundancy/mirroring authority, retention and bandwidth controls.
- [ ] Default-on PeerTube P2P, pre-contact opt-out, persistence, live/VOD/embed
      and fallback/fault/browser matrices; no address disclosure without notice.
- [ ] Moderation/report/takedown propagation and privacy-preserving analytics.
- [ ] Acknowledged storage lifecycle/GC; consistent backup and actual restore.
- [ ] Defined workload budgets and concurrency/resource/startup/seek/rebuffer/
      live-latency measurements; retained positive and failure evidence.
- [ ] Operator documentation, inventory updates and original-gate sign-off.

No push, merge, deployment or PeerTube plugin ABI work is authorized here.

## Verified slices

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
