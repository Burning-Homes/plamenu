# AP09 acceptance inventory

Reference: PeerTube 8.3.0; see [progress and evidence](AP09-VIDEO-PROGRESS.md).
This is the acceptance backlog, not a completion claim. Each row needs concrete
positive, failure, authorization and browser evidence where applicable. Existing
capability does not mean its expanded video use case is verified. Do not compute
a parity percentage from the number of rows: some represent much larger work.

## Identity and publication

| ID | Required capability | State |
| --- | --- | --- |
| ID01 | Stable channels owned by Persons; discovery, signatures, Follow/Accept | M1 verified 2026-09-29: native Group identity, real PeerTube Follow/Accept/Undo, collaborative Video attribution and consented owner handoff preserve canonical channel and Video identities. |
| ID02 | Channel listing, profiles, editing, deletion and owner deletion | M1 verified: HTML/API profile updates and confirmed deletion; deleting a channel retracts all collaborator-authored publications without deleting collaborators; deleting a person retracts their originals even after channel transfer. Rust, real PeerTube and seven browser configurations pass. |
| ID03 | Channel collaborators, roles, revocation and ownership transfer | M1 verified: explicit publisher/editor consent, refusal/revocation, active-account checks, transactional revocation fence, accepted ownership transfer with quota checks and no inherited privileges. Handoff rejects unfinished uploads/scheduled/live work rather than orphaning it. Browser and PeerTube evidence in the M1 sign-off. |
| ID04 | Channel outbox/history and local subscription timelines | Partial: signed path-paginated history imports in PeerTube; local public/unlisted feeds and block/delete gates pass Rust regressions; channel navigation passes seven browser configurations |
| PUB01 | Native Video Create, dereference, Update, Delete accepted by PeerTube | M1 verified: real PeerTube imports, decodes and seeks native Video and receives authorized edits/deletes after collaborative publication and ownership transfer. Original authorship remains immutable; a distinct current-owner Delete supplements the original-author retraction when PeerTube requires channel-owner authority. Standalone post-transfer deletion, empty descriptions and both-direction import/history regressions pass. |
| PUB02 | Public/unlisted/private/internal audience rules across all assets | Pending (public/unlisted publication started) |
| PUB03 | Drafts, scheduled publishing and publication-state recovery | Partial: first-party upload, durable owner review/retry/cancel, and Video scheduled publication now verified through the API, no-JS review/reschedule/cancel, real PeerTube delivery, and Chromium/Firefox browser flows; broader failure/recovery and reminder cases remain |
| PUB04 | Titles, descriptions, tags, language, category, licence and warnings | Native-VOD gate verified: HTML/resumable upload and review, scheduled retention, published category/licence editing and viewer disclosure, client entities, PeerTube Create/history/Update/clear in both directions, trusted licence provenance and batched rendering. Initial live-form controls, legacy-library metadata hydration and broader device/remote-vocabulary cases remain. |
| PUB05 | Comments/replies, reactions, shares, counters and comment permissions | Pending (reuse social machinery where compatible) |
| PUB06 | Canonical client API entities with independently playable attachments | In progress |
| PUB07 | Explicit Audio/Image/Document authoring from the original AP09 finding | Pending |

## Video processing and delivery

| ID | Required capability | State |
| --- | --- | --- |
| VOD01 | Bounded-memory long-form multipart upload without JavaScript | M2 functional gate verified 2026-09-29: actual streamed default-ceiling 2 GiB upload of a decodable one-hour source, bounded server/encoder RSS, complete duration/frame count, private review and no-JS browser publication. Hardware/workload limits are explicit in the resource record; sustained production capacity remains M7. |
| VOD02 | Resumable uploads, progress, cancellation and interrupted-upload recovery | M2 verified: six JS browser configurations and no-JS multipart fallback; maximum-default-size upload preserves committed offsets and same-file fingerprint through process kill/resume, complete processing and ready-state restart. Three concurrent real uploads across two accounts pass bounded-slot/memory checks and acknowledged cleanup. Wider device/network fault matrices remain M9. |
| VOD03 | Durable processing jobs, retry/leases, cancellation and atomic publication | M2 verified: generation/lease fencing, source preservation, private output journal, atomic MP4/HLS/storyboard readiness, retry, aborted-caller/storage failures, acknowledged cleanup, publication rollback and persisted uploader turns. Rust and actual maximum-size/concurrent-upload evidence are in the M2 sign-off; sustained multi-node operations remain M7. |
| VOD04 | Format/codec validation; remux/copy when compatible; safe transcoding | M2 functional gate verified: existing bounded/local-only format admission retained; 36,120-frame actual re-encode plus a complete 3600-frame one-hour source prevent silent truncation. Copy-top packet hashes match the source MP4, five real 1080p ladder outputs retain complete packet counts, and native codec/filter shares and file/disk ceilings are bounded. Broader hostile/container matrices remain M9. |
| VOD05 | Adaptive quality ladder and progressive/native-player fallback | M2 verified: finite copy-top plus useful sequential 1080p/720p/480p/360p/240p lower ladder, no upscale/near-duplicate rungs, range/hash delivery and independent MP4 fallback. PeerTube 8.3 imports/plays/switches/seeks three advertised qualities; first-party Chromium/Firefox/WebKit desktop/narrow and no-JS playback pass. Matched-rendition bidirectional PeerTube P2P and delayed no-seeder Auto startup pass. Sustained production-load/rebuffer benchmarks remain M7/M9. |
| VOD06 | Thumbnails, replacement posters, seek previews/storyboards | M2 verified: numeric/visual frame selection, unpublished and published still-poster replacement with description, immediate JS preview and native HTML form. Immutable quota-accounted AVIF/JPEG revisions and signed original-author Update preserve video/social/caption identity and the old poster on failure. Bounded immutable JPEG sprites feed first-party keyboard/no-JS timestamp navigation, client metadata and PeerTube's actual seek controls. Inbound remote storyboard import is not claimed; broader ecosystem contracts remain M8. |
| VOD07 | Audio-only, silent, portrait and unusual aspect-ratio inputs | M2 functional gate verified: audio-only waveform/AAC films and custom posters import/play in PeerTube; silent portrait/ultrawide films preserve aspect ratio and seekable HLS. Maximum-default-size one-hour and concurrent real 720p re-encode cases retain complete media. Broader format/fault/device combinations remain M9. |
| VOD08 | Replace/reprocess/trim workflows with stable social identity | Pending; current publication refuses media replacement |
| VOD09 | Quotas, resource budgets, admission control and fair processing | M2 functional gate verified: durable per-file/account/global/pending/draft ceilings, conservative free-space/slot admission, bounded encoder thread shares, original-uploader poster debt and acknowledged refunds. Persisted fair turns, concurrent claims, turn-commit rollback and suspended-upload exclusion pass Rust; two worker slots and two active uploaders are observed in three real concurrent uploads. Sustained-load and multi-node capacity remain M7. |
| VOD10 | Import by URL and scheduled remote-source synchronization | Pending; all fetches need SSRF and size protections |
| VOD11 | Downloads, embeds, range/seek, CORS and cache behaviour | Partial: a public read-only oEmbed frame now uses the existing video/HLS player and no-JS progressive fallback; broader embed, download, CORS and cache matrix remains. |

## Accessibility and viewer experience

| ID | Required capability | State |
| --- | --- | --- |
| A11Y01 | Upload/edit/remove multiple caption languages, native WebVTT tracks | M3 functional gate verified 2026-09-29: draft/published add, replace, remove, revision revocation, active native cues and retained languages pass seven browser projects and real PeerTube in both directions. Human AT release evidence remains M9. |
| A11Y02 | Transcripts, visual descriptions, audio-description intent and alternatives | M3 functional gate verified: initial HTML/JS authoring, published atomic original-author/collaborator edits, rollback, bounded canonical Video federation/update/clear, escaped native disclosures and caption-derived readable/seekable transcripts. Seven browser projects, five real PeerTube tests and Rust regressions pass; PeerTube ignores unsupported alternatives, without a lossless re-export claim. Human AT/WCAG release evidence remains M9. |
| A11Y03 | Chapters, keyboard-accessible seeking and no-JS chapter links | Complete for VOD: local draft/published authoring, PeerTube import/export/update, API metadata, no-JS media-fragment links and JS keyboard seeking E2E verified |
| A11Y04 | Remote `subtitleLanguage` and `hasParts`, including updates and hostile data | Partial: bounded same-origin PeerTube captions and chapter collections import/update/remove, local chapter outbound and signed Update verified; single-primary deferred remote VOD now retains attachment identity and caption/chapters URLs across Video edits; broader redirect/hostile-network and mixed-media matrix pending |
| UX01 | Discoverable publishing, review, validation and recovery without JS | Partial: upload-to-worker, private review/retry/confirmed cancellation, scheduled Video review/queue/cancel and enhanced phase updates pass browser configurations including no-JS; broader recovery and authoring flows remain |
| UX02 | Responsive video space, channel library, search and subscriptions | Partial: `/videos/explore` offers indexed public Video title/description search, newest keyset paging, channel attribution, lazy-poster cards, in-place follow/unfollow and a followed-channel view. Channel/creator links, the owner's channel list and native channel profile open a channel-scoped Video library with its own search, paging, profile link and no-JS follow form. Verified imported PeerTube Videos use their Group channel instead of the uploader fallback, including remote null-kind video media; fresh-upload library rendering and actual PeerTube Group Follow acceptance pass E2E. Existing public remote Videos gain verified channel attribution through a durable, rate-limited in-process backfill, exercised against PeerTube. Public/private moderation, channel isolation and cursor behavior have database regressions; the native upload → catalogue → channel library → scoped search/follow → global/followed view journey passes Chromium mobile, Firefox desktop and Chromium no-JS at 320px. Ownership-change cases, anonymous policy variations, WebKit and large-corpus load evidence remain. |
| UX03 | Enhanced player quality, resume/history, captions and keyboard controls | Pending |
| UX04 | Reduced motion, forced colours, zoom/reflow and assistive-technology checks | Pending |
| UX05 | Chromium/Firefox/WebKit desktop/mobile and scripts-disabled evidence | Partial: upload/review/publication/edit/preview, decoded playback, active WebVTT cues and channel navigation pass six desktop/mobile configurations plus Chromium no-JS in pinned container; native-control keyboard/AT checks remain |

## Playlists

| ID | Required capability | State |
| --- | --- | --- |
| LIST01 | Create/edit/delete and public/private/unlisted visibility | M4 verified 2026-09-29: HTML/API creation, revision-checked profile updates and title-confirmed deletion; private cache/CSRF/scope gates; shared-to-private retraction and unlisted re-publication in PeerTube preserve canonical identity. |
| LIST02 | Add/remove/reorder, duplicate handling and missing/deleted entries | M4 verified: stable occurrence IDs, deliberate duplicate clips, bounded ordering and removal; unavailable/restricted/deleted placeholders do not widen Video access. Hard/stub/purge deletion clears dead references without removing entries. Rust, actual signed PeerTube updates and seven browser projects pass. |
| LIST03 | Per-entry clip start/end boundaries and playback navigation | M4 verified: integer duration-checked boundaries, native no-JS fragments and enhanced HLS seek/end pause with completion announcement; actual decoded playback and ordinary Previous/Next links pass the seven-project browser gate. No clip transcoding or forced autoplay. |
| LIST04 | PeerTube Playlist federation, ownership, import and update semantics | M4 verified: real both-direction Playlist/PlaylistElement Create/Update/Delete, duplicates, clip/reorder/profile/removal, channel-owner handoff and privacy transitions; bounded canonical metadata-only import/refresh, stable IDs, stale/foreign/loop/invalid input and Delete retention regressions pass. Evidence in M4 sign-off. |
| LIST05 | Keyboard and no-JS library management | M4 verified: discoverable native library, complete HTML forms, numeric no-JS positioning, enhanced keyboard up/down/focus restoration, HTML remote import, CSRF/confirmation/privacy, 320px reflow and Axe pass. Chromium/Firefox/WebKit plus scripts-disabled Chromium verified; M9 manual AT gate remains unchanged. |

## Live broadcasting

These rows retain the broader AP09 parity backlog. M5's functional acceptance
is signed off separately in `AP09-VIDEO-PROGRESS.md`; a partial row here names
additional M6/M7/M9 work and does not claim that work was verified.

| ID | Required capability | State |
| --- | --- | --- |
| LIVE01 | Native authenticated encoder ingest, rotating/revocable credentials | Verified for RTMP, native RTMPS and HTTPS-FLV: key rotation rejects new use of the old key and closes an active key-authenticated publisher within the bounded poll; recurring streams restart with the new key and keep both session replays |
| LIVE02 | Browser camera/microphone broadcasting and device selection | Partial: camera/microphone selection, actual capture-to-HLS/replay and accessibility checks pass Chromium/Firefox desktop/mobile; real hardware and WebKit capture support remain unverified |
| LIVE03 | Browser screen sharing, source switching and permission/error recovery | Partial: camera ↔ screen switching within one recorder/socket/live HLS session and one replay passes synthetic-capture Chromium/Firefox desktop/mobile, including a silent screen, released old tracks, denied-switch preservation, 320px controls/focus/axe and replay output. Separate camera/screen recurring sessions also pass capture-to-HLS/replay. Localized denial, unavailable/busy device, cancelled sharing and bounded connection-start retry pass Chromium/Firefox desktop; unexpected disconnect recovery passes Chromium desktop. Real OS permission prompts and hardware, WebKit, mid-switch device-loss cases and wider recovery/performance matrices remain |
| LIVE04 | Waiting/on-air/interrupted/ended/replay lifecycle and remote updates | Partial: one-shot conversion and separate recurring session Video publication, reconnect, crash recovery and PeerTube import verified; full delayed/out-of-order matrix remains |
| LIVE05 | One-shot/recurring/scheduled broadcasts with stable session identity | Partial: one-shot/recurring identities, generation-fenced reconnect, independent replay identities, and up to 16 locally authored or inbound PeerTube planned starts with reschedule/clear verified. Per-broadcast and public-channel iCalendar feeds with optional calendar-app reminder pass no-JS browser tests at 320px and public/unlisted/private authorization regression; suspended creators are excluded from all calendar entry points, and sensitive/content-warned live titles are redacted in calendar names and event summaries. Real calendar-app refresh/reminder matrix and broader reconnect/schedule matrix remain. |
| LIVE06 | DVR, latency modes, concurrency and replay capture/processing | M5 gate verified: bounded sliding DVR and three latency choices with real ingest and PeerTube import; desktop/mobile rewind and return-to-live; no-JS five-minute selectable MP4 portions with native seek and HTTP ranges; accelerated 5/30-minute eviction, rolling hashes, 16 HLS viewers, 16 simultaneous rewind range requests, one clip build and long replay saves; bounded accessible recovery from fatal master/sustained segment outages in Chromium. Broader empty/partial-fragment and fault matrices remain M9; multi-node storage remains M7 |
| LIVE07 | No-JS creation/settings/credentials/replay workflows; encoder fallback | M5 gate verified: creation, idle playback and schedule settings, key rotation with active-stop warning, RTMP/RTMPS/HTTPS-FLV fallback, one-shot/recurring replay navigation and playback, plus native no-JS DVR portion selection and seeking at 320px. Broader creator recovery UX remains M6/M9 |
| LIVE08 | Plamenu-to-PeerTube and PeerTube-to-Plamenu live playback | Partial: actual outbound and inbound playback E2E pass; PeerTube DVR/latency and bidirectional schedule import/update verified, broader matrix remains |
| LIVE09 | Explicit future-chat identities, permissions and replay relationships | Architecture requirement; no chat implementation this round |

## Operations, ecosystem and scale

| ID | Required capability | State |
| --- | --- | --- |
| OPS01 | Native installation and maintenance with no required companion service | Required invariant; no new runtime dependency introduced so far |
| OPS02 | Optional storage/offload/worker scaling without changing basic installation | Pending |
| OPS03 | Storage lifecycle, garbage collection, backups and restore evidence | Pending |
| OPS04 | Redundancy/mirroring with ownership, retention and bandwidth controls | Pending |
| OPS05 | Creator analytics, server observability and privacy/retention controls | Pending |
| OPS06 | Moderation/reporting, abuse controls and takedown propagation | Pending |
| P2P01 | Actual PeerTube tracker/segment interoperability | Verified for both origin directions in VOD and live: current PeerTube 8.3 first-party player and Plamenu exchange real segments in both directions over the publishing host's tracker; Plamenu-side SHA-256-validated playback checked in browsers |
| P2P02 | P2P on by default, disclosed network-address sharing and accessible opt-out | Partial: eligible remote and local native VOD/live HLS default on with a visible accessible choice before play; PeerTube-origin live opt-out verified; wider device matrix pending |
| P2P03 | Opt-out before tracker/swarm contact; embeds and persisted preferences | Partial: pre-play opt-out verified in both origin directions for VOD and live, and per-browser localStorage persistence on remote VOD. A real cross-origin first-party oEmbed VOD frame shows the choice before Play; selecting server-only decodes video without any tracker socket or P2P module request. No-JS embedded playback and private-frame denial pass; wider live/embed/browser matrix remains. |
| P2P04 | Server-only fallback without JS, on failure and on unsupported devices | Partial: opt-out HTTP HLS and scripts-disabled progressive MP4 preserved; unsupported-device and fault-injection matrix pending |
| P2P05 | Plamenu-origin compatible swarm announcements, native tracker and integrity-checked uploads | Verified for native VOD and live against PeerTube 8.3: indexed published/on-air swarm allowlist, bounded in-process WebSocket tracker, immutable VOD range hashes and rolling live segment SHA-256, and bidirectional first-party segment exchange; scale/abuse matrix remains |
| API01 | Document Plamenu APIs and explicitly supported PeerTube REST/embed contracts | Pending; do not claim full PeerTube REST compatibility |
| PERF01 | Startup/seek/rebuffer/live-latency and CPU/RAM/storage/origin-byte budgets | Pending |
| PERF02 | Concurrency benchmarks; no per-viewer encoder or eager remote download | Partial: 16 local HTTP live viewers, 192 requests including rolling SHA manifests, one unchanged ~52 MiB remux RSS and bounded five-/thirty-minute HLS storage measured on the dev host; production-scale, browser/P2P concurrency and remote-download benchmarks remain |
| SEC01 | Upload/parser/fetch/permission/credential and out-of-order delivery tests | Pending |

## Explicit exclusions, not hidden omissions

- PeerTube plugin ABI/codebase compatibility: excluded from round one.
- Live chat, including federated live chat: later; preserve an extensible session
  model and permissions now.
- Automatic caption generation: later; uploaded captions remain required now.

Any further omission or material narrowing requires an explicit decision. A
working short-video publisher does not close the long-form or live milestones.
