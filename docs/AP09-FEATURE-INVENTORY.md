# AP09 acceptance inventory

Reference: PeerTube 8.3.0; see [progress and evidence](AP09-VIDEO-PROGRESS.md).
This is the acceptance backlog, not a completion claim. Each row needs concrete
positive, failure, authorization and browser evidence where applicable. Existing
capability does not mean its expanded video use case is verified. Do not compute
a parity percentage from the number of rows: some represent much larger work.

## Identity and publication

| ID | Required capability | State |
| --- | --- | --- |
| ID01 | Stable channels owned by Persons; discovery, signatures, Follow/Accept | In progress |
| ID02 | Channel listing, profiles, editing, deletion and owner deletion | In progress |
| ID03 | Channel collaborators, roles, revocation and ownership transfer | Pending |
| ID04 | Channel outbox/history and local subscription timelines | Partial: signed path-paginated history imports in PeerTube; local public/unlisted feeds and block/delete gates pass Rust regressions; channel navigation passes seven browser configurations |
| PUB01 | Native Video Create, dereference, Update, Delete accepted by PeerTube | In progress |
| PUB02 | Public/unlisted/private/internal audience rules across all assets | Pending (public/unlisted publication started) |
| PUB03 | Drafts, scheduled publishing and publication-state recovery | Partial: first-party upload, durable owner review/retry/cancel and publication tested; scheduled publication pending |
| PUB04 | Titles, descriptions, tags, language, category, licence and warnings | Pending (title/description/language started) |
| PUB05 | Comments/replies, reactions, shares, counters and comment permissions | Pending (reuse social machinery where compatible) |
| PUB06 | Canonical client API entities with independently playable attachments | In progress |
| PUB07 | Explicit Audio/Image/Document authoring from the original AP09 finding | Pending |

## Video processing and delivery

| ID | Required capability | State |
| --- | --- | --- |
| VOD01 | Bounded-memory long-form multipart upload without JavaScript | Partial: default 2 GiB source cap, private disk spool, durable worker handoff and no-JS browser journey verified; larger-file stress evidence pending |
| VOD02 | Resumable uploads, progress, cancellation and interrupted-upload recovery | Partial: offset-checked chunks and same-file resume pass six JS browser configurations; no-JS multipart and PeerTube import pass; a 64 MiB upload survives a disposable process kill/restart; maximum-size and concurrent-load evidence pending |
| VOD03 | Durable processing jobs, retry/leases, cancellation and atomic publication | Partial: upload-to-worker production wiring, persistence/journal/private preview, real-encoder retry/cancellation, storage ledger/recovery and HTML draft controls pass regressions; further long-form and fairness evidence pending |
| VOD04 | Format/codec validation; remux/copy when compatible; safe transcoding | Partial: shared pipeline preserved; actual 36,120-frame re-encode proves removal of silent truncation; ladder and resource cases pending |
| VOD05 | Adaptive quality ladder and progressive/native-player fallback | Partial: local copy-only top and bounded 360p single-file fMP4 HLS (when source exceeds 360p), peak segment bandwidth, same-origin range delivery and MP4 fallback pass worker/HTTP/browser regressions; PeerTube 8.3 imports both advertised qualities and its actual Chromium/Firefox watch UI decodes, switches quality and seeks at desktop/320px sizes; broader ladder, WebKit and sustained-load/seek benchmarks remain |
| VOD06 | Thumbnails, replacement posters, seek previews/storyboards | Partial: creators can choose a frame during native Video upload, with a visual JS picker or numeric no-JS input. Ready unpublished drafts can replace the generated poster with an uploaded still image and required description in the no-JS HTML client; JS adds an immediate preview. Immutable, quota-accounted AVIF/JPEG revisions preserve the previous poster on failure and PeerTube imports the replacement. Published-video poster editing and seek previews/storyboards remain pending. |
| VOD07 | Audio-only, silent, portrait and unusual aspect-ratio inputs | Partial: audio-only native Video becomes a 2 fps waveform with AAC and a poster, accepted and played by PeerTube; silent portrait and ultrawide inputs pass real-encoder/HLS jobs; further codec/container/error and long-duration cases pending |
| VOD08 | Replace/reprocess/trim workflows with stable social identity | Pending; current publication refuses media replacement |
| VOD09 | Quotas, resource budgets, admission control and fair processing | Partial: per-file durable reservations, configurable account/global/pending/draft limits, free-space admission, acknowledged-cleanup refunds and processing budgets tested; sustained load/fairness evidence pending |
| VOD10 | Import by URL and scheduled remote-source synchronization | Pending; all fetches need SSRF and size protections |
| VOD11 | Downloads, embeds, range/seek, CORS and cache behaviour | Pending (existing media endpoints are reused) |

## Accessibility and viewer experience

| ID | Required capability | State |
| --- | --- | --- |
| A11Y01 | Upload/edit/remove multiple caption languages, native WebVTT tracks | Partial: local draft and published add/replace/remove, PeerTube-origin import, bidirectional Update delivery, no-JS and JS playback verified; wider browser/assistive-technology matrix remains |
| A11Y02 | Transcripts, visual descriptions, audio-description intent and alternatives | Partial: author-written transcript/visual description and audio-description intent remain distinct; language-specific caption-derived transcript is now an on-demand, same-origin, no-JS readable/seekable page for local and PeerTube tracks, browser/E2E verified; broader AT and description matrix remains |
| A11Y03 | Chapters, keyboard-accessible seeking and no-JS chapter links | Complete for VOD: local draft/published authoring, PeerTube import/export/update, API metadata, no-JS media-fragment links and JS keyboard seeking E2E verified |
| A11Y04 | Remote `subtitleLanguage` and `hasParts`, including updates and hostile data | Partial: bounded same-origin PeerTube captions and chapter collections import/update/remove, local chapter outbound and signed Update verified; single-primary deferred remote VOD now retains attachment identity and caption/chapters URLs across Video edits; broader redirect/hostile-network and mixed-media matrix pending |
| UX01 | Discoverable publishing, review, validation and recovery without JS | Partial: upload-to-worker, private review/retry/confirmed cancellation and enhanced phase updates pass seven browser configurations including no-JS; resumable enhancement under verification |
| UX02 | Responsive video space, channel library, search and subscriptions | Pending |
| UX03 | Enhanced player quality, resume/history, captions and keyboard controls | Pending |
| UX04 | Reduced motion, forced colours, zoom/reflow and assistive-technology checks | Pending |
| UX05 | Chromium/Firefox/WebKit desktop/mobile and scripts-disabled evidence | Partial: upload/review/publication/edit/preview, decoded playback, active WebVTT cues and channel navigation pass six desktop/mobile configurations plus Chromium no-JS in pinned container; native-control keyboard/AT checks remain |

## Playlists

| ID | Required capability | State |
| --- | --- | --- |
| LIST01 | Create/edit/delete and public/private/unlisted visibility | Pending |
| LIST02 | Add/remove/reorder, duplicate handling and missing/deleted entries | Pending |
| LIST03 | Per-entry clip start/end boundaries and playback navigation | Pending |
| LIST04 | PeerTube Playlist federation, ownership, import and update semantics | Pending |
| LIST05 | Keyboard and no-JS library management | Pending |

## Live broadcasting

| ID | Required capability | State |
| --- | --- | --- |
| LIVE01 | Native authenticated encoder ingest, rotating/revocable credentials | Verified for RTMP, native RTMPS and HTTPS-FLV: key rotation rejects new use of the old key and closes an active key-authenticated publisher within the bounded poll; recurring streams restart with the new key and keep both session replays |
| LIVE02 | Browser camera/microphone broadcasting and device selection | Partial: Chromium desktop/mobile capture pipeline verified; device selection and other browsers remain |
| LIVE03 | Browser screen sharing, source switching and permission/error recovery | Partial: Chromium desktop/mobile screen pipeline verified; source switching and recovery remain |
| LIVE04 | Waiting/on-air/interrupted/ended/replay lifecycle and remote updates | Partial: one-shot conversion and separate recurring session Video publication, reconnect, crash recovery and PeerTube import verified; full delayed/out-of-order matrix remains |
| LIVE05 | One-shot/recurring/scheduled broadcasts with stable session identity | Partial: one-shot/recurring identities, generation-fenced reconnect, independent replay identities, owner-authored planned starts and multiple inbound PeerTube dates with reschedule/clear verified; local multi-date authoring remains |
| LIVE06 | DVR, latency modes, concurrency and replay capture/processing | Partial: bounded sliding DVR and three latency choices verified with real ingest, PeerTube import, desktop/mobile rewind and return-to-live; accelerated 5/30-minute window eviction, rolling hashes, 16 concurrent HTTP viewers, one remux and long replay saves verified; empty/partial-fragment failure UX and multi-node storage remain |
| LIVE07 | No-JS creation/settings/credentials/replay workflows; encoder fallback | Partial: creation, idle playback and schedule settings, key rotation with active-stop warning, RTMP/RTMPS/HTTPS-FLV fallback and one-shot/recurring replay navigation and playback verified; generic no-JS DVR seeking and broader creator recovery UX remain |
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
| P2P03 | Opt-out before tracker/swarm contact; embeds and persisted preferences | Partial: pre-play opt-out verified in both origin directions for VOD and live, and per-browser localStorage persistence on remote VOD; embed contract pending |
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
