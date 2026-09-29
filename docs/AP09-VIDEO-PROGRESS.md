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
| M1 | Channels and native publication | Channel/account lifecycle; authorized collaboration; real Plamenu Video accepted, playable and attributed correctly in PeerTube; edit/delete/follow exchange | verified 2026-09-29; see M1 sign-off below |
| M2 | Long-form VOD pipeline | Large/resumable and no-JS uploads; durable processing; quality ladder; poster/storyboard; audio-only/silent inputs; failures/restart/quota evidence | in progress |
| M3 | Accessible metadata | Caption upload/edit/delete and languages; native tracks and transcript; chapters; descriptions; both-direction federation and update retention | verified 2026-09-29 for the stated functional gate; see M3 sign-off below. Human AT/WCAG release evidence remains M9 |
| M4 | Playlists and library | Create/import/edit/delete; reorder; clip boundaries; ownership/visibility; duplicate and missing videos; PeerTube round-trip; keyboard/no-JS forms | verified 2026-09-29; see M4 sign-off below |
| M5 | Native live | Authenticated ingest including browser camera/screen; waiting/on-air/interrupted/ended/replay states; stable session identities for future chat; scheduling, recurring streams, latency/DVR; PeerTube playback and updates in both directions | verified 2026-09-25 for this stated functional gate; see the M5 sign-off below. Cross-node storage, additional device/browser combinations, and broader fault matrices remain in M7/M9, not silently counted as done |
| M6 | Creator/viewer experience | Discoverable video space; channels, subscriptions/search; drafts/scheduling and reminders; in-session capture-source switching; settings/imports; editing; embeds/downloads/history; desktop/mobile/browser evidence | in progress: live calendars/reminders, scheduled VOD publication, in-session browser camera ↔ screen switching, and searchable/followable HTML video and channel libraries verified; larger/remote channel matrices, settings/imports, history and browser/hardware evidence remain |
| M7 | Operations and scale | Worker/storage options; redundancy; PeerTube-compatible P2P on by default with accessible opt-out; moderation/analytics; recovery/backup; concurrency and resource benchmarks | in progress: remote and local-origin VOD/live PeerTube P2P exchange and pre-play opt-out verified; public VOD embed opt-out verified; wider embeds, operations and scale evidence remain |
| M8 | Ecosystem and beyond parity | Defined REST/embed/runner compatibility scope; chosen extensions; end-to-end tests per supported contract; no PeerTube plugin ABI in round one | pending |
| M9 | Completion gate | All accepted inventory entries verified; current and prior PeerTube matrices; browser/a11y/performance gates; operator and user documentation | pending |

The inventory needs expansion into individually testable features before a
percentage is reported. A milestone count is not a feature-parity percentage.
No "100%" claim while required rows are missing, deferred, failed, or untested.
AP09's Audio/Image/Document authoring also needs an explicit disposition; video
delivery alone must not silently close those parts of the original finding.

## M1 sign-off — 2026-09-29

The original functional M1 gate is now verified. Channel management at
`/videos/channels` is HTML-first: profile editing, explicit publisher/editor
invitations and consent, refusal/revocation, accepted owner transfer and confirmed
deletion. The client API exposes the same authority and pending invitations.
Owners/editors can manage channel Videos; publishers can manage their own.
Original authorship, channel URI, Video URI/UUID and social identity are immutable.
Permission locks fence revocation through commit, including caption/metadata and
live-ingest authority. Suspended identities cannot consent or retain ingest access.

Ownership handoff is deliberately quiescent: finish/cancel drafts, scheduled
publication and live work (including replay finalization and recurring broadcasts)
first. Acceptance enforces the recipient's channel quota and revokes previous
grants. Channel deletion retracts collaborator Videos but preserves their accounts;
person deletion still retracts that person's original publications after transfer.
Database/outbox failures roll the authority/content change back atomically.

Verification:

- Rust integration: `cargo test -p plamenu --test native_video --test
  scheduled_statuses --test edits --test atomic_outbox --test profile --
  --test-threads=2`: **69 passed**. Includes live credential revocation, pending
  scheduled-work transfer refusal, recipient quota/deletion recovery, consent,
  active identities, original signer, concurrent permission fencing and injected
  outbox rollback. DB recurring-live replay regression: **1 passed**.
- Real local peers, refreshed runtime: `e2e/tests/test_video_channel_lifecycle.py`,
  native Video publication/update/delete, inbound PeerTube HLS playback and
  verified-channel library tests: **7 passed, no skips**. PeerTube 8.3 receives
  profile and accepted ownership updates, imports a collaborator's Video, serves
  byte ranges and FFmpeg-decodable media, accepts edits before/after handoff and
  retracts deletion. The M4 run additionally verifies standalone Video deletion
  after transfer from the original uploader to a different Person: the original
  author's Delete is preserved for other peers, supplemented by a distinct,
  current-owner-signed Delete that PeerTube authorizes. Both enqueue in the
  deletion transaction, including channel/person cleanup. No-JS HTTP forms prove CSRF/privacy and two-step published
  Video deletion. Evidence: `target/e2e/ap09-channel-lifecycle-20260929.xml`.
- `./dev a11y` with `video-channels.spec.mjs`: **7 passed**, all pinned-container
  projects: Chromium desktop/mobile, Firefox desktop/narrow-touch, WebKit
  desktop/mobile and scripts-disabled Chromium. Keyboard-only submissions cover
  invitation, accept, profile edit, revoke, transfer consent and delete. Axe and
  320px reflow checks pass; the mobile full-page screenshot was visually checked
  for readable labels, native controls and no horizontal clipping. Evidence:
  `target/accessibility/ap09-channels-20260929/passed/`.
- Strict workspace/all-target Clippy, SQLx offline metadata check, formatting and
  whitespace checks pass. [Channel guide](VIDEO_CHANNELS.md) and API contracts
  document the workflow, limits, roles and handoff constraints.

Failures were not hidden: the initial browser run failed a test locator whose
label query included textarea contents; the actual accessible textbox name was
correct, and the corrected role-based query passed every project. First-run traces
remain in `target/accessibility/ap09-channels-20260929/first-run/`. A Rust regression
first exposed cached-account suspension acceptance; fresh locked identity checks
fixed it. Two E2E setup attempts had wrong peer-root paths, before the real run.
These are supporting browser accessibility checks, not human AT testing or a WCAG
claim. Large-history/load, wider audience/asset rules and the manual release gate
remain in their original M2/M7/M9 inventory scope. M1 and the M4 sign-off below
close the requested two-milestone goal without changing either acceptance gate.

## M4 sign-off — 2026-09-29

The original functional M4 gate is verified. `/videos/playlists` is the native
public library and HTML-first creator/import surface, also linked from the Video
space and channel libraries. Public/unlisted/private collections support create,
profile edit, stable duplicate occurrences, add/remove/reorder, per-entry clip
boundaries and title-confirmed deletion. Owners/editors manage channel lists;
consenting publishers manage their own. Revocation, transfer, concurrent revision
checks and transactional outbox failure protect authority and saved content.

Playlist deletion preserves Videos. Video deletion preserves entry identity,
position and boundaries as an unavailable placeholder, including hard deletion,
thread stubs and moderation/channel/person cleanup. A collection cannot widen a
Video's audience, reveal restricted titles/status IDs or grant media access.
Accepted channel transfer preserves native Playlist/element identities and
revokes the previous owner's implicit authority.

The normal player supplies captions, descriptions, adaptive HLS, default-on
compatible P2P and opt-out. Clips use native progressive media fragments without
scripts; JS reapplies the seek on HLS takeover, pauses at the boundary and
announces completion. Previous/Next remain ordinary links; there is no forced
autoplay/navigation, clip encoding or media copying. Native numeric positioning
is enhanced with keyboard-operable up/down controls and focus restoration.

PeerTube `Playlist`/`PlaylistElement` roots, pages and stable occurrence URLs
federate Create/Update/Delete under current channel-owner authority. Activity
and object audiences agree, including unlisted re-publication after private
retraction. Optional empty descriptions serialize as null for PeerTube, both
for Playlists and Videos. Native actor `playlists` exposes public history only.
Explicit import/refresh verifies canonical origin, Group/Person ownership and
bounded auxiliary metadata; it never eagerly downloads every Video or media.
Stale updates, foreign signers, looping/cross-origin pages, invalid identities,
timestamps/counts, old-owner Deletes and delayed resurrection are rejected.

Verification:

- Rust integration: `cargo test -p plamenu --test video_playlists --test
  native_video --test profile --test atomic_outbox -- --test-threads=2`:
  **61 passed, no skips** (12 playlist, 25 native Video, 20 profile, 4 atomic
  outbox). Includes consent/roles/revocation, handoff and distinct author/owner
  Delete identities, restricted/deleted Videos, empty/cleared descriptions,
  copied anchored permalinks, transactional rollback and hostile remote input.
  The final ownership regression also denies cached former-owner deletion when
  the channel origin is unavailable, without mutating the saved collection.
  The 1000-entry limit and full 1000-entry API rendering pass a three-second
  gate with batched audience/metadata reads; 16 simultaneous first-party app
  bootstraps return one application identity.
- Real local PeerTube 8.3, refreshed runtime: `e2e/tests/test_video_playlists.py`:
  **3 passed, no skips**. Covers empty-description native publication, signed
  duplicate/clip/reorder/profile/remove updates, private retraction and unlisted
  sharing, accepted ownership transfer, new-owner edit/Delete and standalone
  Video deletion, both-direction import/updates/deletion, deleted-video
  placeholders, no-JS HTML import/CSRF/confirmation, scoped API authorization,
  revision conflicts and private cache headers. Evidence:
  `target/e2e/ap09-playlists-20260929.xml`.
- `./dev a11y video-playlists.spec.mjs --workers=2`: **7 passed** in the pinned
  Chromium desktop/mobile, Firefox desktop/narrow-touch, WebKit desktop/mobile
  and scripts-disabled Chromium projects. Real HTML upload/publication, copied
  URL authoring, duplicate clips, keyboard reordering/settings/confirmed delete,
  decoded clip playback with start/end assertions and Previous/Next navigation
  pass. Axe and 320px reflow checks pass; a mobile screenshot was visually
  checked for readable labels, controls and wrapping. The repeated final run
  includes the deleted-entry migration. Evidence:
  `target/accessibility/ap09-playlists-20260929/passed-final/`.
- Strict workspace/all-target Clippy, SQLx offline metadata check, Rust
  formatting, JavaScript syntax and whitespace checks pass. The
  [playlist guide](VIDEO_PLAYLISTS.md) and API contracts document workflows,
  permissions, limits, metadata-only import and deliberate no-autoplay behavior.

Failures remain visible. The four failed browser runs retain traces in
`target/accessibility/ap09-playlists-20260929/{first,second,third,fourth}-run/`.
They exposed real anchored-URL, concurrent first-login and HLS source-reset
issues as well as incorrect test locators/source assumptions. Earlier E2E XMLs
(`ap09-playlists-{first,second,third,fourth,fifth,sixth}-20260929.xml`) retain
empty-description and envelope-audience interoperability failures and corrected
test assumptions about PeerTube's element-shaped API, UUID-only unlisted reads
and combined Vary headers. The final standalone-delete extension also caught
PeerTube's current-channel-owner deletion rule; it now passes without dropping
the original author retraction for other peers.

These are supporting automated accessibility checks, not human AT testing or a
WCAG claim. M9's manual release gate is unchanged. No full PeerTube REST or
plugin-ABI compatibility is claimed. M4 adds no companion service or native
installation/maintenance step. M2/M3/M6/M7/M8/M9 retain their existing open gates;
two closed functional milestones are not an overall parity percentage.

## M3 sign-off — 2026-09-29

Closed the original accessible-metadata functional gate. No acceptance gate was
removed. Initial and published authoring distinguish visual descriptions,
written alternatives, timed captions and author soundtrack statements. The
HTML publisher, edit forms, caption language manager, chapters and readable
caption transcript work without application scripts. Enhanced transcript and
chapter links seek the inline player; native links retain media fragments.

Alternative edits now participate in the original status transaction before
the signed Update is constructed, rather than being written afterwards.
Authorized collaborators use the original author's media authority and signer;
outbox failure rolls back alternatives and the edit timestamp. Preview overlays
the validated alternatives without writing. JSON client edits share this path.
Canonical Video emission includes bounded schema.org transcript and namespaced
visual-description/soundtrack statements; authorized remote Video updates
retain/clear them on the canonical primary without replacing deferred media.
PeerTube 8.3 accepts supported description/caption/chapter updates and ignores
the additive alternatives. Lossless PeerTube re-export of extensions is not
claimed. See [user/API guide](VIDEO_ACCESSIBILITY.md) and
[federation vocabulary](federation/media.md).

Evidence:

- **74 focused Rust checks passed**: native_video (27), content_universe (21),
  wire_shapes (12), edits (6), atomic_outbox (4), remote caption/chapter guards
  (2), caption transcript parser (2). New regressions cover collaborative
  alternative-only edits, original signer, injected outbox rollback,
  revocation, oversize/clear semantics, escaped text and deferred-media identity.
- **Five real PeerTube 8.3 E2E tests passed**: local/remote captions, local/remote
  chapters and video metadata. Local initial alternatives follow the file in
  a normal multipart form and survive finalization. Published JSON alternative
  edits retain languages while PeerTube receives the supported description.
  Result: `target/e2e/ap09-accessible-metadata-20260929.xml`.
- **Seven browser projects passed** `video-accessible-metadata.spec.mjs`:
  Chromium/Firefox/WebKit desktop and narrow/touch configurations, plus
  scripts-disabled Chromium. Keyboard forms/disclosures, actual decoded frames
  and active replaced French cues, readable transcript/seek links,
  published alternatives, retained chapters, removal/revoked routes, 320px
  reflow, reduced motion, forced colours and Axe checks pass. Evidence:
  `target/accessibility/ap09-metadata-20260929/final/`.
- Visual inspection of the real development upload form and keyboard-expanded
  caption transcript confirmed readable layout, native disclosures and visible
  focus. Browser screenshots include narrow transcript and forced colours.
- Workspace all-target strict Clippy, SQLx metadata check, formatting,
  whitespace and mdBook build passed.

Failed runs remain in the evidence directory. They exposed late multipart
soundtrack loss, caption URLs ignoring their revision, and WebKit native-control
value painting overflowing the page. These are fixed. Further test fixes wait
for decoded frames/removal completion, handle intentionally omitted empty
optional metadata and use PeerTube 8.3's actual complete-description detail
endpoint, not the removed separate endpoint.

This is functional/browser evidence, not human screen-reader, real-mobile
hardware or WCAG release certification. M9's independent human AT gate remains
open. M2/M6/M7/M8/M9 and the wider inventory retain their original open scope.
No new installation dependency, push, merge or staging deployment.

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

M0/M1/M3/M4/M5 have functional sign-offs. The current two-additional-milestone
goal has closed M3 and now targets M2: broader bounded ladders, published
posters/storyboards and maximum-size/concurrent-upload/recovery measurements.
M6's remaining creator/viewer surfaces, M7's operations/scale and M9's wider
fault, real-device and human AT release checks remain open. Milestone completion
is not AP09 completion or a parity percentage.

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

### 2026-09-24 — PeerTube-compatible remote HLS peer delivery

- A signed PeerTube Video now supplies bounded WSS tracker, video UUID and
  segment-checksum hints, retained alongside its remote HLS attachment.
  Discovery and checksums must refer to the publisher's host; malformed or
  unrelated hints leave ordinary HTTP playback untouched. The checksum JSON
  is served by Plamenu's same-origin, public-access-checked proxy with a
  2-MiB fetch ceiling and bounded one-minute cache, so the browser never
  fetches it from the remote origin. The standard page CSP
  permits cross-origin WSS signaling but not cross-origin HTTP `connect-src`.
- The 110-KiB Apache-2.0 P2P module is vendored inside the binary, lazy-loaded
  only on playback. No CDN, Node process, tracker service or install step is
  introduced. A visible checkbox beside eligible videos explains network
  address sharing and defaults on; a persisted per-browser opt-out acts before
  any tracker connection. Scripts-disabled playback remains the progressive
  MP4, and opt-out playback remains ordinary HLS.
- The real PeerTube 8.3.0 browser E2E passed **1/1 in 51.59 s**: a fresh
  two-rendition PT Video was imported; the peer swarm infohash matched PT's
  ActivityPub `Infohash`; two independent Chromium contexts connected to
  PeerTube's WSS tracker; a delayed HTTP fallback forced a checksum-validated
  274,144-byte segment transfer from one Plamenu viewer to the other; the
  receiver's completed-segment event confirmed the checksum-validated bytes,
  not merely received chunks. A 320px
  pre-play opt-out connected to no tracker, played via HTTP, persisted after
  reload, showed no overflow, and passed the WCAG A/AA scan. Toggling off
  during an active peer session closed its tracker socket. A scripts-disabled
  browser played the progressive MP4 with no inoperable peer control.
  The prior PeerTube HLS and live federation regressions passed **2/2 in
  81.55 s**. Screenshot:
  `target/e2e/peertube-p2p-optout-320.png`. The parser unit test passed.
  Pre-migration database archive:
  `ap09-runtime-trt7qd/plamenu-before-remote-hls-peers.dump` (listing verified).
- At this point P2P01–P2P04 remained **partial**: direct PeerTube first-party player ⇄ Plamenu
  peer exchange, Plamenu-origin tracker/swarm announcements, live segment
  throughput, embeds, unsupported devices and sustained-load/failure drills
  still need tests. This is not a full P2P or M7 sign-off.

### 2026-09-24 — first-party PeerTube ⇄ Plamenu segment interoperability

- The same fresh-video browser matrix now brings up PeerTube's own first-party
  VideoJS/P2P player on its real WSS tracker. With the first Plamenu peer as
  the only other viewer, PeerTube requested a segment and Plamenu uploaded
  274,144 bytes to it. After that Plamenu seed closed, a fresh Plamenu browser
  received 274,144 checksum-validated bytes from PeerTube's player; both
  advanced beyond the first segment while playing. The full case passed
  **1/1 in 110.11 s**. This exercises both client directions, not merely two
  Plamenu tabs agreeing with each other. The test uses a short disposable
  PeerTube video and deletes it afterward.
- P2P01 is **verified for remote VOD against current PeerTube**. P2P05 records
  the distinct remaining Plamenu-origin tracker, swarm and integrity contract;
  live swarms, embeds, more browsers/devices and load/abuse tests remain open.

### 2026-09-24 — Plamenu-origin native VOD swarms

- Migration 0101 stores immutable per-byte-range SHA-256 digests with each
  generation-fenced local HLS rendition. The worker hashes init and media
  ranges in 64 KiB chunks before publication, bounded to 64 MiB per segment,
  10,000 ranges per rendition and a sub-2 MiB combined manifest. A valid
  video beyond a sharing limit remains playable over HLS/MP4 with P2P disabled;
  the optional peer lane never rejects publication. Publishing assigns PeerTube v2 swarm
  infohashes in the same transaction as the Video UUID. The ActivityPub Video
  advertises the rendition infohashes, checksum Link and WSS tracker; the
  client API supplies the same hints. Existing MP4 attachments and no-JS
  playback stay independent of P2P.
- The tracker is included in the Rust server, with no companion process. It
  accepts only indexed, currently published local infohashes; sockets, swarms,
  peers, offers, signaling frames, outbound queues and announce rates have
  explicit ceilings. An arbitrary-swarm Chromium probe got `unknown info_hash`
  and the socket closed. The standalone checksum unit test and three native
  upload browser cases passed: Chromium desktop, Chromium at 320px and
  Chromium without JavaScript. The browser checks include a real 206 segment
  digest, visible local P2P choice, decoded video and caption cues.
- The real PeerTube 8.3.0 outbound multipart-draft E2E passed **1/1 in
  103.96 s** on the final binary: PeerTube imported a fresh 12-second Plamenu Video with both HLS
  renditions, checksum URL and Plamenu tracker. PeerTube's own VideoJS/P2P
  player received **185,852 bytes** from a Plamenu viewer; after that seed
  closed, a fresh Plamenu viewer received **185,852 checksum-validated bytes**
  from PeerTube with zero validation errors. The existing PeerTube watch matrix
  played, sought and switched quality on Chromium and Firefox desktop/320px.
  A separate pre-play opt-out ran before the two peers: it played over HTTP
  without a tracker socket or peer engine. Private report:
  `ap09-runtime-trt7qd/native-p2p-verified.xml`. The local DB
  was backed up and its archive listing verified before migration 0101 at
  `ap09-runtime-trt7qd/plamenu-before-native-p2p.dump`. Disk remains about
  190 GiB free with non-incremental builds; the tracker research temp was
  removed.
- P2P01 now covers VOD in both origin directions, and P2P05 covers native VOD.
  This does **not** sign off live P2P, embeds, multi-node tracker fanout,
  sustained concurrency/abuse pressure or all browser/device combinations.

### 2026-09-24 — rolling live-segment integrity prerequisite

- The live session's same-origin `sha256.json` now contains SHA-256 digests for
  only the committed MPEG-TS segments named by its current atomic HLS playlist.
  A session-fenced, bounded in-process cache hashes each immutable segment
  once and prunes entries as the DVR window moves. Public-access, on-air,
  filename, segment-size, playlist-size, manifest-size and cache ceilings all
  apply. The manifest is `no-store`, returns an empty object before the first
  segment, and returns 404 after the session ends. It is not yet advertised
  as a live peer source; ordinary HLS/MP4 playback is unchanged.
- The focused hash/prune unit test and strict offline all-target Clippy passed.
  Real outbound live federation E2E passed on each of the three native
  ingest paths: RTMP **1/1 in 28.24 s**, HTTPS-FLV and RTMPS **2/2 in
  114.50 s**. Each compared a served MPEG-TS segment with its manifest hash
  and checked teardown, while retaining the existing PeerTube on-air/replay
  assertions. Reports: `ap09-runtime-trt7qd/live-sha256-rtmp.xml` and
  `ap09-runtime-trt7qd/live-sha256-all-ingest.xml`.

### 2026-09-24 — Plamenu-origin live swarms

- Migration 0102 records the PeerTube-compatible default-stream infohash
  atomically with each on-air session. Plamenu's bounded in-process tracker
  admits that infohash only while its public/unlisted broadcast is live and
  its owner/channel are not suspended; one-second low-latency mode does not
  enter a swarm. The on-air ActivityPub Video advertises the infohash, rolling
  checksum manifest and tracker, and the client API supplies the same hints.
  No installation step or companion process was added. The first-party
  browser refreshes the rolling manifest for new live segments, checks their
  SHA-256 before accepting peer bytes, and keeps the existing HTTP HLS/MP4
  paths when peers or scripts are unavailable.
- A real RTMP broadcast imported into PeerTube 8.3 passed the dedicated
  outbound browser E2E on the final binary **1/1 in 45.20 s**. PeerTube's unmodified player got
  **368,480 bytes** from Plamenu; after the Plamenu seed closed, a new
  Plamenu viewer accepted **748,428 checksum-validated bytes** from
  PeerTube with zero validation errors. The test also verified default-on
  disclosure with pre-play opt-out (no tracker contact), a scripts-disabled
  MP4 source, federation/API hints, and checksum/tracker rejection after the
  broadcast ends. The previous two failed runs exposed errors in the new
  *test harness*: an incorrect HTML selector and a 4-second HTTP delay that
  starved the live player's startup buffer; neither was counted as passing.
  Report: `ap09-runtime-trt7qd/live-p2p-verified.xml`. Strict offline
  all-target Clippy passed, and the existing low-latency RTMP/PeerTube
  lifecycle regression passed **1/1 in 28.67 s** with assertions that this
  one-second mode exposes no peer hints.
- P2P01 and P2P05 now include live first-party interoperability. This does
  not close M5/M7: embeds, wider browser/device coverage, and sustained
  concurrency/resource evidence remain.

### 2026-09-24 — active stream-key revocation across native transports

- Stream-key claims now retain their credential generation in the active
  session. A bounded two-second server-side check, independent of incoming
  media cadence, stops a publisher if the owner rotates its key. The RTMP
  accept loop remains in the same Plamenu process and same port; it retains
  a duplicate socket handle so revocation can actually disconnect RTMP and
  RTMPS encoders. Merely dropping tags in the upstream callback had left
  them connected, as the first E2E run exposed. Cookie-authenticated browser
  camera/screen capture does not depend on the stream key and is unaffected.
  Database verification errors fail closed as an interrupted stream rather
  than silently continuing with uncertain credentials. The no-JS management
  form now warns about active-stop/replay consequences, with English and
  Russian strings; the one-time key page no longer says an active encoder
  must be stopped separately.
- Real RTMP, HTTPS-FLV and RTMPS rotation E2E passed **3/3 in 55.49 s**.
  Each path started an on-air recurring Video with committed media, rotated
  its key, observed publisher disconnection and a closed HLS session, rejected
  the old key, restarted a second generation with the new key, and retained
  two independent replays. HTTPS-FLV's FFmpeg `q` does not reliably send a
  clean final POST body frame, so the test uses the explicit owner rotation
  to stop its second generation; unexpected disconnects retain the existing
  reconnect grace period. Report:
  `ap09-runtime-trt7qd/live-key-revocation.xml`. The existing outbound
  RTMP/HTTPS-FLV/RTMPS HLS, PeerTube-import and replay regression passed
  **3/3 in 134.25 s** on the same binary; report:
  `ap09-runtime-trt7qd/live-key-compat-regression.xml`. Strict offline
  all-target Clippy passed. The Plamenu-origin live P2P first-party browser
  exchange also passed again **1/1 in 44.28 s** after the RTMP listener change;
  report: `ap09-runtime-trt7qd/live-p2p-after-revocation.xml`.
- LIVE01 is verified for these encoder transports. At this point M5 still
  required sustained DVR/window, concurrency and wider recovery evidence.

### 2026-09-25 — bounded long-window DVR and concurrent viewers

- A new real-RTMP, no-JS-created recurring-broadcast E2E drives media
  timestamps at 20× using FFmpeg's built-in audio/video pacing filters.
  The five-minute DVR held exactly **150 playlist segments** and at most
  **151 MPEG-TS files**; the thirty-minute DVR held **900** and **901**.
  Both advanced the media sequence past the first ten segments, returned
  404 for the evicted first segment, removed it from the rolling SHA-256
  manifest, verified a retained segment against its served checksum, and
  finalized a separate long replay. No additional software or install step
  was introduced.
- On an 8-core/16-thread i7-11700F dev host with 31 GiB RAM, 16 parallel
  HTTP viewers each fetched three master/child/checksum/segment cycles while
  the encoder remained on air. For the five-minute window, **192 requests**
  returned **4,329,472 bytes**, median **50 ms**, p95 **72 ms**, max **80 ms**.
  For the thirty-minute window, **192 requests** returned **11,213,268
  bytes**, median **56 ms**, p95 **86 ms**, max **98 ms**. The same single
  FFmpeg remux process served all viewers; its observed RSS stayed at
  **51,736 KiB** and **52,064 KiB**, respectively, before and after each
  burst. These are local debug-build fixture measurements, not production
  capacity or browser/P2P-scale claims. Both tests passed **2/2 in 130.26 s**;
  report: `ap09-runtime-trt7qd/live-dvr-pressure.xml`.
- The initial five-minute run also passed but took 174.50 s because input
  `-readrate` did not pace the synthetic source at the requested factor;
  the fixture was corrected to FFmpeg's built-in realtime filters before
  the final two-window run. Disk remained about **188 GiB free** with
  incremental build artifacts at zero; completed live scratch is subject
  to the existing timed app cleanup, not manually deleted mid-finalization.
- LIVE06 and PERF02 gain bounded-window and 16-viewer evidence, but remain
  partial pending failure UX, broader load and multi-node tests. M5 remains
  open.

### 2026-09-25 — PeerTube-origin live swarm interoperability

- Imported on-air PeerTube Videos now expose their signed, same-origin HLS
  swarm hints in the client API and first-party player. PeerTube's small-
  latency mode deliberately suppresses P2P, consistent with its player. The
  rolling SHA-256 proxy refreshes remote live state before serving, uses a
  one-second in-process cache with per-URL single-flight origin fetches, and
  returns `no-store`; the ended broadcast removes hints and returns 404 even
  if a former checksum document was cached. The player decodes the proxied
  origin URL before matching each live segment to PeerTube's SHA document;
  this also correctly handles URL-safe Base64 in the VOD proxy.
- A real PeerTube 8.3 RTMP broadcast, on-air ActivityPub Video, both unmodified
  first-party browser players, and PeerTube's tracker verified bidirectional
  delivery: Plamenu accepted **711,204 SHA-256-validated bytes** from a
  PeerTube viewer and then uploaded **711,204 bytes** to a fresh PeerTube
  viewer, with **zero validation errors**. The same E2E checked a visible
  default-on choice, opt-out before tracker contact, scripts-disabled MP4
  fallback, rolling checksum changes and post-end revocation; **1/1 in
  63.57 s**. The existing remote VOD P2P browser regression also passed
  **1/1 in 112.35 s**, validating **274,144 bytes** in both exchange
  directions after the URL-decoding change. These are live local-instance
  tests, not broad device or Internet-scale evidence.
- P2P01 now has VOD and live first-party exchange in both origin directions.
  M5/M7 remain open for the wider browser/device, embed, resource and
  multi-node evidence called out above.

### 2026-09-25 — inbound multi-language captions

- Added a language-keyed caption sidecar without breaking the legacy local
  `caption_vtt` column or its API `caption_url`. A signed PeerTube Video's
  `subtitleLanguage` links are bounded to 16 tracks and the publishing
  origin; unsafe languages, credentials, duplicates and cross-origin URLs are
  ignored. Caption bytes are fetched only when selected, through the guarded
  federation client under a 256 KiB limit. Redirects cannot escape the
  publishing origin; only valid timed UTF-8 WebVTT is served. Client API and
  no-JS HTML expose same-origin track URLs, never PeerTube's raw link.
  Inbound Updates replace the track list even when the Video text and media
  did not change; removed tracks stop serving.
- A real PeerTube 8.3 VOD with English and French captions passed **1/1 in
  21.13 s**. The Plamenu API listed both languages; both WebVTT proxies
  returned the correct cues with `no-store`; scripts-disabled Chromium showed
  both native `<track>` elements; JS-enabled playback activated the French
  cue with no page errors. Removing French on PeerTube federated an update to
  Plamenu and made its old URL return 404. This does not yet verify local
  multi-language authoring or remote chapters.

### 2026-09-25 — local multi-language captions before publication

- The durable draft review now offers no-JS add/replace/remove WebVTT forms
  keyed by validated language, with owner-only, non-cacheable draft track
  URLs. The original single-caption upload remains visible in the same track
  list. Replacing its language clears the stale legacy body; removing it
  removes the old route. The JS enhancement previews cue count and catches
  oversized/non-VTT files before submit, while the server remains the
  authority. English and Russian labels are available. One batched caption
  query supplies the client API and canonical Video ActivityPub object, which
  advertises every local track with same-origin WebVTT Links.
- A real no-JS upload/review, two-language caption add/replace/remove/re-add,
  publication, PeerTube channel import, and JS/no-JS first-party playback E2E
  passed **1/1 in 18.84 s** on the final styling build. A forged `../fr`
  language was rejected by the server; a logged-out French draft URL
  returned 404; the authenticated request served the cue. JS validation
  rejected an invalid file, then announced one cue for a valid file with no
  page errors or serious/critical Axe violations in the caption section.
  The full-page screenshot was inspected: controls remain clearly labelled,
  reflow within the review column, and the lighter remove action meets text
  contrast after the first Axe run found 4.26:1 and prompted a colour fix.
  PeerTube imported both English and French caption languages;
  Chromium without scripts saw both native tracks and with scripts activated
  the French cue. An earlier test run stopped only because its unsigned
  direct AP GET expected public dereferencing where Plamenu requires a signed
  fetch; the real PeerTube import passed on the corrected run. The existing
  inbound two-language PeerTube regression also passed **1/1 in 33.06 s**
  after unifying legacy and sidecar tracks. Post-publication caption management
  and update federation were tackled in the following slice.

### 2026-09-25 — published caption edits and federated refresh

- `/videos` now links each owned published film to a no-JS caption manager.
  The page previews the video with native tracks and offers language-keyed
  add/replace/remove forms; the same JS cue/size preview enhances them.
  Mutations lock the owned publication and commit caption rows, the Video's
  edited timestamp and its signed `Update(Video)` outbox item atomically.
  Local caption links carry a short content-derived revision, so a replacement
  changes the ActivityPub URL and remote caches can refresh. The legacy
  single-track link and additive client API remain usable. The old public
  caption route now rejects unattached, private, suspended and deleted media;
  language-specific links apply the same checks.
- A real local draft -> published Video -> PeerTube import test then added
  Spanish, replaced the French cue, and removed English after publication.
  PeerTube imported the added language, cached the **new French cue bytes**
  from the revisioned link, and removed English on the third signed Update.
  An unrelated signed-in account received 404 for the manager and its POST;
  after Video deletion, its remaining caption links immediately returned 404.
  The first-party player exposed the resulting Spanish/French tracks without
  scripts and played the new French cue with scripts: **1/1 in 31.21 s**.
  Desktop, 390px mobile and settled dark-mode Axe scans found **zero
  serious/critical caption-section violations**; mobile had no horizontal
  overflow. The initial dark scan sampled an animated intermediate palette
  during the scheme switch, so the final check waits 600 ms for the theme to
  settle. Desktop and mobile screenshots were inspected. This remains a local
  Chromium/PeerTube 8.3 result, not full M3 sign-off.

### 2026-09-25 — PeerTube-origin chapters and accessible seeking

- Added a bounded chapter sidecar for Video media. A signed PeerTube Video's
  `hasParts` URL is fetched with the existing signed, SSRF-guarded ActivityPub
  transport, with same-origin redirects only. The collection `id` must match,
  the set is limited to 100 unique nonnegative timecodes and 200-character
  titles, and malformed collections do not replace an existing good timeline.
  Embedded `hasParts` arrays are also supported. Create and Update both store
  chapter changes even when the post text and media are unchanged; deletion
  cascades with the media. One batched query adds `chapters` to client API
  attachments.
- The first-party player renders chapter names and times in an accessible
  disclosure. Without scripts, each link opens the local playable MP4 with a
  temporal media fragment; JS enhances the same keyboard-operable link to
  seek the inline native player. No chapter URL points to PeerTube's media.
- A real PeerTube 8.3 video published three chapters, then changed to two
  different chapters and finally removed all. Plamenu imported each state.
  A Chromium browser with scripts disabled found all three local fragment
  links; with scripts enabled, Enter sought the embedded player to six
  seconds without navigation. The final E2E passed **1/1 in 22.48 s**.
  Desktop, 390px mobile and settled dark-mode Axe scans found **zero
  serious/critical chapter-section violations**, with no mobile horizontal
  overflow; desktop and mobile screenshots were visually inspected. Local
  chapter authoring and outbound PeerTube import remain for the next slice.

### 2026-09-25 — local chapter authoring and PeerTube export

- Native Video draft review and the owned published-video library now expose
  a no-JS chapter editor: one time/title per line, sorted and validated under
  100 chapters, 200 characters per title and the known video duration. Saving
  an empty list removes all chapters. JS adds immediate error feedback, a
  sorted preview and an add-current-player-time button. Published changes
  lock ownership and commit the chapter rows, edit timestamp and signed
  `Update(Video)` outbox item in one transaction. The chapter collection URL
  carries a content revision; stale URLs return 404 after replacement.
- The canonical Video object now advertises a dereferenceable `hasParts` AP
  collection, with `hasPart` names, start and end offsets. PeerTube fetches
  this auxiliary collection **unsigned**, unlike its signed Video fetch.
  Plamenu allows only this collection outside the authorized-fetch gate, and
  its handler still requires a public/unlisted, existing, non-deleted native
  Video and the current content revision. Other AP objects keep the gate.
  The first E2E exposed a missing canonical `/ap/accounts/...` route (404);
  the second exposed PeerTube's unsigned chapter GET (401). Both were fixed
  before the passing run.
- A real no-JS draft upload/review added three chapters, rejected duplicate
  times, and published them. PeerTube imported all three from the collection;
  an owned published edit sent a signed Update and PeerTube adopted the new
  two-chapter timeline; an empty edit removed them. An unrelated signed-in
  user received 404 for the manager and a forged POST. The final E2E passed
  **1/1 in 30.02 s**: old revision URLs returned 404, the current empty
  collection remained readable, and deletion revoked it. Draft and published
  editors both passed desktop, 390px mobile and settled dark-mode Axe scans
  with zero serious/critical findings and no mobile overflow. Their screenshots
  were visually inspected. The focused parser unit test covers valid sorting,
  duplicates, invalid clocks and out-of-duration starts. This completes the
  VOD chapter functionality in A11Y03, not all of M3's browser/AT matrix.
  A final combined inbound/outbound run passed **2/2 in 48.54 s** after the
  publication route also began rejecting malformed forged input with 404.

### 2026-09-25 — readable on-demand caption transcripts

- Each caption track's client API entry now has a same-origin `transcript_url`.
  First-party Video cards offer a visible, no-JS language link distinct from
  the author's transcript/visual description. The page fetches remote VTT only
  when opened, applies the existing public-track visibility, SSRF, byte and
  redirect-origin checks, and does not cache personalized HTML. A bounded
  parser extracts cue timings and safely strips remote cue markup. Speaker
  labels are retained; empty or malformed cues are skipped.
- The page keeps a native player and timestamped cue list. Without JavaScript
  each cue links to the same-origin progressive video with a media fragment;
  with JavaScript, Enter seeks the inline player and focuses it. The caption
  language is scoped to the cue list, leaving interface strings in the user's
  negotiated language. The explanatory text states that caption-derived words
  do not replace an author's visual description.
- Parser unit tests and strict Clippy passed. Local and PeerTube 8.3 E2E passed
  **2/2**, including local caption edits/deletion, inbound caption update and
  removal, no-JS readable cues, same-origin media-fragment fallback, JS
  keyboard seeking to 16 seconds, and desktop/mobile/dark Axe scans with no
  serious/critical findings or mobile overflow. Desktop and 390px screenshots
  were visually inspected. The inbound test exposed a separate update race:
  PeerTube may send another Video Update while its HLS ladder settles and our
  current ingest replaces the attachment row, invalidating an earlier media
  ID. The test now re-resolves the current attachment while waiting, rather
  than treating an obsolete URL as the publication's final caption URL. The
  single-primary VOD identity case is addressed in the next slice; mixed-media
  and active-processing cases still need separate work. This does not claim complete
  assistive-technology coverage or M3 closure.

### 2026-09-25 — stable remote VOD attachment identity through edits

- The ingest edit path now reconciles a single deferred remote Video attachment
  in place instead of deleting and recreating it when PeerTube changes a source
  rendition, HLS master, poster or title. This keeps its media ID and already
  emitted same-origin progressive, HLS, caption and transcript URLs stable.
  Rendition metadata is replaced transactionally. When the source or poster
  changes, stale cached files and HLS segments are detached and queued for
  durable cleanup in the same transaction. Live, mixed-media and actively
  processing posts retain the existing conservative rebuild path.
- Two database tests passed: URL/master/poster replacement preserved the ID,
  caption row and new ladder while retiring obsolete cached bytes; mixed-media
  and active-job rows refused the narrow in-place path. A real PeerTube 8.3
  Video title Update reached Plamenu while its existing caption and transcript
  URLs stayed accessible and its media ID did not change. Subsequent caption
  removal retained the same ID. The inbound chapter edit/removal E2E and the
  baseline PeerTube HLS/progressive/audio playback E2E passed. Strict Clippy
  passed for server and database. An actual PeerTube re-transcode that changes
  rendition URLs is covered by the database test, not yet by a live-peer E2E.

### 2026-09-25 — browser live device choice and silent-screen capture

- The live manager now offers camera and microphone selectors and an explicit
  screen-audio request. Device enumeration is progressive: names appear when
  the browser exposes them, current selections survive device refreshes, and
  changes apply to the next broadcast. The fieldset stays hidden without JS,
  preserving the existing RTMP/RTMPS/HTTPS-FLV encoder workflow. The controls
  are responsive and labelled in English and Russian.
- Firefox accepts a VP8+Opus recorder MIME for a video-only screen stream but
  may not emit chunks until recording stops. The publisher now requests a
  video-only codec when capture has no audio track. FFmpeg's WebM probe is
  bounded to 256 KiB for live startup. A focused browser/encoder trace confirmed
  one chunk per second reaches FFmpeg and that camera and silent-screen HLS
  segments are delivered over HTTP before stop.
- Real browser E2E passed camera -> silent screen -> separate replay sessions
  on the same recurring broadcast in Chromium and Firefox, desktop and mobile
  (**4/4** after retrying a transient blank login page in two parallel Chromium
  workers). The test checks selected device constraints, no requested screen
  audio, video-only recorder MIME, actual MPEG-TS bytes, replay playback,
  320px reflow, and light/dark Axe scans. The isolated browser suite passed
  **7/7** for Chromium/Firefox device UI, WebKit external-encoder fallback and
  no-JS live authoring. Firefox mobile screenshots were visually inspected.
  These are synthetic camera/screen sources with real MediaRecorder, WebSocket,
  FFmpeg, HTTP and playback; physical devices and browser permission prompts
  remain untested.

### 2026-09-25 — bounded browser broadcast startup and failure recovery

- Browser broadcasting now bounds the WebSocket ready handshake at 15 seconds.
  A timeout or early socket failure closes the attempt, releases captured
  tracks, restores the form controls and announces a retryable error. The
  handoff also checks that the socket is still open before creating a recorder.
  Permission denial, unavailable or busy devices and cancelled screen sharing
  use distinct localized status messages; they do not claim a broadcast is on
  air. The HTML/encoder workflow is unchanged without scripts.
- A new real-browser test injects each capture failure and a never-ready
  connection, checks that no live session was claimed, verifies the captured
  tracks end after timeout, then retries through the real authenticated
  WebSocket to an on-air session and clean stop. Chromium and Firefox desktop
  passed **2/2** in the isolated HTTPS runner. The normal camera/silent-screen
  journey and the existing unexpected-close reconnect case also passed in
  the local browser regression run (one first-run Chromium login request hit
  the transient service startup window and was rerun after readiness).
  This verifies application recovery, not physical-device permission dialogs.

### 2026-09-25 — visible recovery from live HLS outages

- The enhanced live player no longer dies silently after a failed HLS master
  or sustained segment outage. A pre-manifest network failure reloads the
  source because `startLoad()` alone has no parsed levels to resume. Repeated
  segment errors can remain non-fatal to hls.js indefinitely even while no
  media buffers; a 30-second deadline now stops that traffic unless a fresh
  fragment buffers. Successful buffering clears the deadline and resets the
  fatal retry count, preserving normal DVR seeking through brief holes.
- On exhaustion, the first-party player announces an accessible status with
  a basic same-origin stream link and a real page reload to discover an ended
  broadcast or replay. When the player is visible, the status scrolls into
  view; at 320px its links sit above the fixed tab bar. The scripts-disabled
  progressive source and existing external HLS playlist link are unchanged.
- A real browser broadcast with injected 503 responses verified both master
  and segment failures, the 30-second bound, accessible actions, and resumed
  decoded playback after removing the outage and refreshing. Chromium desktop
  and 320px mobile passed **2/2**. Light/dark Axe scans found no violations in
  the failed player, and screenshots were visually inspected. Initial tests
  exposed the pre-manifest no-op retry, same-URL hash navigation that failed
  to reload, and non-fatal fragment retries that otherwise ran for 90 seconds;
  each was corrected before the passing run. Broader network/codec/device
  failures and no-JS rewind are still open. The ordinary desktop/mobile DVR
  rewind-and-return journey also passed **2/2** after this change, and the
  PeerTube 8.3 inbound live lifecycle/playback E2E passed **1/1 in 69.71 s**.

### 2026-09-25 — locally authored multi-date live schedules

- Local one-shot and recurring broadcasts now accept up to 16 distinct planned
  starts. The no-JS HTML form accepts one local date-time per line, and JS adds
  a date-picker shortcut to the same field; both paths use the account time
  zone and a single server-side parser. Waiting creators can reorder or clear
  the list. The database stores the ordered list with its first date as a
  constrained projection, keeping existing queries and client API fields
  stable. The client API and PeerTube-compatible ActivityPub Video publish all
  dates. A new recurring episode consumes one date; a reconnect preserves the
  remaining dates; a one-shot start clears the list. Dates are announcements,
  not automatic starts or ingest restrictions.
- A database regression passed owner-only edits, the first and second
  recurring-session consumption, and active-session edit rejection. Browser
  tests passed **4/4** across Chromium desktop/mobile, Firefox desktop and
  scripts-disabled Chromium at 320px, plus Firefox at 320px **1/1**. They cover
  chronological sorting, duplicate rejection, clearing, public semantic time
  display, reflow and an Axe accessibility scan. Isolated WebKit desktop and
  mobile passed **2/2**; the 320px Chromium screenshot was visually inspected.
  The real PeerTube 8.3.0 outbound lifecycle passed RTMP, RTMPS and HTTPS-FLV
  **3/3**: PeerTube imported both dates and accepted the one-shot on-air
  clearing Update. Strict Clippy
  passed. Calendar subscriptions and reminders are still outside this slice.

### 2026-09-25 — M5 native-live functional sign-off

- Added an explicit rewind link on local live status pages and a scripts-off
  first-party page with a GET-based five-minute portion selector, native MP4
  controls, byte-range seeking and return-to-live navigation. The endpoint
  stream-copies the existing fragmented recording into a finite fast-start
  clip, rather than re-encoding or requiring a second persistent rendition.
  It waits for the archive header before reading, bounds FFmpeg time/output,
  preserves the configured DVR window, caps per-session scratch cache, checks
  available disk, expires old URLs, and invalidates clips when the session
  ends. Builds share a per-session lock, so simultaneous viewers do not
  duplicate work while independent broadcasts can proceed concurrently.
  The first browser run exposed a real `moov atom not found` startup race;
  the bounded header wait/retry fixed it before sign-off.
- M5's stated gate is met: real RTMP, RTMPS and HTTPS-FLV ingest; Chromium and
  Firefox desktop/mobile browser camera, microphone and screen ingest through
  HLS and replay; waiting, on-air, interrupted, ended and recurring replay
  identities; locally authored and imported multi-date schedules; three
  latency modes and 5/30-minute DVR; no-JS external encoder, scheduling,
  playback and rewind paths; and Plamenu-to-PeerTube plus
  PeerTube-to-Plamenu live updates and playable streams. Stable session IDs
  and replay relationships are in place for a later federated live chat.
- Final local evidence: strict server and database Clippy passed; the native-live
  PeerTube/encoder suite passed **13/13** (including key revocation, P2P and
  reconnect/replay variants), inbound PeerTube live lifecycle/playback passed
  **1/1**, and accelerated 5/30-minute retention and load passed **2/2**.
  The latter probed the oldest permitted rewind MP4 and had 16 concurrent
  clients range-seek the same clip, as well as 16 ordinary HLS viewers.
  Scripts-disabled Chromium at 320px passed finite playback, native seek,
  `206` and `416` byte-range behavior, stale/out-of-window rejection and
  post-session `404` **1/1**; even an extreme negative expiry bucket is a
  bounded `404`, not an overflow. Browser live checks passed **25** across
  Chromium/Firefox/no-JS, with **11** intentional applicability skips;
  isolated WebKit desktop/mobile passed **4** applicable fallback/authoring
  checks with **12** capture/playback skips. WebKit's local test runtime was
  supplied missing shared libraries in user-local test artifacts, not an
  application runtime dependency. The 320px no-JS screenshot was inspected;
  it revealed a return link obscured by fixed navigation, which was moved
  above the selector and rechecked.
- This closes **M5's written functional acceptance**, not every AP09 live
  enhancement. Physical camera/OS permission prompts, WebKit capture support
  and wider viewer fault injection remain M9 validation; seamless in-session
  camera/screen switching and the broader reminder experience remain M6 work;
  cross-node scratch/storage, redundancy and greater P2P/load pressure remain
  M7 operations work. The requested future live-chat architecture exists, but
  chat implementation remains outside this round. These items remain visible
  in the inventory rather than being represented as complete.

### M6 live calendar subscription slice — 2026-09-25

- Added per-broadcast iCalendar feeds for public and direct-link unlisted
  planned starts, plus a strictly public per-channel feed. The existing
  schedule records are authoritative; no reminder worker, companion service,
  or new installation step is required. The optional 15-minute `VALARM` is
  delivered by the subscriber's calendar app, not Plamenu. Feeds use UTC,
  escaped/folded iCalendar text, stable occurrence UIDs, ETags and bounded
  five-minute cache lifetime. An event is a planned start, not a claim about
  broadcast duration or actual on-air time.
- The owner gets a selectable subscription URL and reminder link in the
  no-JS management page; viewers get calendar and channel-subscription links
  under planned dates. The UI explains that opening a feed once may only
  import a snapshot. Client API native-live media expose both feed URLs.
- Verification: scripts-disabled Chromium at 320px passed the scheduled
  broadcast, anonymous per-broadcast feed, 15-minute alarm, conditional GET,
  unlisted channel exclusion and public channel inclusion paths (**2/2**).
  Chromium and Firefox mobile schedule-picker checks passed (**2/2**), with
  a zero-violation axe scan on the scheduled status figure. The Chromium
  mobile status screenshot was inspected for readable links and reflow.
  The database visibility regression covers public, unlisted and private
  feed access. Rust calendar unit tests and strict database/server Clippy
  passed. Isolated WebKit desktop was attempted twice but its WPE web process
  aborted on post-login navigation before reaching this UI; this is an open
  browser-matrix result, not a WebKit pass.
- This is not M6 completion. Calendar-app-specific subscription refresh and
  reminders, server-side notification preferences, publication scheduling,
  in-session source switching, discovery/search and the wider M6 experience
  matrix remain open. No staging deployment was performed.

### M7 public video/P2P embed slice — 2026-09-25

- Closed the oEmbed placeholder gap with a real, read-only, local-status frame
  that uses the first-party media renderer, captions, descriptions, no-JS
  progressive fallback and the existing default-on P2P player. Its peer
  disclosure and opt-out precede the embedded video, so a viewer can choose
  server-only playback before any tracker or swarm contact. The CSP framing
  exception is scoped to a successful numeric status embed path; every other
  HTML page retains frame denial. Frame rendering ignores session cookies and
  rejects non-public, remote and deleted statuses. oEmbed now accepts a
  copy-pasted permalink with its `#post-id` fragment.
- Verification: the scripts-disabled browser rejects a private post's frame
  even for its author. A real cross-origin loopback host loads the VOD frame;
  browser assertions cover the visible default-on choice, keyboard-operable
  pre-play opt-out, decoded playback and absence of tracker socket or P2P
  module request after opt-out. Its 400px and 320px frame screenshots were
  inspected, and the narrow frame has no horizontal overflow.
  The native-video browser fixture now reuses an owned channel when the local
  test account reaches its 50-channel cap, preserving accumulated test data.
- This is a Plamenu public embed, not PeerTube's complete embed API. The wider
  PeerTube embed, live-frame, WebKit, performance and abuse matrices remain
  open. No push, merge or staging deployment was performed.

### M6 in-session browser source switching — 2026-09-25

- Browser broadcasts now retain one authenticated WebSocket, one MediaRecorder,
  one live status/HLS session and one replay while switching camera → screen →
  camera. A capped 720p canvas feeds the stable video track; a Web Audio
  destination preserves the audio track when switching to a screen without
  captured audio. Only the active source is connected; previous capture tracks
  are released after the replacement is playable. This adds no external
  service, installation step or stored intermediate media.
- A rejected capture request leaves the previous source broadcasting. The
  controls name the active source, dim its unavailable action, announce the
  transition and error in the live region, and restore keyboard focus to Stop
  or the retry action. Device-choice help now reflects switch-time changes.
  External RTMP/RTMPS/HTTPS-FLV encoder authoring remains the no-JS route.
- The canvas WebM time base was advertised as 1000 fps to replay validation;
  browser WebM ingest now emits an explicit constant 30 fps HLS/replay output,
  matching its 30 fps GOP size. This was found by the real replay E2E gate,
  not inferred from the preview alone.
- Verification: the new in-session test and the existing separate camera/screen
  capture-to-HLS/replay test passed in Chromium and Firefox, desktop and 320px
  mobile (**8/8**). The new test checks one recorder/socket, live status,
  changed preview pixels, released old tracks, denied-switch continuity, a
  live HLS segment, one saved and playable replay, focus, overflow and zero axe violations.
  The no-JS encoder-authoring test passed (**1/1**). A 320px viewport screenshot
  was inspected after switching; controls, status and preview reflow without
  horizontal overflow. Strict server Clippy, formatting, JS syntax and diff
  checks passed. Actual hardware/OS permission prompts, WebKit, mid-switch
  device loss, long-duration CPU/battery measurements and broader recovery
  remain open. M6 is not yet complete; no push, merge or deployment occurred.

### M6 video discovery and subscriptions slice — 2026-09-25

- Added `/videos/explore` as the signed-in Videos navigation destination and,
  when federated public preview is enabled, to signed-out navigation. It has
  an authoring link back to `/videos`. The server-rendered catalogue has
  public local and imported Video cards, title/description search, a followed-
  channel view, follow/unfollow actions, and keyset-based Older navigation.
  Search and follow work as ordinary HTML forms and links without JavaScript.
  The cards load a lazy poster rather than instantiating a player per result;
  the Video permalink retains the established accessible playback, caption,
  description and pre-play P2P choice. Sensitive videos expose neither title
  nor thumbnail on the catalogue card.
- Discovery identifies local publishing channels through
  `video_publications`, falling back to the source actor for imported Videos.
  The database query restricts to public, original, attached Videos and
  excludes unlisted/private/deleted/history-only posts; owner/channel blocks,
  mutes and suspensions are checked before display. Anonymous catalogue access
  follows the federated public-timeline preview switch, while anonymous text
  search requires the existing public-search switch. A dedicated newest index
  and title/description GIN index are included in migration 0106; the local
  indexed title/description `EXPLAIN ANALYZE` completed in 0.266 ms on the
  current development corpus. No external search process is required.
- A layout regression checks that the signed-out Videos link appears only
  when its public preview is enabled. The page itself also enforces the
  anonymous preview/search switches; the navigation alone never grants access.
  On the local instance, a real 320px Chromium browser with scripts disabled
  opened the signed-out mobile drawer, found its Videos link and the catalogue
  search field, and showed no horizontal overflow; the page returned HTTP 200.
- Verification: the database regression passed public/privacy, indexed title
  and description matches, cursor pagination, pending/accepted channel
  follows, and channel blocking. The real native Video upload/publication test
  now visits the catalogue, submits its search form, checks its poster and
  channel link, follows through the HTML form, enters the followed view via
  its disclosure, and unfollows. The complete journey passed Chromium and
  Firefox desktop/mobile plus Chromium with JavaScript disabled at 320px
  (**5/5**). The 320px screenshot was inspected; the channel link received an
  underline after axe caught a color-only distinction, and the subsequent
  mobile run passed with no axe violations or horizontal overflow. Strict
  database/server Clippy passed. WebKit desktop was attempted but its local
  page process crashed immediately after login, before the catalogue was
  reached; it remains unverified, not an app failure. Remote-channel acceptance, large-
  corpus/concurrent search load, broader channel libraries and M6's other
  creator/viewer features remain open. No push, merge or deployment occurred.

### M6 video catalogue poster and calendar visibility fixes — 2026-09-25

- Catalogue cards previously selected the first attachment's thumbnail, even
  when that attachment was an image before the actual Video. They now choose
  the first `video`/`gifv` attachment. A focused unit regression covers image-
  first media, and a second checks that sensitive poster URLs remain hidden.
- The local video poster's authored description was available on the playback
  page but lost in the catalogue. Cards now expose it through a native HTML
  disclosure outside the watch link, with the video's language; it remains
  available without scripts. The real upload/publication browser journey opens
  that disclosure and checks its text in Chromium mobile, Firefox desktop and
  Chromium without JavaScript (**3/3**). The 320px screenshot was inspected;
  poster text and controls fit without horizontal overflow.
- A public broadcast calendar checked the channel's suspension but not the
  creator's. Both broadcast and channel event queries now exclude suspended
  creators, and a channel calendar name is unavailable while its owner is
  suspended. The existing isolated recurring-live regression failed at the
  new suspension assertion before this fix, then passed after it. No calendar
  data is deleted; clearing a suspension restores availability.
- Sensitive or content-warned live titles were still exported to `NAME` and
  `SUMMARY` in public iCalendar feeds. Their query titles are now a generic
  label. The same isolated regression failed before that change with the
  secret title, then passed with sensitive and spoiler-text redaction for both
  broadcast and channel feeds. Feed identity, links and planned times are
  unchanged; only the sensitive title is withheld.
- The final offline database and web unit regressions passed, as did strict
  database/server Clippy, formatting, JavaScript syntax and diff checks.
  These fixes do not complete M6; WebKit and calendar-app subscription refresh
  remain unverified. No push, merge or staging deployment occurred.

### M6 channel/creator video libraries — 2026-09-25

- The catalogue's channel name now opens a public Video-only library for that
  actor; the author's channel list and local channel profile link there too.
  The library has channel-specific title/description search, keyset Older
  links, a creator-profile link and an HTML follow/unfollow form with the
  current search and page preserved after submission. JavaScript is optional.
- Native publications are selected by the indexed channel/publication relation;
  imported Videos without that relation are selected by their author-account
  index. Filters run within each channel branch before its bounded candidate
  window, so an older channel's videos cannot disappear merely because 1,000
  unrelated Videos were published globally. The public-only, completed-media,
  deletion/history and owner/channel moderation predicates remain in force;
  blocked or suspended libraries return no page. The local development query
  plan returned a 21-row native-channel page in 4.46 ms; this is not a
  large-corpus or concurrency benchmark.
- The isolated database regression covers native channel vs author fallback,
  public vs unlisted, scoped search and cursor paging, a viewer block and owner
  suspension. URL preservation and translation-key unit checks passed. The
  real native Video publication journey enters the library, searches, tests
  its follow form, returns to global discovery and the followed view, and
  checks the owner's channel link and profile navigation. Chromium mobile,
  Firefox desktop and Chromium without JavaScript at 320px passed (**3/3**)
  with axe and overflow checks. The first browser run had an assertion with
  reversed query-parameter order; the next run exposed an old test assumption
  that the owner's channel link still opened the profile, plus a missing wait
  after returning to global search. Those test expectations were corrected
  without application changes. The final **3/3** browser run passed after the
  compact, wrapping header was built; its 320px screenshot was inspected.
  Anonymous HTTP checks returned 200 for a public library, 400 for an invalid
  channel id and 404 for an unknown one. Strict Clippy passed.
- M6 remains open: larger channel libraries, remote PeerTube channel
  attribution/follow acceptance, scheduling/settings/imports/history and the
  wider browser/hardware matrix still need verification. No push, merge or
  staging deployment occurred.

### M6 imported PeerTube channel libraries — 2026-09-25

- PeerTube 8.3.0's Video declares both a Person uploader and a Group channel;
  the Group actor independently declares that Person as its owner. Imported
  Videos now retain the Person as the status author but persist a verified
  Group-channel relation for catalogue cards, channel-scoped libraries and the
  Followed filter. Ingest and Update both reconcile it. A missing, ambiguous,
  cross-origin or non-owner Group claim cannot place a Video into a channel;
  a temporary actor-fetch failure leaves an already verified mapping intact.
- The fresh PeerTube upload test exposed that remote video attachments may have
  `kind = NULL` despite a playable `video/mp4` MIME type. All catalogue
  branches now accept that media shape; the database regression uses it.
- The isolated database regression verifies attribution, author-library
  exclusion, channel-library inclusion, accepted-channel Followed filtering
  and removal. Fixture parsing and wrong-owner/type checks pass. A real
  PeerTube 8.3.0 upload was resolved into Plamenu, displayed in the
  server-rendered public channel library, and its Group Follow was accepted
  (**1/1** E2E). The first live run discovered the null-kind gallery bug; the
  next exposed an incorrect test permalink assumption; the final run passed.
- This is not M6 completion. Already imported Videos have no stored AP object
  from which to derive their Group without a remote refresh; the new relation
  applies on new live ingest or Update, so a bounded historical backfill remains.
  Cold-history imports defer channel actor fetch until live promotion because
  those rows are excluded from public discovery.
  Remote-channel discovery at larger scale, live browser accessibility, wider
  follow/ownership-change cases and the rest of M6 still need verification.
  No push, merge or staging deployment occurred.

### M6 automatic legacy PeerTube channel backfill — 2026-09-25

- An in-process worker now recovers channel attribution for public remote
  Videos imported before that relation existed. It refetches the canonical
  Video, requires the same stored author, then applies the same Group-owner
  verification as live ingestion. It changes only the channel relation: no
  historical notification, status edit, media rebuild or timeline fan-out.
- One Video is claimed every 15 seconds. A durable five-minute lease survives
  a crash or restart; transient origin/Group failures retry with bounded
  exponential backoff, while conclusively absent/invalid claims are marked
  checked. Idle scans run every five minutes. Cold-history-only rows stay
  outside public discovery and are not fetched until promoted. The automatic
  worker needs no extra installation or operator maintenance step.
- Database tests cover claim, lease, retry and terminal settlement; the worker
  backoff unit test passes. In a real PeerTube 8.3.0 E2E, a newly imported
  Video's two attribution rows were removed to simulate a legacy import; the
  running server restored the verified channel from the remote AP object
  without an operator command (**1/1**). Strict Clippy and SQLx checks pass.
- M6 remains open. This slice does not establish large-corpus scan cost,
  multi-instance contention or the broader remote ownership-change matrix.
  No push, merge or staging deployment occurred.

### M6 scheduled native Video publication — 2026-09-25

- `POST /api/v1/videos` accepts an RFC 3339 `scheduled_at` and returns a
  scheduled-status entity. The first-party Video review form offers an
  independently labeled, timezone-aware Schedule action. Its
  `datetime-local` value resolves through the author's configured zone, works
  with JavaScript disabled, and gains required-field validation with JavaScript.
  The existing queue supports owner reschedule and cancel; a queued draft now
  points to that queue instead of offering a misleading Publish/Delete action.
- The scheduled row stores its channel and reserves exactly one ready Video
  media row in the same transaction. Immediate publication, duplicate queues,
  draft cancellation, ordinary media deletion and draft expiry cannot take
  that reservation. A distant schedule extends draft retention; rescheduling
  adjusts it, and cancellation releases the media with seven days of ordinary
  draft life. The due worker reuses the normal Video/Create/outbox transaction
  and generation-fenced claim, so publication and queue removal commit once.
- Database-backed Video reservation/due/cancel tests pass (**2/2**); existing
  scheduled-post tests pass (**15/15**). Real PeerTube 8.3.0 E2E confirms no
  early Video, one due publication and channel import (**1/1**). A separate
  no-JS HTML upload/review/schedule/reschedule/cancel E2E passes (**1/1**),
  including refusal to delete a reserved draft. Browser review-to-queue
  journeys pass Chromium desktop/mobile, Firefox desktop and Chromium with
  JavaScript disabled (**4/4**), with reflow and axe checks on JS variants.
  Strict Clippy and SQLx preparation pass. Migrations 0109–0110 were applied
  only to the disposable AP09 development/test database.
- This closes the scheduled-VOD portion of PUB03, not M6. Reminders,
  category/licence/settings/imports, published-media editing, larger-library
  and device matrices, and failure/recovery drills remain open. No push,
  merge or staging deployment occurred.

### M6 interoperable Video publication metadata — 2026-09-28

- Native VOD upload/review now offers category, explicit licence, content warning
  and sensitive-media choices alongside title, description and language. Hashtags
  in the description use the existing tag system. Private draft options survive
  HTML and resumable upload, review reloads and scheduled publication. Draft
  updates merge under the media lock; scheduling reads warnings under the same
  lock as its reservation. No upload silently grants reuse rights.
- Published category/licence editing is available from `/videos` and the ordinary
  Video edit form, through the owner-only `/videos/{media_id}/metadata` manager.
  It works without JavaScript, reports successful saves, and uses native selects.
  Script-enabled selection updates a trusted licence link immediately; no-JS
  review/edit pages get the selected link from the server. The viewer exposes
  metadata in a native disclosure under the player. English and Russian labels
  are included. Warnings continue to use the existing whole-post native details
  gate and normal Status editor, rather than a second incompatible warning path.
- The client API accepts metadata on `POST /api/v1/videos`, exposes it on media
  entities/previews/schedules, and supports sparse owned published edits through
  `PUT /api/v1/videos/{media_id}/metadata`. Metadata, edited timestamp and canonical
  Update enqueue share a transaction; injected outbox failure rolls them all back.
  Exact no-ops create neither an edit timestamp nor another delivery job. The
  initial Create now carries the same metadata as dereference/history/Update.
- PeerTube 8.3.0 checks caught and fixed two wire incompatibilities: native Video
  Hashtag names must be bare (PeerTube stores them verbatim), and explicit metadata
  clears must send zero-ID `Unspecified` objects (omitted/null fields leave remote
  values unchanged). Ordinary Note hashtags are unchanged. Inbound metadata-only
  Updates now bypass the unchanged-text fast path, retain stable attachment IDs,
  and update category/licence even when no text or warning changes.
- Only bounded recognized category/licence IDs acquire local semantics. Remote
  labels and arbitrary links are discarded. PeerTube's licence ID alone does not
  establish a version, so imported CC licences remain generic and unlinked unless
  the exact trusted canonical URL is supplied. Local chooser options explicitly
  state CC 4.0 or public-domain 1.0; clearing a choice never leaves a local grant.
- Verification: native Video regressions **15/15**; metadata parser unit **1/1**;
  scheduled-post regressions **15/15**; dedicated metadata query-count guard
  **1/1** (3 Videos: 43 statements; 12: 42, with one initial-cache statement).
  Real PeerTube/API/HTML E2E **5/5** covers new delivery, late channel history,
  metadata edits/clears and warning/tag interoperability in both directions,
  range/decode checks, invalid API input, CSRF refusal and private manager caching,
  scheduled publication and no-JS schedule/reschedule/cancel retention.
- Final browser journeys **8/8** cover metadata upload/review/publication,
  keyboard warning/metadata disclosure, navigation from ordinary edit to manager,
  save confirmation and clearing, plus existing scheduling journeys in Chromium
  desktop/320px, Firefox desktop and Chromium with JavaScript disabled. JS variants
  have no axe violations, and reflow/screenshot inspection passes. Evidence is
  retained in `target/accessibility/ap09-video-metadata-20260928/` and
  `target/e2e/ap09-video-metadata-20260928.xml`. Strict Clippy and workspace SQLx
  preparation/check pass; migrations 0111–0112 were applied only to the disposable
  AP09 dev/test database. The rebooted local AP09 service was restored. About
  153 GiB remained free; no source, media, backups or audit evidence were removed.
- This completes the native-VOD PUB04 gate, not M6 or all publication parity.
  Initial live-form metadata controls, legacy-library metadata hydration, wider
  remote-vocabulary/assistive-technology/device cases and the other M6 inventory
  work remain. No push, merge or staging deployment occurred.
