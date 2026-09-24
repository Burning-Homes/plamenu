# AP09: native video and live streaming

Status: implementation started; product architecture agreed. AP09 is confirmed;
the implementation and parity requirements below are not complete.

This is the working record for native video publishing, live broadcasting,
PeerTube interoperability, first-party usability, accessibility, and performance.
Passing the original audit regression is one milestone, not completion of the
broader video-platform work requested on 2026-09-22.

## Reference baseline

- Plamenu inspected at `f6b5998091b36ec35001b35ed01974511e906296`.
- Initial local PeerTube baseline: **8.2.4**, with live enabled.
- Initial reference source: `30eb3cc4f8701198ec1c785aedd2fbb5db401a2f` (8.2.4).
- Updated local PeerTube and source to stable **8.3.0** on 2026-09-22 at the
  owner's request. Source: `81bf3179bbdddbd6bfb431aeac4320e8f7c87c03`.
  Official image digest:
  `sha256:0bee710d0ac8da3fbc01383e0322d51d90072ee121eb56d8a4218d174d970be7`.
- PeerTube **8.3.0** is now released. Use its release as the parity reference
  and retain the 8.2.4 baseline as historical evidence. Testing 8.2.4 alone
  cannot establish current PeerTube parity.
- Pin tested server revisions/images and report actual running versions. A
  moving `develop` branch is useful for research, not a reproducible test target.

Sources:

- [PeerTube ActivityPub contract](https://docs.joinpeertube.org/api/activitypub)
- [PeerTube 8.3 release](https://joinpeertube.org/news/release-8.3)
- [PeerTube changelog](https://docs.joinpeertube.org/CHANGELOG)
- [Publishing and live broadcasting](https://docs.joinpeertube.org/use/create-upload-video)
- [Remote workers](https://docs.joinpeertube.org/admin/remote-runners)
- [PeerTube REST API](https://docs.joinpeertube.org/api-rest-reference.html)

## Confirmed starting state

| Surface | Finding | Evidence |
| --- | --- | --- |
| Local object types | No native Video, Audio, Image, or Document authoring | `PostKind` in `crates/ap/src/activity.rs` |
| Remote playback | Existing HLS, sparse/progressive MP4, and live remux paths; preserve these | `crates/server/src/routes/{hls,hls_live,live_stream,virtual_mp4}.rs` |
| Remote metadata | No ordinary `subtitleLanguage` / `hasParts` processing or persistence found | Search of Rust and migration sources |
| Playlists | PeerTube Playlist is not an accepted status type; HLS playlists are a separate concept | `CONVERTED_STATUS_TYPES` in `crates/server/src/ingest.rs` |
| Local media processing | Existing FFmpeg attachment processing, not a long-form publishing pipeline | `media_transcode.rs`, `media_worker.rs` |
| Upload limitations | Existing A/V setting clamps to 99 MiB; encoding uses a 36,000-frame bound | `TranscodeParams::from_settings`, `MAX_VIDEO_FRAMES` |
| Channels | Existing Group support is community-oriented; channel ownership needs explicit semantics | `GroupActorParams` in `crates/ap/src/actor.rs` |
| E2E | Two PeerTube tests cover inbound VOD and live; both explicitly declare no outbound publishing | `e2e/tests/test_peertube_federation.py` |
| Browser authoring | Post type offers Note, Article, Event; media is an attachment panel | Local Chrome compose inspection, 2026-09-22 |

Do not implement publishing merely by changing a Note's `type` to Video.
PeerTube expects a channel Group owned by a Person, channel attribution on the
Video, valid media links/metadata, and the corresponding activity lifecycle.

## Product decisions

Owner clarification on 2026-09-22: comparable video-platform capabilities and
interoperability where reasonable, with Plamenu's own design. Do not clone
PeerTube's UI or repeat its architectural choices merely for parity. PeerTube
plugin compatibility is explicitly excluded from the first round.

P2P decision: **PeerTube-compatible peer-assisted delivery enabled by default,
with an accessible opt-out** (owner answer, 2026-09-22). Preserve a server-only
playback path for scripts-disabled browsers and for P2P failure/unavailability.
Offer the preference through ordinary HTML forms as well as enhanced controls;
honour opt-out before joining a swarm or contacting trackers, including embeds.
Compatibility must be tested against PeerTube's actual tracker/segment protocol,
not inferred from both products using WebRTC. Keyboard and assistive-technology
users must be able to find, understand, and change the setting.

Accepted architecture and round-one scope:

1. Platform capabilities include channels, collaborators, imports, workers,
   storage/redundancy, analytics and moderation. Specify useful client API
   compatibility independently; PeerTube plugin ABI compatibility is excluded.
2. Plamenu owns storage, processing and ingest. No required companion service
   or new manual installation/maintenance steps. Use existing packaged media
   dependencies and native in-process services; optional scale-out must not be
   required for a working installation. Any unavoidable operational addition
   must be justified against installation simplicity and measured performance.
3. Browser camera and screen broadcasting are in this round. Live chat and
   automatic caption generation are later work. Uploaded captions remain a
   round-one accessibility requirement.
4. Design for future federated live chat: stable channel and broadcast-session
   identity, explicit ownership and moderation permissions, and replay/session
   relationships. Chat must not require changing video identifiers, exposing
   ingest credentials, or treating every chat event as a video edit.

The original request already requires authoring and consumption, real PeerTube
E2E exchange, performance work, and usable first-party flows without JavaScript.
These are not optional or postponed behind a JS-only implementation.

## Implementation contract

The following guide implementation under the accepted product scope.

- Keep account identity, channel ownership/collaboration, video metadata,
  media assets/renditions, playlists, and broadcast sessions explicit.
  Do not overload community moderator collections with channel ownership.
- Reuse existing attachment, transport, caching, ACL, and outbox machinery
  where the semantics match. Preserve ordinary social-post attachments.
- Publish stable Video and channel identifiers. Queue Create/Update/Delete
  atomically with state changes; retries must not duplicate publications.
- Native videos need long-form streaming uploads, bounded resource usage,
  resumability with JS, and ordinary multipart uploads without JS. Neither
  silent truncation nor buffering the whole upload in RAM is acceptable.
- Processing needs durable jobs, retry/cancellation/recovery, quotas, and
  atomic rendition publication. Copy/remux compatible streams when suitable;
  transcode only the required ladder. Measure CPU, memory, disk and latency.
- Private, unlisted, scheduled and public states need explicit rules across
  originals, previews, captions, transcripts, HLS segments and downloads.
- Playback retains native controls/progressive sources when scripts are off;
  JS adds adaptive quality, resume, chapter seeking, and richer controls.
- Caption tracks use native WebVTT where supported; transcripts and chapter
  links remain readable without JS. Descriptive text and audio description
  are distinct from captions and should not be substituted for one another.
- Live creation, settings, scheduling, credentials and replay management must
  work as HTML forms. Broadcasting without browser JS uses an external encoder.
  Browser camera/screen capture necessarily needs script and device permission.
- Treat remote captions, chapter collections, media links and playlist elements
  as untrusted fetches: guarded transport, redirect checks, bounded sizes and
  recursion, ownership validation, safe text rendering, and deletion handling.

## Milestones and acceptance evidence

The [feature acceptance inventory](AP09-FEATURE-INVENTORY.md) breaks the agreed
scope into testable backlog entries. It is intentionally not a parity percentage.

`pending` means no completion claim. `implemented` is not `verified`.
Each milestone must link its actual tests, browser observations and measurements
before becoming `verified`. Keep partial results and failures visible.

| ID | Milestone | Required acceptance evidence | State |
| --- | --- | --- | --- |
| M0 | Baseline and finite parity inventory | Current-version feature inventory; local peer/version records; baseline E2E; browser gap observations; product decisions | verified 2026-09-23 |
| M1 | Channels and native publication | Channel/account lifecycle; authorized collaboration; real Plamenu Video accepted, playable and attributed correctly in PeerTube; edit/delete/follow exchange | in progress |
| M2 | Long-form VOD pipeline | Large/resumable and no-JS uploads; durable processing; quality ladder; poster/storyboard; audio-only/silent inputs; failures/restart/quota evidence | in progress |
| M3 | Accessible metadata | Caption upload/edit/delete and languages; native tracks and transcript; chapters; descriptions; both-direction federation and update retention | pending |
| M4 | Playlists and library | Create/import/edit/delete; reorder; clip boundaries; ownership/visibility; duplicate and missing videos; PeerTube round-trip; keyboard/no-JS forms | pending |
| M5 | Native live | Authenticated ingest including browser camera/screen; waiting/on-air/interrupted/ended/replay states; stable session identities for future chat; scheduling, recurring streams, latency/DVR; PeerTube playback and updates in both directions | partial: no-JS authoring, RTMP/RTMPS/HTTPS-FLV and browser ingest, bounded HLS/MP4, generation-fenced interruption/reconnect, process-crash recovery including a recoverable one-shot replay, and PeerTube state/VOD updates verified; scheduling, recurring replay, DVR, active-session key revocation and P2P remain |
| M6 | Creator/viewer experience | Discoverable video space; channels, subscriptions/search; drafts/scheduling; settings/imports; editing; embeds/downloads/history; desktop/mobile/browser evidence | pending |
| M7 | Operations and scale | Worker/storage options; redundancy; PeerTube-compatible P2P on by default with accessible opt-out; moderation/analytics; recovery/backup; concurrency and resource benchmarks | pending |
| M8 | Ecosystem and beyond parity | Defined REST/embed/runner compatibility scope; chosen extensions; end-to-end tests per supported contract; no PeerTube plugin ABI in round one | pending |
| M9 | Completion gate | All accepted inventory entries verified; current and prior PeerTube matrices; browser/a11y/performance gates; operator and user documentation | pending |

The inventory needs expansion into individually testable features before a
percentage is reported. A milestone count is not a feature-parity percentage.
No "100%" claim while required rows are missing, deferred, failed, or untested.
AP09's Audio/Image/Document authoring also needs an explicit disposition; video
delivery alone must not silently close those parts of the original finding.

## Verification matrix

For each applicable feature, record all of these separately:

1. Plamenu publishes; PeerTube discovers, fetches, displays and plays it.
2. PeerTube publishes; Plamenu displays and plays it.
3. Updates, deletions, permissions, duplicates and delayed/out-of-order delivery.
4. First-party desktop and mobile: real player startup, audible audio,
   seek/quality/fullscreen, captions, keyboard/focus and errors.
5. Scripts disabled and blocked: authoring, settings, captions/transcript,
   playlists, playback, validation and recovery still usable.
6. Chromium, Firefox and WebKit; 320px reflow and zoom; reduced motion and
   forced colours; manual assistive-technology evidence where available.
7. Third-party attachment-only clients retain an independently playable URL.
8. Resource/security cases: hostile URLs and metadata, unauthorized access,
   oversized uploads/tracks, worker failure, concurrent viewers, cleanup.

Follow [accessibility verification](ACCESSIBILITY_RELEASE.md). Axe and an
accessibility-tree inspection do not establish screen-reader/device coverage.
Record unavailable manual environments as untested, not passing.

Performance measurements must record fixture, hardware, concurrency, cache
state, source revision and budgets. Measure startup/seek latency, rebuffering,
live latency, encode throughput, RAM/CPU, origin bytes and storage growth.
Prove that extra viewers do not create per-viewer encoding processes and that
unwatched remote videos do not trigger whole-file downloads.

## Execution log

### 2026-09-23 — M0 baseline sign-off

- Closed **M0** against its stated acceptance evidence, not against feature
  completion. The [acceptance inventory](AP09-FEATURE-INVENTORY.md) has 59
  uniquely identified requirement groups and explicit pending/partial states.
  The pinned local PeerTube API answers `serverVersion=8.3.0`, matching the
  recorded reference source `81bf3179bbdddbd6bfb431aeac4320e8f7c87c03`.
- The initial 8.2.4 baseline remains recorded below as 1 pass/1 failure; the
  later 8.3.0 runs do not erase it. The first-party browser gap and the
  owner's product decisions (native packaging, no first-round plugin ABI,
  camera/screen live, default-on P2P with opt-out) are captured above.
- This is a **baseline milestone**, not a parity percentage or a release
  sign-off. The inventory groups still require individual acceptance evidence
  and some will be split as implementation reaches them. M1–M9 remain open.

### 2026-09-22 — initial investigation

- Confirmed AP09 against the present code and audit evidence.
- Inspected authenticated Plamenu composer and PeerTube publishing in Chrome.
  Plamenu exposes only attachment uploads; PeerTube exposes channel selection,
  file publishing and live creation. URL/torrent import is disabled in this
  fixture, which is configuration, not evidence of missing PeerTube features.
- Started the existing two-test PeerTube baseline against the running local
  instances, without fail-fast:

  ```sh
  cd e2e
  PLAMENU_PEER_ROOT=/home/v/Projects/activitypub-dev uv run pytest \
    tests/test_peertube_federation.py -q --maxfail=0 \
    --junitxml=../target/e2e/ap09-baseline.xml
  ```

- Baseline result: **1 passed, 1 failed in 68.39s**. VOD passes. Live creation,
  delivery and on-air notification succeed, but going on air sets `edited_at`;
  `test_peertube_live_federates_and_plays_in_every_client` expects a lifecycle
  transition not to appear as a post edit. Later assertions were not reached.
  Report: `target/e2e/ap09-baseline.xml` in the original checkout.
- The live failure's root cause is not established. The same unmodified
  Plamenu subsequently passes the complete test against PeerTube 8.3.0.
  Do not describe the upgrade as a proven Plamenu bug fix or erase the failed
  8.2.4 result; investigate if it recurs.
- No application behavior changed during this investigation.
- Implementation checkout: sibling `plamenu-ap09`, branch
  `feat/ap09-native-video`, preserving the ongoing lightbox branch.

## Next work

The current priority is M5's remaining substantive live gaps: a separate
published Video for each recurring replay, scheduling, latency/DVR and
default-on compatible P2P
with opt-out. One-shot replay does not close M5. M2 still owes difficult-input,
maximum-size and concurrent-upload stress; M1 channel lifecycle/collaboration
and several federation paths remain open. Do not treat the current publication
slices as a milestone sign-off.

### 2026-09-22 — publication slice and browser verification

- Implemented initial owned channel identities, native Video publication,
  shared Create/dereference/Update shape, channel Announce, channel follower
  update/delete delivery, owner deletion handling, and client API endpoints.
  Channel management/collaborators/history and the full permission matrix
  remain incomplete.
- Added `/videos` HTML-first channel creation and upload/review/publication,
  single WebVTT upload, transcript and visual description. This currently
  reuses the short attachment upload pipeline and its 99 MiB limit; it does
  **not** satisfy long-form upload or adaptive streaming requirements.
- Backed up the standing local server executable and database before running
  the AP09 build against the local development instance. The database archive
  was list-verified, not restore-tested. No staging deployment or AP09 commit.
- Rust checks: four native-video integration tests and nine long-form tests
  pass; Clippy for the library and native-video test passes with warnings
  denied. SQLx offline metadata generation is still pending.
- New `accessibility/tests/native-video.spec.mjs`: Chromium desktop, 320px
  mobile and scripts-disabled flows pass (3 tests, 7.6 seconds). Firefox
  desktop/mobile also pass. WebKit desktop/mobile fail before application
  navigation because this host lacks the browser's expected ICU 74, libxml2
  and Flite libraries; these are untested, not application failures or passes.
- Connected Chrome: created a channel, uploaded a synthetic video and WebVTT,
  reviewed and played it, opened the transcript, exercised an invalid language
  error with upload/metadata retained, corrected it and published. Mobile ALT
  description is visible; 320px DOM reflow had no horizontal overflow. This
  does not establish manual screen-reader, audible-caption or device coverage.
- File chooser troubleshooting correction: the video was selected despite
  the initial tool timeout; caption selection succeeded on retry. The user's
  file-URL permission was already enabled. A subsequent Chrome connection loss
  was recovered. Neither incident demonstrates a disabled browser permission.
- Real PeerTube 8.3.0 matrix against the AP09 build: **2 passed, 1 failed in
  167.72 seconds**. Both inbound VOD/live tests pass. Outbound Follow/Accept
  succeeds; PeerTube receives the channel Announce and fetches the Video but
  never exposes it in subscriptions, so the outbound test fails before its
  playback/update/delete assertions. An earlier run used the wrong assertion
  database and is not evidence of a Follow implementation defect.
- Confirmed contract mismatch from the served Video and reference source:
  Plamenu's only `icon` is `image/avif`; PeerTube 8.3's
  `setValidRemoteIcon` filters it out because `MIMETYPES.IMAGE` accepts PNG,
  GIF, WebP and JPEG, then the Video validator requires a nonempty icon list.
  A compatible rendition is needed; outbound interoperability is not verified.
- Independent outbox issue: PeerTube's signed `?page=true` requests receive
  401. Its `server/core/helpers/requests.ts` signs URL `pathname` without the
  query. Plamenu deliberately does not allow this for arbitrary signers
  because it would leave query parameters unauthenticated. No security
  relaxation has been made. Channel outbox/history is still an open feature.

### 2026-09-22 — compatibility poster and signed history implementation

- Added a cached compatibility poster without changing the AVIF native preview.
  The derivative is serialized by a media-row lock, included in storage totals
  and reconciliation, hidden on owner suspension, and queued for deletion by
  an atomic cascade trigger (including abandoned, unattached uploads).
- Channel history now advertises path-based keyset pages. This avoids the
  PeerTube query-signing defect without changing signature verification.
  Owner/channel moderation filters and deleted-status filters apply before
  counting/pagination. Local channel subscription timelines are still pending.
- Added real PeerTube scenarios for existing subscribers and subscriptions
  made after publication; neither uses URL search to force an import.
- Initial regression run: **25 Rust tests passed** (7 native video, 9 outbox,
  9 long-form). Fixed Clippy's nested-Option warning with an explicit view enum;
  targeted Clippy subsequently passed. Regenerated SQLx metadata with `./dev sqlx`.
- Actual browser verification then caught an inadequate poster fixture: real
  video-generated AVIF failed conversion while a still-image-generated AVIF
  passed. The new build's seven browser scenarios all failed publication with
  that same image error; **WebKit successfully launched in the pinned container**.
  The real peer run was **2 failed, 2 passed in 213.13 seconds** (outbound failed
  at publication, inbound VOD/live passed). These are not interoperability passes.
- Root cause: FFmpeg's AVIF muxer removes temporal-delimiter OBUs, while its
  raw OBU demuxer expects them. Restore missing framing when passing extracted
  AVIF colour/alpha items to the decoder. Reference: [FFmpeg raw AV1 demuxer](https://www.ffmpeg.org/doxygen/7.1/libavformat_2av1dec_8c_source.html).
  Replaced the weak regression fixture with a poster from `process_av`; the
  corrected build and reruns are pending. The browser review retained the
  uploaded video and metadata across the failed publication.
- Browser runner improvements: generate the synthetic MP4 on the host using
  the existing FFmpeg dependency, mount it read-only into the pinned browser
  image, and support linked worktrees in the evidence reporter. No host package
  installation was needed. Added an optional `PLAMENU_BINARY` E2E override to
  test the running candidate instead of rebuilding CLI fixtures mid-run.

### 2026-09-22 — real playback and poster selection

- The actual-video AVIF regression now passes: 7 native-video integration
  tests and 20 image-processing unit tests. Local candidate v3 recovered the
  retained browser upload without another upload.
- The pinned publication browser matrix passes **7/7 in 25.5s**: Chromium,
  Firefox and WebKit desktop/mobile, plus Chromium with JavaScript disabled.
  This covers channel creation, upload/review/publication, caption attachment,
  reflow and automated accessibility checks; it is not full player/a11y parity.
- Connected Chrome visibly played the native preview with its uploaded cue.
  PeerTube imported that same publication through a channel subscription,
  attributed it to the correct Person and Group, played it and visibly
  rendered `[Steady test tone]` after enabling captions.
- v3 peer run: **2 failed, 2 passed in 95.81s**. Both outbound cases imported;
  the assertion incorrectly assumed the legacy `thumbnailPath` covers all
  ratios. PeerTube 8.3 puts the preserved 4:3 poster in `thumbnails` instead.
  Corrected the assertion to fetch the actual advertised thumbnail URL.
  The subsequent four-case run was terminated with exit 143 after three
  passing cases and no final report; do not count it as a complete pass.
- Browser inspection found a separate real presentation gap: PeerTube's
  player and thumbnail components explicitly select only 16:9 thumbnails.
  Added a separately cached, padded JPEG poster without cropping/stretching
  the frame or changing the native AVIF. Emit the original video aspect ratio.
  Added portrait/square/landscape padding tests; verification is pending.
- Began M2's file-based processing primitive: no full-input RAM read or second
  input copy, with the same byte/probe limits and owned output scratch cleanup.
  Removed the silent 36,000-frame encoder cutoff; added a real 36,120-frame
  regression. This is not yet a streaming-upload or durable-job completion.
- Candidate v4: **4/4 PeerTube E2E passed in 95.31s**, no skips. Both outbound
  subscription timings completed thumbnail fetch, byte-range and FFmpeg decode,
  owner edit and deletion; inbound VOD/live regressions also passed. Private
  runtime report: `peertube-v4.xml`.
- Browser confirmed the fresh v4 video appears in the PeerTube channel, plays
  with captions and now has a visible poster before playback on a fresh load.
  Padded-poster/long-clip regression run: **8 native-video + 21 image tests passed**.
- Next source iteration streams the HTML file part to private scratch storage,
  checks CSRF before disk allocation, bounds small fields while receiving them,
  and processes from disk. Silent films remain `video`, not looping `gifv`.
  Byte caps have not been raised: durable jobs/admission/quotas must precede
  exposing large-file limits. Failure/cleanup and browser reruns are pending.
- Disk-spool verification: **1 web integration + 29 existing media + 8 native
  video tests passed**. The new web test covers bad CSRF before allocation,
  oversized files/captions, malformed media cleanup, silent-film classification,
  and retaining the upload through rejected metadata and successful retry.
  Regenerated SQLx metadata after making optional compatibility-poster columns
  explicitly nullable across planner variations. Offline Clippy and the
  isolated publication browser matrix are running.

### 2026-09-22 — storage, metadata and editing follow-through

- The isolated pinned browser publication matrix passed **7/7 in 24.0s** after
  disk spooling, including JavaScript-disabled submission. Report:
  `target/accessibility/ap09-spooled-publication.json`.
- Local attachment workers now borrow the stored input path instead of loading
  the whole original into RAM. Other storage backends use bounded streamed
  scratch copies, with declared-length validation and cleanup. This does not
  change the existing worker's retry/lease semantics into a native VOD queue.
- Native video titles now follow the peer's 3–120-character contract. Video
  descriptions have their own 10,000-character budget rather than inheriting
  short-post limits. Outgoing descriptions preserve sanitized HTML instead of
  stripping formatting and mislabelling plain text as Markdown. Serialized
  descriptions exceeding the peer's envelope are rejected transactionally.
- **9 native-video, 30 media and 9 long-form tests passed**, including long
  descriptions, literal asterisks, rejected-title/description rollback and
  processing stored local files without deleting the original.
- Manual browser inspection found that editing omitted the video title and
  offered attachment removal that the publication contract rejects. Added a
  labelled title field, the video description counter and an explanatory
  fixed-file control. Extended the browser matrix through preview/save/reopen;
  this UI is now verified (see results below).
- Federation coverage classification passes: the native outbound scenarios
  are paired with inbound PeerTube VOD; native live remains explicitly pending.
  The pre-existing AP11 client/rendering-only scenario is classified as local,
  not misrepresented as an exchange with another implementation.
- The broad regression gate passed 330 tests with one new test expectation
  needing correction: blank titles hit the existing blank-title validation
  before the video-length check. The corrected upload/edit test passes. A
  pre-existing emoji error test also needed its expected markup updated for
  the accessibility ID already present in the base implementation. Offline
  all-target workspace Clippy passes with warnings denied.
- Candidate v5's first editing matrix failed due to test assumptions: saved
  permalinks need not retain the original fragment, and no-JS preview applies
  the established trailing-whitespace normalization. After correction,
  **7/7 browser scenarios passed in 32.1s**, including desktop/mobile reflow,
  automated accessibility scans, no-JS preview/save and reopening the edit.
- The first v5 peer run was **2 failed, 2 passed in 93.52s**: the new description
  assertion requested an API subresource removed in PeerTube 8.3. Its details
  endpoint now includes the complete description. After correcting the test,
  **4/4 passed in 98.24s**, no skips (`peertube-v5-metadata.xml`). This covers
  long descriptions and literal asterisks plus short-description updates.
- Connected Chrome confirmed the same manual title/body edit in Plamenu and
  PeerTube, including literal asterisks, retained captions and the padded poster.
- Next source iteration creates the channel's local Announce entry in the same
  publication transaction, copying the original visibility and moderation
  metadata. This should populate channel profiles and local subscriber feeds
  without treating channels as community submissions. Added owner-facing
  channel links and removed community ranking controls from local channels;
  new audience/filter/deletion and browser checks are pending.
- Local channel checks now pass for both timeline orderings: public/unlisted
  delivery, no unlisted discovery promotion, creator-block filtering and
  deletion. Native channels are excluded from community attribution, vote
  fan-in and community directories. Explicit film publication converts a
  client-uploaded silent `gifv` to non-looping `video` transactionally.
- Owner-menu inspection also found unsafe delete-and-redraft: the ordinary
  composer cannot preserve a video's channel/media. It is now hidden for Video
  and rejected server-side before deletion; ordinary post redrafting remains.
- Clean gate: **366 selected Rust tests passed** (274 web, 10 native-video,
  31 hosted-community, 26 query-count, 9 outbox and 16 group database tests),
  plus all-target workspace Clippy with warnings denied. The next browser
  matrix adds actual decoded playback/WebVTT activation and channel navigation;
  candidate v6 built and was installed on the standing local instance.
- Candidate v6's PeerTube exchange rerun passed **4/4 in 96.56s**, no skips
  (`peertube-v6.xml` in the private runtime evidence directory).
- The expanded browser matrix first caught the fixture's invalid canonical
  media origin (random TLS port versus absolute `https://localhost` URLs).
  Request rewriting was unsuccessful and removed. Browsers now share the
  disposable proxy's network namespace, using canonical port 443 unchanged;
  no production hostname validation was weakened. A subsequent owner-menu
  selector failure was corrected to target the native disclosure by title.
- Candidate v6 then passed **7/7 browser scenarios in 37.2s**: actual decoded
  playback, activated WebVTT cues, upload/review/publish/edit/preview/save and
  channel navigation in desktop/mobile Chromium, Firefox and WebKit, plus
  Chromium with page JavaScript disabled. This does not establish native-player
  keyboard or assistive-technology conformance. Report:
  `target/accessibility/ap09-video-channels-playback.json`.
- Next iteration distinguishes native video channels from communities in
  profile badges, handles and publication banners, while retaining the
  ActivityPub Group type. Account entities gain a `video_channel` boolean;
  list/feed rendering resolves it once per batch of local Group accounts.
  Remote channel classification remains separate pending work. Verification
  of these presentation changes passed **342 selected Rust tests** (274 web,
  10 native-video, 31 hosted-community and 27 query-count guards), plus
  all-target workspace Clippy with warnings denied. Candidate v7 built;
  its full frontend browser matrix is in progress.
- Manual Chrome: Space on the native play control played the local-channel
  sample to completion with the caption visible; Enter on the transcript
  disclosure expanded its text. This is a limited keyboard smoke test, not a
  screen-reader conformance claim.
- During local candidate replacement, logs exposed a separate inbound-history
  issue to investigate: PeerTube's root outbox returned HTTP 304 and the history
  worker treated it as fetch failure, eventually reaching its finite failure
  budget. This does not invalidate the passing targeted exchange tests, but
  prevents claiming general history recovery is complete.
- Candidate v7's full frontend matrix was **32 passed, 6 failed**. All seven
  native-video cases passed, including the new channel presentation. Five
  representative-page scans found undersized shared ALT controls; these now
  have 44×44 CSS-pixel targets, with image/video geometry assertions and
  keyboard/no-JS tooltip coverage. One Firefox-mobile lightbox drag dismissed
  the viewer unexpectedly; pointer-event diagnostics were added for the next
  run rather than weakening the assertion. Manual Chrome confirmed the new
  channel badge, @handle and “published on” banner.
- The HTTP response walker handled generic 3xx before 304, wrongly requiring
  a Location header. It now recognizes unchanged representations first.
  Deterministic tests exercise direct/redirected 304 and preserve redirect
  policy/SSRF refusals. A real PeerTube creator revalidation E2E was added.
- Its first run against v7 failed earlier than 304: a new creator's empty
  outbox (`totalItems: 0`, no first page) was rejected as unsupported. Empty
  declared collections now complete and retain their ETag; a Rust regression
  covers this. The initial E2E run is not a successful 304 reproduction.
- A wider Rust run passed **485/487**: it caught missing Russian video strings
  and ingest using a viewer-facing media lookup that queues downloads. Added
  the translations and a read-only attachment lookup for ingest/inline-image
  rewriting. The existing cold-history/media-promotion regression covers this
  failure; normal viewer-triggered fetching remains unchanged. Corrected gate:
  **838/838 tests passed**, no skips, including server/federation unit tests
  and remote-history, long-form, media, native-video, web and query-count
  suites. All-target workspace Clippy passed with warnings denied. Candidate
  v8 built. Real PeerTube verification: **5/5 passed in 108.89s**, including
  fresh empty-creator outbox caching and unchanged HTTP 304 revalidation.
  The browser matrix was **37 passed, 1 failed**; all ALT target checks and
  native-video cases passed. The remaining Firefox-mobile mouse drag produced
  no pointer events. A minimal standalone probe reproduced this only with
  `isMobile: true`; Playwright documents that flag as unsupported in Firefox.
  The Firefox project now keeps 320×800 and touch input without that flag.
  The test additionally checks that dragging actually moves the track and uses
  a real touch tap to dismiss in touch projects. No production gesture code
  was changed. Corrected full matrix: **38/38 passed in 2.9 minutes**, no skips,
  recorded in `target/accessibility/ap09-v8-full-corrected.json`. This covers
  the seven browser projects, not physical mobile devices or assistive-tech
  conformance. The tree is dirty, so this is development evidence, not a
  release accessibility attestation.
- Hot-path verification first stopped before measurements because the cached
  synthetic dataset predated migration 0079's default catalog row. Rebuilt
  **only `plamenu_bench`** with the documented seed; local application and
  PeerTube databases were untouched. The regenerated dataset can be recreated
  from the seed; the previous disposable rows were not backed up.
- The subsequent full 70-case benchmark completed and preserved its write/job
  invariants. Strict grading found five extra-query failures (community
  new/top/hot, local public timeline, web explore), plus low-headroom warnings
  for relationships and inbound deletion. No latency or response-size ceiling
  was exceeded. The channel-classification read is now folded into the existing
  account overlay; login-less channels still retain null `noindex` and no user
  role. Added an assertion that channel rendering uses no more SQL statements
  than person rendering, alongside the 3-vs-12 batching regression. The measured
  v8 Criterion output is archived privately. The corrected full 70-case run
  completed: all query, latency and response-size ceilings passed. Strict
  grading still fails its headroom requirement for `write/inbox_create_reply`
  (23%, below the 25% target); zero run-level problems. Relationships and inbound
  deletion no longer trigger warnings. This is dirty-tree development evidence,
  not a revision-bound performance attestation. Corrected output is archived
  privately as `bench-overlay-fixed.tar.gz`.
  No benchmark budgets were raised.
- The account-overlay optimization passed all-target Clippy and **331/331**
  native-video, profile, web and query-count tests, with no skips.
- M2 persistence implementation has started (migration 0086 and `video_draft`
  database module): stable media/draft identity and metadata, reserved source
  bytes, serialized source-admission limits, single-item renewable generation
  leases, explicit retry/cancel/expiry, retained originals in reconciliation
  and storage metrics, and protection from the generic orphan-upload vacuum.
  Tests cover concurrent admission, foreign ownership, expired/stale claims,
  source retention, readiness, published-media protection and file cleanup.
  Initial tests caught the existing attachment-source constraint; reservations
  now point at their private `.orig` source from creation without weakening
  that invariant. Corrected full database suite: **420/420 passed in 73.364s**,
  no skips, including all seven new lifecycle/concurrency tests. All-target
  workspace Clippy passed with warnings denied, and SQLx metadata was generated
  with `./dev sqlx`. At that checkpoint no worker or HTTP/HTML caller used these
  primitives. Attempt-output reservations, scratch/output budgets, private
  preview authorization and end-to-end durable upload UI are still required;
  source admission alone is not complete storage-quota enforcement. The
  standing v8 instance still used the synchronous upload/review flow.

### Next M2 implementation constraints

Bounded encoder groundwork is now implemented. The first version passed
all-target Clippy and **464/464** selected server/media/native-video/HLS tests
in 18.626s (no skips), including four real-kernel/FFmpeg cases: exact output cap,
unchanged parent limits, ordinary small output without config/DB startup,
source retention and scratch cleanup after failure, and same-PID cancellation.
The subsequent parent-death-signal follow-up passed all-target Clippy and
**465/465** selected tests in 22.844s, with an additional Linux test killing a
disposable parent during a running encode.
A hidden
entry point in the existing Plamenu binary sets child-only Unix kernel file-size
limits, disables child core dumps, then replaces itself with FFmpeg. It does not
start the app runtime, load configuration or open a database. This preserves
same-process cancellation and avoids both unsafe pre-exec hooks (forbidden by
workspace policy) and a separately installed limit helper. `rustix` 1.1.4 was
already in the lockfile; it is now a direct Unix dependency with its `process`
feature. No crate version was upgraded. See the primary
[setrlimit API](https://docs.rs/rustix/1.1.4/rustix/process/fn.setrlimit.html).
Linux/FreeBSD additionally request a kernel kill on parent death and verify the
expected parent id after arming it, closing the spawn-to-limit setup race. Other
Unix hosts still have the file cap and normal cancellation, not this crash-kill
guarantee. These checks do not reclaim scratch directories left by a hard server
crash; restart scavenging remains part of worker integration.

This is a per-file bound, **not** a total storage quota or full process sandbox.
The native caller must supply/reserve its output budget; poster and hash-frame
files are each capped at 4 MiB (or the smaller caller limit). Encoder diagnostics
retain only a 16 KiB prefix while draining the pipe. Ordinary attachment callers
still leave file-size limits unset. Unsupported non-Unix hosts refuse an explicit
bounded job rather than silently dropping the limit. Production worker/HTML wiring and
aggregate scratch/output admission remain pending. The standing v9 binary does
not include this subsequent encoder change yet.

The journal/private-preview groundwork (migration 0087)
reserves per-generation MP4/AVIF output keys before storage; retired keys stay
in the journal to prevent reuse while deletion is queued. Reconciliation keeps
active reservations, and cancellation/purge cascades enqueue their cleanup.
Readiness requires that generation's reserved outputs, not the original source.
The `.draft.` output namespace fails closed without a committed video publication,
including after draft and cleanup records disappear. Captions and every public
playback alias also deny unpublished drafts. A cookie-authenticated owner preview
supports streaming/ranges and captions with `private, no-store`, including errors.
Publication locks and rechecks the media/draft; ordinary social-post attachment
cannot consume a native draft. This checkpoint did not yet wire a worker, upload
caller, progress/retry UI or scratch/output byte budgets.

This slice passed all-target Clippy and **1,159/1,159** selected database/server,
web, native-video, media and HLS tests (144.896s, no skips), including two added
journal tests and three owner/privacy/range/publication tests. HTTP fixtures use
small placeholder bytes to isolate access control; this is not decoder proof.
The standing instance was subsequently updated to v9 (database migration 88),
after a fresh private database snapshot whose archive listing was verified.
No full restore drill was performed. Real PeerTube: **5/5 passed in 105.17s**.
Full browser matrix: **38/38 passed in 2.9 minutes**, including no-JS authoring,
playback/captions and the seven browser projects; report
`target/accessibility/ap09-v9-full.json`. These browser journeys still exercise
the synchronous first-party upload path, not the unwired durable-draft flow.
Manual Chrome on v9 played the existing synthetic film to 3/3 seconds using
Space (320×240 decoded, no media error), visibly showed the caption, and opened
ALT and transcript using Enter. This is not assistive-tech conformance proof.
After the restart the first navigation showed the offline fallback; its normal
“Try again” link recovered immediately. No security/browser settings were changed.

A subsequent query-plan check found a full attachment scan in raw-file access
checks. Migration 0088 adds key indexes for media, previews, segments and profile
images. The query now resolves candidate media ids by key before ownership joins.
On the synthetic database's **171,075 attachments**, both an existing attachment
key and an absent draft key use bitmap file-key indexes instead of the previous
parallel attachment scan. Empty poster/segment tables may still correctly use
tiny sequential scans. This is a query-plan check, not a whole-playback benchmark;
the revised query passed all-target Clippy and **1,186/1,186** selected tests in
165.084s, no skips (the previous selection plus query-count guards). The 70-case performance
record above predates migrations 0087–0088 and is not evidence for their write cost.

- Keep the now file-based transcode primitive and streaming multipart parser.
  Do not restore byte-vector APIs in the native path when adding durable jobs.
- Introduce an owned draft/upload record before raising the public byte cap.
  Persist metadata and accessibility fields with the upload, so a dropped
  connection or a server restart does not lose the review/publish workflow.
- Give native VOD its own bounded processing lane. Existing attachment jobs
  use a ten-minute lease with batches claimed before serial processing; that
  is not sufficient for long encodes. Claim one job per slot, renew a fenced
  lease, and condition completion/cancellation on its claim token.
- Preserve originals across retriable failures. Publish complete renditions
  atomically and clean only the losing attempt's uniquely named files. Never
  delete source bytes on an uncertain database commit.
- Keep durable originals outside publicly served media keys. The existing
  `.orig` extension is explicitly refused by the media route and can be reused
  with private-source regression tests. Attempt outputs use the explicitly
  protected `.draft.` namespace; arbitrary unreferenced flat-store keys outside
  that namespace are not private.
  Include original references in cleanup, reconciliation and storage accounting.
- Enforce active-upload/storage quotas transactionally before accepting large
  files; include incomplete uploads and originals. Expire abandoned uploads
  through the existing cleanup/reconciliation system.
- An ordinary GET progress/review page and explicit refresh/retry/cancel forms
  are the no-JS baseline. JS enhancement may poll/progress/resume without
  moving focus or repeatedly announcing every percentage to assistive tools.
- Stable draft and media identifiers must survive processing/retry. Resumable
  offsets require serialized writes and crash-safe byte accounting; do not
  infer completion from a client-provided final byte count.
- Reserve attempt output keys in the database before storing them as well as
  source keys: reconciliation must not race an encode between its final rename
  and commit. Each generation owns distinct outputs; completion transfers those
  references to the media record in the same fenced transaction. A stale
  attempt must never overwrite or clean the winning attempt's files.
- Reuse the process-wide heavy-media gate inside the transcode primitive; do
  not acquire it a second time around the worker (that can deadlock a one-slot
  installation). The native lane separately bounds claimed jobs and renews its
  lease even while waiting for a heavy-encode permit. Cancellation or loss of
  the lease must drop the encode future, preserving `kill_on_drop` semantics.
- Publication must mark the owned draft as published inside the existing
  status/outbox transaction, not in a later request-side update. Persist source
  ownership through publication for reprocessing, and include source bytes in
  quota/accounting until the associated media is deleted.
- Do not let the ordinary one-day orphan-media vacuum delete an actively owned
  draft. Give drafts explicit expiry and cancellation semantics; account purge
  must still remove them even when the account remains as a tombstone.
- Original retention is not draft privacy by itself. Test access to finished
  draft previews/captions as owner, another member and signed-out visitor;
  an unreferenced random storage key is not an authorization mechanism.
- Fence the final file-storage window as well as the database update. Otherwise
  cancellation can enqueue/delete an attempt key before an old worker writes
  it, leaving a late orphan behind. Coordinate that short storage window with
  cancellation; never hold a database transaction through the entire encode.
- Admission must count reservations under concurrency, not only completed
  uploads. Output growth can exceed the input size, so an assumed “twice the
  original” reservation is not a hard bound; processing needs a real scratch/
  output budget and final accounted-size check as well as source limits.

### 2026-09-22 — Durable worker execution and cancellation

- Added a single-job native worker, separate from the ordinary attachment
  worker. It renews the generation lease during source loading/encoding, checks
  video output, commits unique output-key reservations before storage, and
  completes media metadata and draft readiness in one fenced transaction.
  Failures retain the original for retry; silent films remain `Video`.
- Final storage runs in an owned task. Dropping an async filesystem future is
  not a guarantee that its blocking write has stopped, so cancelling the caller
  must not release the database lock while that write can still complete.
  Slow final storage emits a warning after 30 seconds but is not force-cancelled.
  Cancellation/deletion waits for this fence. Crash recovery continues to rely
  on generation fencing, private journaled keys and reconciliation.
- The first version passed all-target Clippy and **65/65** selected real-encoder,
  durable-worker, native-video, media and HLS tests in 20.610s, with no skips.
  Five new worker cases cover ready/private real output, oversize failure and
  same-draft retry, cancellation during source loading, caller cancellation
  during final storage, and failure after the main file but before the poster.
  The final-storage test probes the actual database row lock and verifies that
  deleting the finished draft queues its original and both outputs.
- Follow-up: a non-cloneable processing reservation now travels with the worker
  into final storage. Admission atomically counts active jobs and a conservative
  temporary-file allowance (source spool, output/copy and poster/hash/copy).
  The input size is clamped to the admitted source allowance; output files retain
  their kernel caps. Concurrent admission/overflow and detached-finalizer slot
  retention passed all-target Clippy and **473/473** tests in 22.351s. The final
  follow-up removes scratch before releasing its reservation and adds an
  admitted-source-budget test, including refusal before scratch allocation.
  Broader database/server/web/media/query-count regression: **1,200/1,200**
  passed in 162.398s, no skips; all-target Clippy again passed with warnings
  denied. No SQL changed in this worker slice; offline SQLx checks were used.
- This is not enabled in the production scheduler or first-party upload form.
  Durable original/output quotas, free-space checks, crash scratch scavenging,
  scheduler drainage and no-JS/JS progress/retry/cancel UX remain integration
  requirements. In-process temporary budgets are not a filesystem-wide quota:
  other processes and stored renditions can consume space independently.
  Standing v9 and its previous browser/PeerTube results do not cover this worker.

### 2026-09-22 — Media input boundary follow-up

- A controlled concat-playlist fixture confirmed that the existing probe could
  inspect another local fixture file before application-level format validation.
  The new refusal regression failed before the fix. A loopback-only network
  fixture did **not** reproduce outbound contact; this is not evidence of a
  demonstrated network exploit or arbitrary-data exfiltration.
- File-processing probes/encoders now specify the existing accepted demuxers
  before opening each input and allow only the `file` protocol. Both inputs of
  a separated video/audio command receive their own restrictions. Dedicated
  HLS/live networking is separate and unchanged. These options are documented
  by FFmpeg's [protocol](https://ffmpeg.org/ffmpeg-protocols.html#Protocol-Options)
  and [format](https://ffmpeg.org/ffmpeg-formats.html#Format-Options) references;
  they are a narrower I/O boundary, not an operating-system sandbox.
- Probe JSON requests only fields actually consumed (including cover-art
  disposition) and refuses output beyond 256 KiB. A noisy subprocess fixture
  must fail within two seconds, without waiting for the ordinary probe timeout.
  All-target Clippy passed with warnings denied; **1,204/1,204** selected
  database/server/web/media/query-count tests passed in 159.732s, no skips.
- Built local candidate v10 and replaced v9 without a schema change (still
  migration 0088). v9 exited successfully, but its streaming worker again hit
  the existing drain timeout and was aborted. v10 readiness passed after
  startup; the first immediate startup probe briefly received HTTP 502.
  Native durable processing remains unhooked from scheduler and UI.
- Real PeerTube 8.3 verification: **5/5 passed in 104.98s**, including native
  outbound publication/history/edit/delete, conditional creator revalidation,
  inbound VOD ranges/decoding and waiting/on-air/ended live streaming with
  shared remuxing. Private report: `peertube-v10.xml`.
- Manual Chrome: uploaded and published an unlisted synthetic film through the
  ordinary HTML review form. Space playback reached 3/3 seconds with 320×240
  decoded and no media error; screenshot showed the caption, description-only
  ALT popup and expanded transcript. This verifies the shared media boundary
  on v10, not the unhooked durable worker or assistive-tech conformance.
  Synthetic status id: `117314115491135488`.
  Full seven-project/no-JS browser rerun: **38/38 passed in 2.9 minutes**, no
  skips; report `target/accessibility/ap09-v10-full.json`. Includes native
  authoring/editing, real decoding/captions, reflow and automated accessibility
  checks. Firefox's narrow/touch project is not physical Android testing.
  This remains dirty-tree development evidence, not a release attestation.

### 2026-09-22 — Durable stored-media quota ledger (in progress)

- Migration 0089 introduces per-file native storage reservations independent
  of account/media deletion cascades. Source admission and output admission
  share the existing short admission lock; quota checks include all retained
  native files, including published originals and bytes awaiting cleanup.
  These quotas concern media-store files, not PostgreSQL metadata storage.
- Verified source lengths shrink their reservations. Attempt outputs reserve
  their measured MP4/AVIF sizes before any final writes plus a bounded 1 MiB
  compatibility-poster allowance. The JPEG uses that generation's pre-reserved
  key and shrinks its allowance to the actual stored size when committed.
- Successful file cleanup (including an acknowledged missing file) atomically
  refunds reservations and completes jobs in one statement. Exhausted cleanup
  jobs retain their charges. Reconciliation includes charged-but-missing keys
  so an exhausted/uncertain cleanup cannot leave an unrefundable ghost charge.
  Candidate keys are collected before references to avoid treating newly
  admitted, unwritten outputs as orphans.
- Old attempts are retired/enqueued before encoding the next attempt, not only
  after successful new-output admission. Otherwise a full quota could prevent
  both the retry and the cleanup needed to make that retry possible.
- Migration backfill does not guess missing output sizes from source sizes.
  Unknown sizes refuse new admission until measured under the media-row fence;
  live attempts are skipped. Recovery also grants older ready drafts one bounded
  1 MiB allowance for their not-yet-generated compatibility poster, journaled
  atomically under admission/media fences. This backfill preserves existing
  videos even above a newly configured quota; it does not allow new uploads to
  bypass the quota. Failed/processing and retired attempts receive no allowance.
- Applied 0089 only to the AP09 development/test database. Generated SQLx
  metadata using `./dev sqlx`; all-target Clippy passed. The first expanded run
  passed 1,211/1,213 tests: two cleanup fixture lists needed the newly reserved
  JPEG key. Corrected those assertions and added real schema-0088-to-0089 tests
  for missing/retired files, concurrent idempotent recovery, poster journaling,
  cleanup/refunds and active-claim protection. The rerun passed **1,215/1,215**
  tests with no skips in 169.138 seconds.
  Standing v10 and its database remain unchanged.
- Production policy/defaults, free-space admission, crash-scratch scavenging,
  scheduler integration and the HTML/JS progress/retry/cancel flow remain open.
  A default per-account-quota preference question has been offered; the storage
  primitives take explicit limits and do not encode that policy decision.
- Before enabling production quotas, account for legacy/M1 native publications
  that have no `video_drafts` row, and files queued for deletion before the
  ledger existed. Migration 0089 backfills surviving durable drafts, not every
  historical native-publication or orphan file. API publication through the
  ordinary attachment uploader also needs the same accounting boundary.

### 2026-09-22 — HTML-first durable draft recovery (in progress)

- `/videos` lists the owner's recent unpublished drafts. Draft pages expose
  saved processing state, an explicit refresh link, review when ready, retry
  after failure and confirmed cancellation. GET requests never change state.
  Mutations check CSRF and ownership; cancellation requires an explicit
  checkbox, checked on the server as well as by native browser validation.
- Review uses private cookie-authorized video/caption URLs, displays visual
  descriptions and transcripts, and fixes the channel to the draft's owner.
  Draft pages/review/list responses are private and non-cacheable. Expired
  drafts cannot be reviewed or retried. Suspended channels cannot be retried.
- These routes do not yet switch the ordinary uploader to durable admission
  or schedule workers. JavaScript phase updates are also still pending; the
  current refresh/retry/cancel controls are ordinary HTML.
- Added route tests and a seven-configuration browser case using a disposable
  database seeded with ready/failed states and real media. This isolates
  recovery UI, keyboard confirmation, private decoding/captions and reflow;
  it is explicitly not evidence for upload-to-worker E2E. All-target Clippy
  passed; the expanded Rust regression selection passed **1,217/1,217** with
  no skips in 179.958 seconds.
- First browser expansion: 41/45 passed. The final four cases hit HTTP 429
  during login because the suite exceeded the unchanged per-account/IP login
  budget. They did not reach their target UI. The fixture now reuses a real
  browser-authenticated session within each worker, only for signed-in cases;
  production limits remain unchanged. Initial reports/screenshots are archived
  privately as `ap09-draft-pages-initial.tar.gz`.
- Screenshot inspection found review content displaced by the draft/channel
  lists and recovery controls with small touch targets. Review now omits those
  lists; draft recovery links, buttons and confirmation labels have at least
  44-pixel targets. Deletion uses the existing danger style. The expiry date
  uses the viewer's clock and zone, with the exact timestamp retained without
  JavaScript. Added browser size assertions. Final all-target Clippy passed;
  the post-polish web regression rerun passed **279/279** with no skips in
  48.462 seconds. Full canonical-origin browser rerun: **45/45 passed in 3.2
  minutes**, no skips, seven configurations including Chromium without JS.
  Report: `target/accessibility/ap09-draft-pages-polished.json` (`passed`,
  `source_dirty=true`). Screenshot inspection confirmed review placement,
  visible captions/visual description and 320-pixel reflow. New checks verify
  44-pixel recovery targets, exact expiry display and native keyboard
  confirmation. Full-page narrow-screen screenshots include the fixed bottom
  navigation at its viewport position; they are not physical-mobile or
  assistive-technology certification.
- Standing v10, its schema-0088 database and the synchronous upload flow were
  not changed by this slice. No AP09 commit, publication or deployment was
  performed. Production uploader/scheduler wiring, comprehensive admission
  and JavaScript phase updates are still the next integration work.

### 2026-09-22 — PeerTube reference upgrade

- GitHub's latest release API identifies `v8.3.0`, not a prerelease.
- Source checkout was clean; switched from the 8.2.4 commit to the 8.3.0 tag.
- Stopped PeerTube and backed up PostgreSQL, media, configuration and Redis
  before migration. Verified the database archive can be listed; a full
  restoration drill has not been run.
- Updated the active local Compose definition and the versioned E2E fixture
  to the same pinned official image digest.
- Automatic migration reached schema **1125**. Ran the required
  `node dist/scripts/migrations/peertube-8.3.js` as the `peertube` user.
- Verified `manualMigrationScriptsRun` records `peertube-8.3`; 18 videos and
  1 user preserved; `videoSearch` populated with 18 rows; HTTPS config reports
  8.3.0; container health is healthy.
- Local backup: sibling `peertube-test/ap09-pre83-p7ZBsz/` (0700). Its README
  records recovery requirements. No instance secrets or backups belong in Git.
- Post-upgrade E2E: **2 passed in 76.92s**, with no skips. The existing VOD
  and full waiting/on-air/ended live tests passed against PeerTube 8.3.0.
  Report in the original checkout:
  `target/e2e/ap09-peertube-8.3-baseline.xml`.
- Browser: upgraded authenticated publishing and video library loaded; library
  displayed 18 preserved videos. A pre-existing five-second test video played
  to completion in Chrome (`readyState=4`, no media error, 320x240 decoded).
- Browser: the same remote video played inside Plamenu. Starting playback
  activated the HLS quality selector (Auto/144p/240p); current time advanced,
  `readyState=4`, `paused=false`, 320x240 decoded, no media error.
- These checks establish the current inbound baseline, not outbound publishing,
  captions, playlists, P2P interoperability, cross-browser or no-JS completion.
- Compose validation and `git diff --check` passed for the fixture changes.

### 2026-09-23 — Native draft upload and worker integration

- The first-party video form now reserves a private `.orig` source and account,
  global, pending and filesystem capacity before reading the file part. Source
  bytes stream to a private scratch file; an owned finalizer fences its store
  rename and metadata/queue commit against browser disconnect and cancellation.
  The browser receives a private draft page, not an in-request transcode or a
  prematurely published status. Review/publication remains an explicit step.
- Native Video limits are operator-configurable in `[native_video]`, separate
  from the ordinary media attachment cap: source size, retained per-account
  and global storage, pending uploads, unfinished drafts, concurrent jobs, and
  minimum free disk. Defaults are 2 GiB/source, 50 GiB/account, 1 TiB global,
  8 GiB pending, 32 unfinished drafts, two jobs and 1 GiB free. A worker
  reserves conservative scratch/output capacity before claiming a job, uses a
  kernel-bounded output file, and leaves jobs queued when capacity is absent.
- Migration 0090 backfills pre-draft native publications and recognizable
  pending native cleanup keys into the storage ledger. Surviving file sizes
  are grandfathered; unknown sizes close admission until measured. This does
  not identify a pre-ledger M1 file whose media row was already deleted and
  whose filename is indistinguishable from an ordinary attachment.
- The draft page has a read-only cookie-authorized, no-store phase endpoint.
  JavaScript polls only while the page is visible, announces phase changes,
  reveals Review/Retry when valid and stops at terminal states. Manual Refresh,
  forms and preview continue to work without JavaScript. A browser test caught
  and fixed a CSS specificity error that exposed the hidden Review link while
  processing was still underway.
- The JavaScript form enhancement now displays native upload progress and a
  distinct server-save phase, with a labelled progress meter and polite live
  text. It uses the browser's File-backed multipart body rather than reading
  the video into JavaScript memory. Errors focus a visible alert and remind
  the author to check Recent drafts before retrying after an uncertain network
  failure. The unmodified HTML multipart POST remains the no-JS path.
- Final verification of this source: all-target Clippy clean; expanded Rust
  selection **1,220/1,220 passed** with no skips; full canonical-origin
  accessibility/browser matrix **45/45 passed** across Chromium, Firefox,
  WebKit desktop/mobile and Chromium without JavaScript. The browser run
  includes a delayed-transfer check of the active progress UI at narrow width.
  Report: `target/accessibility/ap09-upload-progress-final.json` (`passed`,
  `source_dirty=true`). A preceding 44/45 run exposed only a stale browser
  assertion that assumed a retried job could not finish before Refresh;
  the final run allows valid queued → processing → ready advancement.
- Real browser upload-to-publication and draft-recovery cases passed **14/14**
  in the canonical-origin disposable fixture across Chromium, Firefox and
  WebKit desktop/mobile configurations plus Chromium without JavaScript.
  This covers real encode, caption playback, visual description, keyboard
  controls, layout/reflow, private review and explicit publication. Report:
  `target/accessibility/ap09-upload-queue-full.json` (`source_dirty=true`).
  The first two runs failed and were corrected: the recovery fixture's 14
  drafts exceeded the initial four-draft policy, then a hidden Review link was
  styled visible. The final passing run is the evidence, not those failed runs.
- At startup, only recognized `video-upload-*` files and `av-*` directories in
  the media scratch directory are scavenged; unrelated entries and symlinks
  are untouched. A worker now keeps new jobs queued while any legacy storage
  size remains unmeasured, instead of claiming and failing a job merely
  because safe output admission is temporarily closed. Recovery retries once
  per minute under the existing media-row fence. Regression tests cover the
  scratch-name filter and an active legacy writer blocking a new claim.
- The HTTP media route already denied `.orig` keys; the shared database file
  gate now also denies them as defense in depth. A subsequent browser rerun
  passed **7/7** upload/publication cases while explicitly requesting each raw
  original before and after publication and receiving 404 both times. Report:
  `target/accessibility/ap09-upload-privacy.json`. The owner-only draft phase
  endpoint has a dedicated signed-in/stranger/anonymous web regression.
- Backed up the local standing development database to private
  `ap09-runtime-trt7qd/plamenu-dev-before-m2-v11.dump` (custom archive listed;
  full restore still not rehearsed), then started the v11 candidate on the
  existing `plamenu.local` configuration. Migration reached schema **90** and
  `/ready` answers 200. PeerTube reports **8.3.0**. The combined real-peer set
  passed **6/6** in 110.85 seconds, no skips: prior API-originated outbound
  Video, inbound VOD/history/live, plus a new first-party multipart upload →
  durable worker → private review → publication → PeerTube import/range/decode
  case. Its test checks the raw original is 404 before and after publication.
  Report: private `ap09-runtime-trt7qd/peertube-v11-full.xml`. Federation
  coverage checker reports no violations. This local instance is not staging.
- This is one M2 integration slice, **not** AP09 completion. Resumable uploads,
  long-form rendition ladders/storyboards, round-one live camera/screen authoring,
  PeerTube-compatible default-on P2P with opt-out, and several platform/API
  surfaces in the inventory remain open. This verified foundation was committed
  as `5ddc1629` on `feat/ap09-native-video`; no AP09 PR, merge or staging
  deployment has been made. Only the authorized local development instance
  was updated.

### 2026-09-23 — Resumable authoring follow-up (in verification)

- Migration 0091 adds an optional first/last-64-KiB SHA-256 file fingerprint
  to receiving drafts. A JavaScript-enabled browser now reserves the complete
  file size, sends 8 MiB chunks with explicit offsets, and resumes a private
  draft after the same local file is reselected. The server fsyncs each chunk
  before committing its offset and trims an uncommitted tail on retry.
- A read-only owner status endpoint and CSRF-gated chunk/finalize endpoints
  keep the original private; finalization checks the actual stored length and
  fingerprint before queuing processing. ALT text, transcript and captions
  enter the reservation transaction, avoiding a metadata-loss crash window.
- The ordinary HTML multipart upload remains the no-JavaScript path; the
  receiving page keeps Refresh/Cancel links and adds a JS-only resume form.
  The file fingerprint is a bounded first/last-block accidental-mismatch
  check, **not** a full-file cryptographic integrity claim.
- Initial Chromium browser runs failed in two useful ways: JSON numeric
  snowflake IDs lost precision in JavaScript, yielding a false 404 after
  upload initiation; and the resume button was parsed inside an empty hidden
  alert paragraph. IDs are now decimal strings and the alert has an explicit
  empty body. The passing run is `target/accessibility/ap09-resumable-chromium-final.json`.
- The full isolated HTTPS browser/accessibility matrix passed **51/51** in
  3.8 minutes, no skips: Chromium, Firefox and WebKit desktop/mobile plus
  Chromium without JavaScript. Each JS configuration performs a valid video
  upload, private review/publication and an interrupted 9 MiB upload resumed
  from its committed 8 MiB offset after a wrong-file rejection. The no-JS
  browser performs the ordinary multipart upload, caption playback, review
  and publication. Report: `target/accessibility/ap09-resumable-full.json`
  (`status=passed`, `source_dirty=true`). The isolated browser suite is not a
  real PeerTube federation run for this follow-up; the previous 6/6 peer
  matrix applies to the committed foundation only.
- Strict workspace/all-target Clippy passed after the chunk/JSON and markup
  fixes. The final database-backed Rust regression passed offset, CSRF,
  ownership, overrun, incomplete/mismatched finalization and private-original
  checks after recreating the router and disk store mid-upload. This exercises
  persisted recovery but is not a kill/restart/load benchmark.
- Local `plamenu.local` now runs the v12 candidate on schema **91**, with the
  prior v11 executable preserved in the private AP09 runtime directory.
  `/ready` responds 200. The real PeerTube **8.3.0** matrix passed **7/7** in
  125.12 seconds, no skips: outbound native Video with early/late follows,
  first-party multipart **and resumable** draft upload → worker → explicit
  publication → PeerTube import/range/FFmpeg decode, plus inbound VOD,
  collection revalidation and live. The resumable case checks its source
  offset after both chunks, ALT retention and raw-original 404. Private report:
  `ap09-runtime-trt7qd/peertube-v12-full.xml`. Federation coverage checker
  reports no violations. This is a local instance, not staging.
- VOD02 remains partial: the browser matrix uses a 9 MiB interrupted source,
  while large-file throughput and concurrent-upload pressure remain unmeasured.
  No AP09 PR, merge or staging deployment has been made.

### 2026-09-23 — Disposable process-crash recovery check

- Added a reusable E2E fixture restart that sends SIGKILL to only its own
  Plamenu server, retains its disposable database/media directory and starts
  the same binary again. It does not touch the standing local instance.
- The new test reserved a 64 MiB + 123-byte source, committed the first 8 MiB
  chunk, killed and restarted the fixture server, read the same offset and
  fingerprint, sent the remaining bounded chunks, and finalized. Raw `.orig`
  requests returned 404 before/after completion; the unpublished draft was
  cancelled. **1/1 passed** in 24.80 seconds, no skips. Private JUnit report:
  `ap09-runtime-trt7qd/resumable-crash-v12.xml`.
- This validates persistence and process restart for one upload. It does not
  benchmark maximum-size transfers, concurrent pressure, or output encoding
  of the synthetic bytes (which are intentionally not video).

### 2026-09-23 — Native adaptive HLS delivery slice

- Added a copy-only full-resolution single-file fMP4 HLS rendition and a
  bounded 360p re-encode when source height exceeds 360p. Segment byte ranges
  share immutable stored files; the ordinary MP4 remains a no-JavaScript and
  attachment-only-client fallback. The master advertises measured peak
  segment bandwidth rather than whole-file average.
- Migration 0092 stores transactional playlist metadata and links each fMP4
  body to the existing per-attempt storage reservation and cleanup journal.
  Final files, media completion, renditions and draft readiness commit under
  the same generation fence. A master request reads only ladder metadata, not
  every potentially large per-rendition playlist.
- Draft HLS playlists and fMP4 files return 404 until publication; both return
  404 again after owner suspension. A published 206 byte-range response and
  client API `hls.master` extension pass an isolated HTTP regression.
- Browser verification found that the existing HLS source switch cancelled
  an external WebVTT load in Chromium. The player now reattaches the external
  track after HLS binds, preserving the user's selected caption mode. The
  **13/13** native-video browser matrix passed across Chromium, Firefox and
  WebKit desktop/mobile and Chromium without JavaScript, including decoded
  playback, active captions, the two quality choices, and the progressive
  fallback. Evidence: `target/accessibility/ap09-native-hls-full.json`.
- The 11 native-video and 13 worker integration tests pass, including real
  FFmpeg packaging, failure/retry/quota/cleanup, a cancelled finalizer and
  publication/suspension HTTP gates. The peak-bandwidth unit regression,
  SQLx metadata check and strict workspace/all-target Clippy also pass.
  The complete isolated browser/accessibility matrix passed **51/51** with no
  skips; evidence: `target/accessibility/ap09-native-hls-all.json`.
- After browser verification, a private `pg_dump -Fc` snapshot of the
  standing local database was list-verified (not restore-tested). The standing
  `plamenu.local` process moved from v12 to v13 on schema **92**, with the
  previous executable preserved; `/ready` answered 200 after the switch.
  The real PeerTube **8.3.0** matrix passed **7/7** in 127.32 seconds, no
  skips: outbound Video creation/history/update/delete, first-party multipart
  and resumable draft publication/import/decode, inbound VOD/history and live.
  Private JUnit report: `ap09-runtime-trt7qd/peertube-v13-full.xml`.
  Federation coverage audit reports no violations (the local-only resumable
  crash test is explicitly excluded from directional pairing).
- VOD05 is **partial**, not complete. The 360p pair is not a broad ladder;
  PeerTube still receives the independently playable MP4, not an advertised
  HLS playlist. Seek/rebuffering, bandwidth under load, longer/odd inputs,
  quality switching and real PeerTube HLS import remain to be verified.

### 2026-09-23 — PeerTube receives the native HLS ladder

- The canonical Video now includes a PeerTube-compatible
  `application/x-mpegURL` master Link with tagged rendition file Links. The
  independently playable MP4 stays in `url` for attachment-only clients.
  Create and dereferenced/updated objects use the same batched metadata,
  without pulling playlist bodies or adding a query per video.
- Real PeerTube **8.3.0** imported both 640×480 and 480×360 fMP4 renditions
  from each first-party multipart and resumable publication. The E2E test
  fetched both media playlists and byte-range segments with a PeerTube Origin,
  checked CORS, and decoded the lower file with FFmpeg. Targeted result:
  **2/2**. Full inbound/outbound/live matrix on local v14: **7/7** in 124.31
  seconds, no skips. Private reports: `ap09-runtime-trt7qd/peertube-v14-hls.xml`
  and `ap09-runtime-trt7qd/peertube-v14-full.xml`.
- The local `plamenu.local` process is v14 on schema 92, with v13 and v12
  executables retained; `/ready` answers 200. No new migration or required
  installation step was introduced by this federation follow-up.
- The native-video and worker Rust suites pass **24/24**; strict workspace
  all-target Clippy and SQLx metadata checks pass. The preceding local HLS
  slice's complete browser matrix was **51/51**. This follow-up changes the
  federated object, not the first-party player.
- VOD05 remains partial: only two quality levels at most, no PeerTube UI
  playback observation for this new outbound path, no P2P tracker/segment
  interoperability, and no multi-viewer/seek/rebuffering benchmarks. The
  original broader AP09 acceptance inventory is still open.

### 2026-09-23 — PeerTube's actual watch player

- Added a browser regression inside the outbound first-party publication E2E,
  while its temporary remote Video still exists. A fresh browser signs in as
  the disposable PeerTube 8.3 test user (this local peer restricts anonymous
  remote viewing), acknowledges PeerTube's P2P privacy notice, and uses its
  real Video.js/HLS.js watch page. Chromium and Firefox on desktop and at
  320px all decoded the 640×480 video, exposed 480p and 360p in PeerTube's
  quality menu, switched to a decoded 480×360 frame, and sought through the
  player's progress bar. Both HLS playlists and 206 fMP4 ranges were fetched
  by the browser. The captured Chromium frame visibly contains the synthetic
  test pattern: `target/e2e/peertube-native-hls-frame.png`.
- The four local navigation-to-decoded-playback observations were 790–851 ms
  for this 5-second 640×480 fixture, including PeerTube UI/consent work. These
  are **smoke measurements**, not startup SLOs or a cold-cache/production load
  benchmark. No multi-viewer, long-input, rebuffer, CPU/RAM, origin-byte or
  WebKit claim follows from them. PeerTube's installed chat plugin returned a
  404 for its unrelated room API; the core video requests and decoding passed.
- The real-peer suite passed as two bounded runs: **6/6** VOD/history/browser
  cases in 83.02 seconds and **1/1** inbound live in 66.02 seconds. Private
  JUnit reports: `ap09-runtime-trt7qd/peertube-v14-browser-vod.xml` and
  `ap09-runtime-trt7qd/peertube-v14-browser-live.xml`. An earlier combined run
  was externally terminated during live and produced no JUnit; it is not
  counted as a complete pass. The browser test initially exposed anonymous
  follow restrictions and test-harness assumptions about autoplay, responsive
  login and cancelled quality-switch requests; all are now handled explicitly.
- This confirms watch-page playback for the small two-rung fixture, but VOD05
  remains **partial**. Next: representative longer videos, difficult input
  shapes, concurrent-viewer bandwidth/latency benchmarks, then a broader
  quality ladder and seek previews. PeerTube-compatible P2P and native live
  authoring remain open.

### 2026-09-23 — audio-only native Video and difficult-input coverage

- The first-party native Video form now accepts audio-only files in both its
  ordinary HTML upload and JavaScript-enhanced resumable upload. A bounded
  FFmpeg job generates a 640×360, 2 fps H.264 waveform with AAC sound and an
  AVIF poster; its HLS and progressive MP4 are published as a Video. This does
  **not** change ordinary audio attachments, which remain Audio/MP3. The form
  explains the conversion and still offers a visual description, transcript
  and WebVTT captions. No new runtime dependency or installation step.
- The real-encoder native worker suite passes **16/16**, including audio-only
  duration/poster/HLS and silent portrait/ultrawide jobs. The ordinary audio
  attachment API regression passes **1/1** against the corrected v16 local
  server. Strict workspace/all-target Clippy and Rust formatting pass.
- A real first-party audio-only draft is reviewed, published and imported by
  PeerTube **8.3.0** with the expected one-rung HLS and range-accessible MP4.
  PeerTube's actual watch player decodes and seeks it on Chromium and Firefox,
  desktop and 320px mobile. The four-context browser test now logs in once;
  earlier per-context authentication hit the local PeerTube API rate limit.
  Targeted E2E: **1/1** on v16; private JUnit:
  `target/e2e/peertube-v16-audio-browser.xml`. The captured watch frame is
  `target/e2e/peertube-native-audio-frame.png`.
- The audio-only first-party browser journey passes on Chromium/Firefox
  desktop and mobile and Chromium without JavaScript (**5/5**). No-JS Axe
  injection is inapplicable, so the no-JS test uses actual form, disclosure,
  progressive decoder and reflow interactions; JS runs have Axe checks.
  The pinned isolated browser container passes the complete native-video
  matrix **20/20**, including WebKit desktop/mobile and existing upload/edit/
  resume checks. Host-local WebKit could not launch because this host lacks
  its libraries; the container result, not that failed host diagnostic, is
  the cross-engine evidence. Report: `target/accessibility/automated.json`.
- The local `plamenu.local` process is v16 on schema **92** with v15 and older
  executables preserved; `/ready` answers 200. This verifies a meaningful
  portion of VOD07, not all unusual codecs/containers, long audio durations,
  assistive-technology behavior, or the full M2 milestone. Replacement
  posters, storyboards and representative stress remain open.
- The bounded PeerTube VOD/history/publication and federation-coverage run on
  v16 passes **8/8**, with inbound live intentionally split into a separate
  run. This covers the existing multipart/resumable video routes and the new
  audio-only multipart path. Private JUnit:
  `target/e2e/peertube-v16-vod.xml`.
- The separate inbound PeerTube live run passes **1/1** in 67.02 seconds;
  private JUnit: `target/e2e/peertube-v16-live.xml`. This protects an existing
  consumption path and does not complete native live authoring.
- Both audio-only multipart and audio-only resumable drafts pass final
  PeerTube import checks **2/2** after correcting their authored visual
  descriptions to describe the waveform. Private JUnit:
  `target/e2e/peertube-v16-audio-final.xml`.

### 2026-09-23 — creator-selected initial posters

- Native Video authors can now choose an initial poster frame before uploading.
  The HTML form accepts a numeric time with JavaScript disabled; JavaScript
  adds a local video preview, native scrubbing controls and an accessible
  “Use this frame” action. The private review displays the generated poster
  and the actual selected time. Audio-only waveform Videos retain the numeric
  option. Both multipart and resumable uploads persist the time in the durable
  draft; existing drafts default to the historical first frame.
- The worker seeks in the delivered MP4, generates its AVIF poster at the
  bounded time, and the existing compatibility path derives PeerTube's JPEG
  thumbnail from that AVIF. A requested time past EOF is clamped to a
  decodable position and that actual time is saved for review. Ordinary media
  attachments retain their previous first-frame processing path.
- Real-encoder worker regressions pass **18/18**, including a red-to-blue
  frame-selection fixture and a past-EOF selection. The isolated first-party
  browser matrix passes **20/20** across Chromium, Firefox and WebKit desktop/
  mobile plus Chromium without JavaScript. A separate live-browser visual
  inspection found the picker and action reachable at desktop and 320px width.
- PeerTube 8.3 imported the authored blue thumbnail and decoded it as blue;
  the targeted import passes **1/1**, and the native-video publication/upload
  matrix passes **7/7**. Private JUnit reports:
  `target/e2e/peertube-v17-poster.xml` and
  `target/e2e/peertube-v17-native-video.xml`. Strict all-target Clippy and
  SQLx prepared-query metadata checks pass.
- Before schema **93**, a custom PostgreSQL backup was saved at
  `ap09-runtime-trt7qd/plamenu-before-poster-time-v17.dump` and its archive
  listing validated; no restore drill was performed. The local `plamenu.local`
  v17 candidate answers `/ready` with 200, with v16 preserved. The v16
  shutdown exited normally, but its streaming worker reached the drain
  deadline and was aborted; this is not a claim of a clean worker drain.
- VOD06 is still **partial**. The initial poster can be selected at authoring;
  replacement after upload/publication, an uploaded custom poster and seek
  previews/storyboards are not yet implemented. No additional installation
  dependency was introduced.

### 2026-09-24 — uploaded poster revisions for unpublished Videos

- A ready, unpublished native Video now has an HTML-first “Replace poster”
  form on its private review page. A required 1–500-character description
  accompanies a still-image upload of at most 8 MiB. JavaScript adds a local
  image preview, while the same form submits without JavaScript. The review
  links directly to the form; published Video playback exposes the authored
  poster description in a visible disclosure and the client API exposes it as
  `poster_description` on Video media attachments. ActivityPub `icon.name`
  carries the description to compatible peers.
- Replacement encodes bounded AVIF and PeerTube-compatible padded JPEG files
  using the native image pipeline. Each revision has immutable storage keys,
  durable quota reservation and cleanup journaling. The ready-draft/media-row
  fence protects the pointer swap; failures leave the old poster intact, and
  old bytes are refunded only after acknowledged cleanup. An interrupted HTTP
  caller cannot abandon reserved writes. No companion software or install
  step was added.
- Migration **94** was applied to the standing local instance after a private
  custom-format backup at
  `ap09-runtime-trt7qd/plamenu-before-poster-replacement-v18.dump`;
  `pg_restore --list` passed, but a restore drill has **not** been performed.
  The local v18 candidate answers `/ready` with 200. It supersedes v17 on
  this schema; the v17 executable does not recognize migration 94.
- Real-encoder worker and security regressions: **21/21 passed**. PeerTube
  8.3 native-video E2E: **8/8 passed**, including a decoded blue uploaded
  poster and its federated description. Reports:
  `target/e2e/peertube-v18-custom-poster.xml` and
  `target/e2e/peertube-v18-native-video.xml`. Live browser inspection found
  the review form reachable and free of horizontal overflow at 320px. The
  isolated HTTPS native-video browser matrix passes **20/20** across desktop/
  mobile Chromium, Firefox and WebKit plus Chromium without JavaScript; it
  exercises uploaded replacement, its local JS preview, published visible
  description and the unchanged audio-only/resumable flows.
- VOD06 remains **partial**. Poster replacement is limited to ready,
  unpublished drafts; published Video editing, seek previews/storyboards and
  more representative long-duration validation are still owed. The broader
  M2 milestone is not closed.
- The final local v19 build (same schema 94) replaced v18 and answers `/ready`
  with 200. v18 exited after TERM, but its streaming worker reached the drain
  deadline and was aborted; do not treat that shutdown as a clean worker drain.

### 2026-09-24 — native live RTMP/HLS vertical slice

- Migration **95** adds fileless waiting live media, a persistent broadcast
  identity and hashed one-time ingest key, plus independent session IDs across
  reconnects. Claiming is single-publisher and generation-fenced; a process
  restart closes orphaned sessions before accepting new publishers. The
  private pre-migration backup is
  `ap09-runtime-trt7qd/plamenu-before-native-live-v20.dump`; its archive
  listing passed, but a restore drill has **not** been performed.
- First-party HTML forms create a waiting Video in an owned channel and reveal
  the RTMP key once; management and key rotation remain available without
  JavaScript. The default listener is loopback-only and uses the existing
  FFmpeg binary, so no companion service or install step is added. RTMP is
  plaintext, and secure remote ingest remains open work.
- The built-in RTMP publisher accepts H.264/AAC FLV, rejects unknown keys and
  competing publishers, and remuxes once into a six-segment sliding HLS
  window. The local HLS route fences each segment to the currently published
  session; the existing shared MP4 live gateway covers clients and no-JS
  playback. Bounded scratch is removed after a stream drains, and startup
  scavenges crash-abandoned session directories. Stopping the encoder ends a
  one-shot Video, removes playback URLs
  and federates the change. PeerTube requires an image icon even while
  waiting and an HLS Link with an array-valued `tag` once on air; both are now
  emitted, with a bundled local placeholder image before any authored poster.
- The real PeerTube 8.3 outbound native-live test passes **1/1** in 18.81
  seconds: no-JS creation, invalid-key rejection, real RTMP A/V, MPEG-TS
  playback, fMP4 init bytes, PeerTube on-air HLS availability and ended-state
  revocation. The existing inbound PeerTube live regression passes **1/1** in
  67.42 seconds. Chrome desktop and 390px mobile visual inspections covered
  the creation form, key page and management layout; no horizontal overflow
  was observed. These observations are not a full browser accessibility matrix.
- The FLV serializer, crash-scratch fence and HLS path traversal tests pass
  **3/3**. Strict workspace Clippy, formatting and SQLx metadata checks pass.
  The schema-95 local build answers `/ready` with 200. One disposable waiting
  broadcast created during browser inspection remains on the local instance
  for review; it is not part of the E2E fixture.
- M5 stays **partial**. Browser camera/screen broadcasting, RTMPS or another
  protected remote ingest path, creator posters, scheduling, interruption and
  reconnection UX, latency/DVR, replay, P2P default-on/opt-out and wider
  browser/performance tests are not yet done. The next substantive slice is
  browser broadcasting over the existing HTTPS origin, reusing these stable
  broadcast and session identities.

### 2026-09-24 — browser camera/screen native live authoring

- The same persistent broadcast can now be published from the first-party
  manage page with camera/microphone or screen capture. Capture is a progressive
  enhancement: HTML-only creators still receive the RTMP key and can manage
  and rotate it. The owner-authenticated, CSRF-checked, same-origin WebSocket
  claims a session without exposing the RTMP key to JavaScript. Encoded
  MediaRecorder chunks feed the same bounded HLS pipeline; WebM VP8/Opus is
  transcoded once to H.264/AAC for all viewers, while compatible MP4 can be
  remuxed. The UI exposes an on-page preview, live status and stop control;
  browser capture is necessarily unavailable when scripts are disabled.
- Isolated HTTPS Chromium tests pass **3/3** across desktop, 390px mobile and
  no-JavaScript projects: real browser MediaRecorder and WebSocket streaming
  of synthetic camera *then screen* on one reusable broadcast, MPEG-TS
  segment playback for each, axe scans of the controls and no mobile
  horizontal overflow; the no-JS project verifies creation, management,
  stream-key handling and RTMP fallback. These tests exercise encoded
  browser/network/media I/O, but synthetic tracks do not prove physical
  device permissions or screen-picker UX. Firefox, WebKit, assistive
  technology and performance/load evidence remain open.
- The Videos page lists recent owned broadcasts so the management page is
  reachable after the initial one-time key reveal. No new installed service
  or operator configuration is needed for browser publishing.
- M5 remains **partial**: secure off-box ingest, scheduling, interruption and
  reconnection UX, latency/DVR, replay, P2P, wider browser/device tests and
  resource measurements are not done. The next high-impact slice should
  address protected remote ingest and reconnect/interruption behavior, not
  cosmetic post-publication editing.

### 2026-09-24 — HTTPS-FLV encoder ingest without a companion service

- Remote FFmpeg-compatible encoders can POST FLV to `/live/ingest` over the
  existing HTTPS origin, sending the one-time stream key only as a Bearer
  header. The backend accepts an HTTPS assertion only from a configured
  trusted reverse proxy; it rejects direct HTTP lacking that assertion and
  untrusted peers spoofing it. The request body streams through a bounded
  queue into the same single-remux HLS/MP4/session lifecycle as RTMP and
  browser publishing. No new listener, package, or install step is required;
  a non-loopback reverse proxy already needs an explicit `trusted_proxies`
  entry, as elsewhere in Plamenu.
- The key page now shows both the local RTMP endpoint and HTTPS-FLV endpoint,
  with transport guidance. Plain RTMP stays local/trusted-network-only by
  default. HTTPS-FLV is an encrypted off-box option, **not** a claim of
  native RTMPS protocol compatibility with existing RTMP-only encoders.
- The real PeerTube 8.3 test passes **2/2** for RTMP and HTTPS-FLV, each
  covering waiting creation in no-JS HTML, unauthorized-key rejection, actual
  H.264/AAC ingest, local HLS MPEG-TS, client-compatible MP4, PeerTube on-air
  HLS and ended-state revocation. Direct HTTP without TLS-proxy metadata is
  rejected. The isolated browser capture matrix still passes **3/3** after
  the key-page change. Strict workspace Clippy and the focused proxy-trust
  Rust test pass; formatting and diff checks pass as final gates.
- M5 remains **partial**. Native RTMPS/encoder compatibility, abrupt
  disconnect/reconnect UX, scheduling, DVR/replay, P2P and resource/load
  evidence still need implementation and verification.

### 2026-09-24 — durable interrupted/reconnect lifecycle

- Migration **96** adds a persisted reconnect deadline and permits
  `interrupted` on media attachments. An unexpected RTMP, HTTPS-FLV or browser
  publisher loss withdraws HLS/MP4 URLs immediately, exposes an interrupted
  state, and permits a new session on the same Video for one minute. Deliberate
  stop still ends a one-shot stream or returns a recurring one to waiting.
  A new generation revokes old HLS paths. A five-second worker resolves
  expired one-shot streams to ended and recurring streams to waiting, including
  deadlines left across a restart. Startup marks orphaned active sessions
  interrupted and announces the change; a destructive crash/restart drill has
  **not** yet been run, so that recovery claim remains implementation-only.
- The first-party HTML management and status views describe interruption and
  recovery without JavaScript; the browser broadcaster announces a lost
  connection and allows retry. The off-air view no longer renders a video
  element with a missing source. The isolated HTTPS Chromium suite passes
  **4 applicable tests**, with one intentionally skipped project case:
  desktop/mobile camera and screen ingest, desktop forced WebSocket loss and
  same-Video retry, and no-JS authoring.
- The real PeerTube **8.3** native-live suite passes **6/6** in **332.29 s**:
  normal RTMP and HTTPS-FLV lifecycles plus one-shot and recurring reconnects
  for both transports. It kills real FFmpeg publishers, verifies interrupted
  API/HTML and HLS revocation, PeerTube off-air updates, a new generation and
  HLS path on reconnection, PeerTube on-air updates, actual one-minute one-shot
  expiry/ended federation, and deliberate recurring stop. This is functional
  evidence, not a concurrency/latency benchmark.
- Before applying migration 96 to the local instance, a custom-format backup
  was written to `ap09-runtime-trt7qd/plamenu-before-live-reconnect.dump` and
  its archive listing verified. A restore drill remains open. The migrated
  local instance answers `/ready` with 200.
- M5 remains **partial**. Native RTMPS protocol compatibility, a crash/restart
  recovery drill, scheduling, latency/DVR, replay, P2P and resource/load
  measurements are still required. The next substantial slice should address
  RTMPS or replay/recording, not cosmetic management controls.

### 2026-09-24 — process-crash recovery and native RTMPS ingest

- Backed up the local database to
  `ap09-runtime-trt7qd/plamenu-before-live-crash-drill.dump` and verified its
  `pg_restore --list` archive. An opt-in destructive E2E test validates the
  *exact* local process/config/binary before SIGKILL. It restarts the candidate,
  checks the surviving Video becomes interrupted, the old HLS generation is
  revoked, and the same key reconnects into generation 2. **1/1 passed** in
  23.61 seconds. The database restore drill is still open.
- Optional `[live_rtmps]` serves TLS natively in Plamenu and forwards decrypted
  RTMP to the existing bounded native listener: no companion service, proxy,
  storage copy or re-encode. TLS certificate parsing and key matching are checked at server
  startup; a failed renewal keeps the previous certificate and retries every
  minute. The local certificate fingerprint changed on the TLS port without a
  Plamenu restart after replacing both certificate and key. Plain RTMP remains
  loopback-only by default; HTTPS-FLV still uses the regular HTTPS origin.
- The no-JS key page exposes a labelled RTMPS Server field alongside RTMP,
  stream key and HTTPS-FLV. Chrome visual/accessibility inspection confirmed
  the field is visible and named on a disposable unlisted broadcast. The
  Chromium JavaScript-disabled authoring test passes **1/1** at a **320px**
  viewport against the local RTMPS-enabled instance; it checks the server field
  before the key disappears and rejects horizontal overflow.
  The server config template and host deployment guide explain opt-in and cert
  access; installations without RTMPS acquire no new listener or steps.
- Real FFmpeg→Plamenu→PeerTube **8.3** E2E: RTMPS normal publish, unknown-key
  rejection, local HLS/no-JS MP4, PeerTube live and ended updates **1/1** in
  18.69 seconds; RTMPS one-shot and recurring interruption/reconnect **2/2**
  in 94.10 seconds; plaintext RTMP and HTTPS-FLV normal regression **2/2** in
  95.12 seconds and their one-shot/recurring reconnect regression **4/4** in
  248.85 seconds. A further final-build RTMPS run with FFmpeg verifying the
  rotated local certificate against its CA file passed **1/1** in 18.17
  seconds, and both certificate-verified reconnect variants passed **2/2** in
  93.12 seconds. These are protocol/lifecycle tests, not resource or latency
  benchmarks.
- The final local process was stopped and replaced with a detached candidate;
  `/ready` returned 200, the native RTMPS port listened on loopback, and an
  OpenSSL client verified the rotated certificate with IP SAN against the
  local certificate file. One shutdown logged an existing `streaming` worker
  drain timeout and aborted that worker after ten seconds, despite no active
  native live sessions; investigate its shutdown path separately.
- Browser regression runs filled the fixture account's 50-channel limit and
  then its ten non-ended-broadcast slots. The browser test now reuses an
  existing owned channel, creates one only for a new account, publishes
  unlisted, and deletes its own test broadcast in teardown. We removed the ten
  exact disposable test broadcasts left by earlier runs; the remaining
  non-ended count returned to zero. A second regression exposed optimistic
  "Broadcast stopped" UI after reconnect while the API entered `interrupted`.
  The browser now waits for the server stop acknowledgment. That exposed the
  actual lifecycle edge: FFmpeg can exit nonzero on a deliberately stopped,
  extremely short browser recording before it produces a segment. The server
  still logs that failure, but an acknowledged deliberate stop now takes
  precedence over remux success when choosing waiting/ended versus interrupted.
  Strict Clippy and the new focused stop-reason unit test pass. The complete
  local Chromium live-browser matrix passes **4 applicable tests**, with the
  mobile socket-loss case intentionally skipped: desktop/mobile camera and
  screen, desktop forced socket loss and reconnect, and 320px no-JS authoring.
  Teardown left zero non-ended developer test broadcasts. A final
  certificate-verified FFmpeg→Plamenu→PeerTube RTMPS smoke test passed **1/1**
  in 18.67 seconds after the stop-state change.
- M5 remains **partial**. Scheduling, DVR/latency choices, replay capture,
  active-session revocation, P2P and resource/load measurements remain.

### 2026-09-24 — bounded one-shot replay and PeerTube VOD conversion

- One-shot RTMP, RTMPS, HTTPS-FLV and browser broadcasts now tee the *same*
  encoded packets to a bounded recording while the six-segment HLS window
  remains sliding. Compatible encoder input is remuxed rather than encoded
  again. A clean stop finalizes a progressive MP4 and padded JPEG poster;
  duration, dimensions and byte counts become an ordinary Video attachment
  under the original stable status/Video identity. The session records its
  replay-media relationship. An interrupted final generation can finalize
  after the reconnect deadline, but an actual reconnect fences off the older
  partial recording. Recurring broadcasts still return to waiting without a
  saved replay.
- The recording checks the configured video byte cap and media free-space
  floor while ingesting. Processing reserves a bounded worker slot and the
  exact resulting MP4/poster bytes against video storage quotas. A database
  cleanup journal covers files written before publication; the replay and
  session link publish atomically only after both files are stored. A clean
  stop sends one federated final Video Update, avoiding competing ended/VOD
  updates. PeerTube 8.3 requires dereferenceable Video `likes` and `dislikes`
  collection URLs and a JPEG icon for this import, so those contract details
  are supplied. The RTMP dependency's INFO logs contained stream keys; its
  log level is now forced to WARN regardless of broader filter directives.
- An early full-matrix run was interrupted after exposing a deployment race:
  rebuilding/replacing the running executable made `current_exe()` resolve
  to a deleted pathname, so its bounded FFmpeg helper could not start. Both
  replay and VOD workers now re-exec the live executable through Linux
  `/proc/self/exe` where available, with the former path as a fallback on
  other hosts. A replacement-while-running drill verified the old process's
  executable was marked `(deleted)` while it remained ready, and all **9/9**
  FFmpeg→Plamenu→PeerTube 8.3 cases passed under that condition.
- The first-party creator page offers a replay link or a processing message;
  the status page presents native video controls, and the client API exposes
  the replay MP4 as a normal attachment with HTTP Range support. Scripts-off
  users can follow the link and play it. The isolated Chromium live suite
  passes on desktop, mobile and 320px no-JS: **7 passed, 1 intentionally
  skipped**. This includes actual browser-camera replay, an external FFmpeg
  publisher and no-JS native playback controls. Browser testing found a real
  narrow-width reply-composer overflow; fixed grid/file-picker/language-combo
  sizing and verified no horizontal panning beyond one rounding pixel.
- Local backup before the candidate switch:
  `ap09-runtime-trt7qd/plamenu-before-live-replay.dump` (archive listing
  verified). A restore drill is still open. The local PeerTube 8.3 native-live
  regression passed **9/9 in 478.43 s**. The three normal-stop transports
  covered replay MP4/JPEG/duration, byte ranges, client API, HTML video,
  actual FFmpeg decoding of PeerTube's imported file and HLS revocation.
  Six one-shot/recurring reconnect variants covered interruption, generation
  fencing, reconnected playback and either replay after the deadline or the
  recurring return to waiting. Report: `target/e2e/live-replay-full-matrix.xml`.
  Strict workspace checks and SQLx metadata verification pass; the focused
  executable re-entry test passes **1/1**.
- M5 is still **partial**. The recording lives in process-local scratch and
  does not survive a Plamenu process crash. It does not merge earlier sessions
  across a reconnect; a final replay is of the last completed generation.
  Long-running one-shot streams stop at the configured size/free-space limit,
  not a full live-DVR design. Recurring replay, scheduling, latency modes,
  P2P, Firefox/WebKit and resource/load evidence remain open.

### 2026-09-24 — recoverable one-shot recording after hard process loss

- One-shot ingest now writes fragmented MP4: a front-loaded index and
  keyframe-bounded fragments mean a hard Plamenu process loss can leave a
  decodable prefix without a second live encode. Startup closes orphaned
  sessions as before, but retains only latest-generation, unpublished
  one-shot scratch. A recovery task waits through the real reconnect window,
  rechecks the generation/ended fence, waits for an orphaned FFmpeg child to
  finish writing, and passes the recording through the same bounded processor,
  exact storage quota and atomic Video publication as a clean stop. If a new
  publisher reconnects, the older recording is discarded, never published.
  Unexpected files and symlinks remain untouched by scratch scavenging.
- A hard-crash E2E drill used a real RTMP encoder, waited for committed video
  fragments, SIGKILLed only the verified local candidate server, restarted
  that binary, and verified interrupted/HLS-revoked state. After the deadline,
  the same Video became a ranged MP4 with duration, its session linked to the
  replay, PeerTube 8.3 imported the VOD, and the scratch was cleaned. The
  opt-in drill passed **1/1 in 99.35 s**; report:
  `target/e2e/live-crash-replay-drill.xml`. Normal fragmented-MP4 RTMP replay
  also passed **1/1** against PeerTube. The complete RTMP/HTTPS-FLV/RTMPS
  transport and one-shot/recurring reconnect regression passed **9/9 in
  684.00 s**; report: `target/e2e/live-crash-replay-regression.xml`.
  The desktop/mobile/320px-no-JS live
  browser matrix passed **7 applicable tests**, one intentional skip. Strict
  workspace checks passed, including the 18-case benchmark gate. After the
  final idempotent-journal change, the exact hard-crash drill passed again
  **1/1 in 99.33 s** (`target/e2e/live-crash-replay-final.xml`).
- Before SIGKILL, the local database was backed up to
  `ap09-runtime-trt7qd/plamenu-before-crash-replay.dump` (402 MiB; archive
  listing verified); a second verified backup preceded the final drill at
  `ap09-runtime-trt7qd/plamenu-before-final-crash-replay.dump`. The standalone
  recovery process was stopped and the
  managed local service returned to `/ready` 200. No migration or new
  companion service was required. A database *and media* restore drill remains
  open; a database dump alone is not a complete replay backup.
- This recovers a completed fragmented prefix, not an unwritten or corrupt
  fragment. A recording that cannot pass validation or quota admission is
  retained for inspection rather than falsely published or silently erased;
  automated retry/failure presentation and retention policy still need work.
  An interrupted finalizer's exact MP4/poster reservation and cleanup journal
  are reused idempotently on restart instead of double-charging the owner.
  Recurring broadcasts still have no replay: they need an independent,
  idempotently published Video for each session, preserving the stable live
  broadcast identity and avoiding a multi-file PeerTube compatibility hack.
  M5 remains **partial**; scheduling, DVR/latency, default-on compatible P2P,
  cross-browser and resource/load evidence remain open.

### 2026-09-24 — independently published recurring-session replay Videos

- Every broadcast session now writes one bounded fragmented MP4 alongside the
  sliding live HLS window after the single encoder/remux stage. A reusable
  broadcast keeps its original, stable live Video and returns to waiting on a
  deliberate stop. Each ended session can publish a distinct ordinary Video
  in the same channel, with its own MP4, JPEG poster, title and link back to
  the live post. The creator management page and public live permalink expose
  the published episode links as semantic, no-JavaScript HTML.
- The session-to-replay media link is durable before publication. Pending MP4
  and poster keys fail closed at the public file and media-id gates; generic
  Note/Video attachment paths cannot consume them. The episode claim and
  cleanup-journal cancellation join the Video/Create/outbox transaction. A
  reconnect or newer session does not invalidate an older recurring replay;
  a restart scans and resumes unpublished session recordings. The quota
  reservation and crash journal continue to account for exact stored bytes,
  including a retry after only one MP4/poster cleanup job finished.
- Verified against local PeerTube 8.3.0: the full RTMP/RTMPS/HTTPS-FLV live
  matrix passed **9/9 in 613.01 s** (`target/e2e/live-recurring-replay-full.xml`).
  For each recurring transport, PeerTube imported two independent replay
  Videos while the original live Video remained reusable. Client API
  attachments, HTTP Range, first-party playback and creator links were
  asserted. The subsequently added public permalink links passed the focused
  RTMP/PeerTube rerun (`target/e2e/live-recurring-replay-public-link.xml`).
  The opt-in recurring SIGKILL/restart drill passed
  **1/1 in 99.48 s** (`target/e2e/live-recurring-replay-crash.xml`), recovering
  the crashed first recording alongside the cleanly stopped second session.
  The desktop/mobile Chromium and scripts-disabled browser live matrix passed
  **8 applicable tests**, one intentional skip, including browser camera and
  screen session replays, decoded playback and a 320-pixel no-JS encoder flow.
  The targeted database regression verifies private-before-publication gates,
  repeated preparation, older-generation eligibility and published access.
- The disposable standing database was backed up before migration 0097 at
  `ap09-runtime-trt7qd/plamenu-before-recurring-replay.dump` and the archive
  listing was validated. This does not replace a media-directory backup.
- LIVE04–LIVE07 remain **partial**. This slice does not implement scheduling,
  live DVR/latency controls, default-on compatible P2P, automatic retries for
  permanently invalid/over-quota fragments, retention policy, captions or
  broader cross-browser and sustained-load evidence.

### 2026-09-24 — bounded live DVR and latency choices

- A creator can choose off/5/10/15/30-minute rewind and low/balanced/resilient
  latency before publishing or while a recurring broadcast is waiting. The
  settings are validated and stored with the live identity, fenced against
  edits during an active session, and announced as PeerTube-compatible
  `dvrWindow` and `latencyMode` on the federated Video. Browser ingest uses
  matching keyframe targets; external encoders still determine the actual
  keyframe cadence. One FFmpeg tee produces a bounded sliding HLS window and
  the same crash-recoverable replay MP4 without a new companion service.
- The first-party player uses the native seek range with a keyboard-usable
  return-to-live control on desktop and mobile. With scripts disabled, the
  progressive live MP4 remains playable, the creator settings remain usable,
  and the HLS playlist has a direct link for compatible players. Generic
  non-JS browsers still cannot seek a live fragmented MP4; a native HTML
  rewind interface is not claimed.
- Local PeerTube 8.3.0 imported the chosen DVR window and latency mode in
  the real RTMP, HTTPS-FLV and RTMPS matrix (**3/3 passed**). Each case
  verified that the HLS playlist grew beyond the former six segments and
  contained short segments, that segments decoded, and that the no-JS MP4
  fallback and replay still worked. Chromium desktop/mobile browser tests
  each passed live seeking, keyboard return-to-live, accessibility scanning
  and 320px reflow (**2/2**); the no-JS creator settings check passed
  (**1/1**). The DB regression passed (**1/1**) for owner-only idle edits
  and active-session rejection. Pre-migration database archive:
  `ap09-runtime-trt7qd/plamenu-before-live-dvr.dump` (listing verified).
- LIVE06 remains **partial**. The configured 30-minute maximum and FFmpeg
  sliding-list bound are in place, but a 30-minute eviction/retention soak,
  latency measurement under load, no-JS generic-browser rewind, P2P delivery,
  and multi-node storage are not yet verified or implemented.

### 2026-09-24 — creator-authored live schedule announcements

- A one-shot or recurring live can advertise an optional planned start from
  the accessible HTML form. Owners can move or clear it while waiting; the
  operation is CSRF- and ownership-checked, serialized with publisher claims,
  and produces a federated Video Update. Plamenu resolves `datetime-local`
  values with the member's saved IANA time zone and rejects dates less than
  one minute or more than one year away. The date is a public announcement,
  not an automatic encoder start or a restriction on early ingest. Going on
  air clears the spent date; a recurring broadcast may set another afterward.
- The client API includes `live.scheduled_at`, the public waiting tile and
  creator index render semantic `<time>` elements in the viewer's time zone,
  and ActivityPub publishes PeerTube's `schedules: [{startDate}]` shape. The
  RTMP/HTTPS-FLV/RTMPS E2E matrix passed **3/3 in 131.57 s**: a real PeerTube
  8.3.0 instance imported the waiting planned date, accepted the on-air
  clearing Update, and continued through HLS/MP4 playback and replay. The
  full desktop/mobile Chromium and scripts-disabled live browser matrix passed
  **10 applicable tests**, one intentional project skip. It covered
  create/change/clear, public time display, invalid-date rejection, 320px
  layout, camera/screen publishing, DVR, reconnect and replays. The database
  regression passed owner-only waiting edits and active-session rejection.
- LIVE05 remains **partial**: one planned date per local broadcast is supported,
  but multiple future dates, inbound PeerTube schedules, reminders and
  calendar subscriptions are not implemented. No new companion service or
  installation step was added.

### 2026-09-24 — inbound PeerTube live schedules and updates

- Remote live Videos now retain up to sixteen valid planned dates in a small
  attachment sidecar. An inbound schedule-only Update atomically replaces or
  clears those dates without recreating the media attachment or marking the
  authored post edited. The client API exposes all dates and the next future
  start; the public waiting tile shows that start and a native disclosure for
  multiple upcoming times. Past remote dates are not mislabeled as upcoming.
  No script is needed to open the disclosure.
- The real PeerTube 8.3.0 inbound live test passed **1/1 in 68.95 s**:
  two announced dates arrived, PeerTube changed them to one then cleared them,
  and the stream still moved through on-air playback and its ending state. A
  Chromium scripts-disabled 320px browser opened the two-date disclosure,
  showed no horizontal overflow and saved
  `target/e2e/remote-live-schedules-320.png`; the JS-enabled accessibility
  scan found no WCAG A/AA violations on that tile. The database was backed up
  before migration 0099 to
  `ap09-runtime-trt7qd/plamenu-before-remote-live-schedules.dump` and its
  archive listing was verified.
- LIVE05 remains **partial**: local creators still have one planned date per
  broadcast, and this is calendar metadata rather than reminders,
  subscriptions or automatic ingest start. Migration 0099 adds no companion
  process or installation step.
