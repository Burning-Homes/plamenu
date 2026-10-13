# Songhive parity design and acceptance inventory

The remaining implementation and final acceptance work is tracked in
[the completion checklist](SONGHIVE_COMPLETION_CHECKLIST.md).


This is an implementation and verification inventory, not a completed-parity
claim. The reference is Songhive **0.4.9**, source revision
`4243a566f8de767b824a55df69ae87c03d17e35b`, and the OCI index
`sha256:6fe2f971350b1c364be5b41b35c60510da7d7664fcd5f11b5b82512e7f1b8bf1`.
The local source is provisioned by `e2e/peers/sources.lock`; the independent
peer runs at `https://songhive.local`. This development hostname is local.

The reference source is [Songhive](https://git.fabiomanganiello.com/songhive).
The audit used its pinned `songhive/api/routes/`, `songhive/federation/`,
`songhive/models/`, `songhive/services/`, `songhive/adapters/subsonic/`,
`frontend/src/views/`, and `docs/ARCHITECTURE.md`. Marketing-level descriptions
alone do not establish a wire contract or complete interoperability.

## Existing Plamenu capabilities to retain

Plamenu already provides accounts, registration policies, OAuth client grants,
MFA, profiles, follow requests, notifications, feeds, search, replies, quotes,
boosts, favourites, hashtags, mentions, content warnings, public/follower/direct
visibility, local-only posts, media upload/probing/transcoding, guarded remote
fetches, reporting, blocks, mutes and domain moderation. Native video channels,
video playlists, video watch history and video analytics are separate models.
They do not establish equivalent audio/music features.

The music experience must use Plamenu's server-rendered forms, progressive
JavaScript enhancement, Fluent translations, ordinary post interaction bar,
transactional delivery outbox and existing media authorization. A separate SPA,
second account system or a second ActivityPub identity for each listener is
unnecessary. Music metadata is a status sidecar so canonical identity, reply
threads, source edits/deletions and moderation remain authoritative.

## ActivityPub contract

The source emits ordinary `Person` actors and social `Note` posts, native
`Audio`, structured `Track`, `Artist` and `Album` documents, and `Library`
collections in the Funkwhale vocabulary. A published track and its music
metadata are related resources, not interchangeable actor identities.
`attributedTo` can be an array whose first member is the publishing actor and
whose later member is an artist resource. Audio has multiple `Link` renditions
and may also have attachments. The structured `track` carries artists, album,
position, disc and MusicBrainz identity. Libraries carry paginated `Audio`
items and support object-scoped follows. Per-publication Audio IDs survive
metadata edits; republishing after a Tombstone uses a new identity.

Plamenu must preserve origin ownership for every imported item. An artist
resource must not become a social account merely because it appears in
`attributedTo`. Imported collections are references to origin objects. Reading,
favouriting or adding an item to a personal collection must not silently
republish it under the listener's identity.

| Contract | Prior Plamenu surface | Work in this change | Remaining acceptance |
| --- | --- | --- | --- |
| Person discovery, signed Follow/Accept/Undo | Existing generic AP support | Add real Songhive peer and exchange coverage | Full live matrix on the final implementation |
| Note Create/Update/Delete, replies, likes, boosts, quotes | Existing generic AP support | Retain the existing paths | Dedicated Songhive interaction/quote-policy cases |
| Receive native Audio and playable media | Existing native-object ingestion | Retain structured artist/album/position/year/MBID metadata | Remote Audio edit/retraction and rendition cases |
| Author Audio Create/Update/Delete | Previously absent | Explicit Audio composer/API kind; one processed audio attachment; stable type and identity; atomic outbox | Full live and browser verification |
| Track/Artist/Album dereference | Previously absent | Embedded structured documents and protected canonical subresource URLs | Canonical owner-scoped reusable entities, source-aware pages and revisioned metadata editing are implemented; artwork is implemented with protected serving, current source ACLs and atomic updates; final live evidence remains open |
| Library discovery, pages and follows | Previously absent | Actor advertises an implicit public Library; bounded Audio pages, separate scoped Follow/Accept/Undo records and public Audio delivery | Verify real scoped exchange; named Library management and public AP discovery/scoped deliveries are implemented; remote collection import remains open |
| Public/local/private music and explicit sharing | Generic post visibility and ordinary bearer attachment URLs | Audio metadata inherits post visibility; ordinary attachment serving is retained | Dedicated creator-private asset authorization and revocable collection grants/tokens |
| RSS/Atom, embeddable audio, link-back notifications | Profile Atom and generic media exist | No music-specific claim | Music feeds, music embeds and bidirectional Webmentions |

## Full product inventory

Reference behavior must be confirmed in the pinned source. A route, model or
marketing description alone does not establish an implemented feature. For
example, Songhive 0.4.9 stores radio configuration and applies ACLs, but
`api/routes/radios.py:get_radio_tracks` returns an empty list. Radio records and
access controls belong in parity; a functioning generator is additional work.

Every row below is required if the parity target includes all Songhive product
features. Rows with an existing Plamenu counterpart still require behavior
comparison; names alone are not evidence of equivalence. The present implementation covers Audio publications, implicit public libraries
and independent music resource subscriptions. It does not complete the table.

The user explicitly excluded payment integrations on 9 October 2026. Stripe,
sales, paid memberships and payment-derived media entitlements are out of scope.
The user also excluded third-party service integrations on 9 October 2026:
provider connectors, hosted scrobbling and metadata enrichment services, external
storage backends, Icecast/Snapcast output adapters and the external Mopidy
extension. Native HTTP streams, local-file import, Subsonic/OpenSubsonic APIs,
podcasts/feeds and ActivityPub music interoperability remain in scope.

One Music/Audio entry in the main menu must open a unified native workspace
for music, radio, podcasts and audio collections. Browsing, search, publishing
and management belong inside that workspace, following the existing Videos
product pattern. Do not add a separate main-menu entry for each audio type;
backend endpoints alone do not satisfy this requirement.

Source implementation and the pinned native contract review are complete; final runtime, interoperability and browser acceptance remain open. During implementation, run fast static
checks and tests selected for the changed behavior and its direct dependencies.
Reserve the complete workspace test, browser and federation matrices for the
feature-complete candidate. Collect all failures before repairs; repeat only
failed cases and checks affected by a subsequent change. Benchmarks run last.


| Reference feature family | Plamenu design | Completion evidence |
| --- | --- | --- |
| Upload/import tracks, tag extraction, content deduplication | Music assets retain original audio, extracted tags and an audio-only hash; per-owner references reuse bytes without reusing access rights | Real tagged uploads, duplicate imports, quotas, failed imports and cleanup |
| Artists, albums, artwork, genre, tags, year, disc and track order | Stable reusable music entities; structured metadata edits; link public entity pages from Audio cards | Multi-disc ordering, duplicate artist names, safe artwork and metadata changes |
| Browse/search public and personal music | Music navigation and catalogue, then independently paginated artist/album/library views | ACL-aware search, deleted/blocked sources, mobile and no-JS navigation |
| Local/public/creator-private libraries | Named music collections; private means creator-only, distinct from the post API's follower-only `private` | Anonymous, owner, local member, remote follower and granted-member access matrix |
| Collection management and remote music collection | Ordered references to canonical local/remote tracks; unavailable entries do not reveal hidden metadata | Add/remove/reorder, concurrent revision conflicts, origin deletion and revoked access |
| Playlist covers, tags, statistics and M3U export | Same ordered-reference model; no-JS forms; exports issue only URLs the requester may read | Stable order, escaped export titles, private export refusal and source revocation |
| Persistent player queue, shuffle, repeat, previous/next | Durable account/device queue with revision checks; progressive persistent player; explicit user gesture starts playback | Reload/navigation, competing devices, queue mutation and invalidated sources |
| Radio records and access controls | Owned radio records, saved configuration and authorized browsing; the pinned `api/routes/radios.py` generated-track endpoint returns an empty list | Radio creation, owner/private/public access and collection membership; functional track generation is absent from the reference and is not required for parity |
| Browser streaming, seek and downloads | Reuse guarded same-origin media serving; preserve HTTP Range; never fetch a source from an arbitrary player-supplied URL | Decoded audio, partial-content ranges, cancellation and remote fetch failure |
| MP3/OGG/FLAC/AAC/Opus transcoding and bitrate caps | Audio rendition jobs through the existing processing workers with per-role/user limits | Codec decode, bitrate enforcement, cache eviction and resource limits |
| Persistent native HTTP streams directory | Owned server playback outputs, bounded listener fan-out, output authorization, dedicated worker | Multiple real listeners, tab closure, restart, listener limit and disconnect |
| Icecast output and Snapcast casting | Excluded third-party service integration | No implementation or service gates required |
| Listening history, favourites and per-track/album/artist stats | Explicit play reports and deduplicated listen events, distinct from requests for bytes; opt-out and erase | Prefetch never counts, replay deduplication, threshold and time-zone tests |
| Last.fm/Libre.fm scrobbling | Excluded third-party service integration | No implementation or service gates required |
| Subsonic/OpenSubsonic | Authenticated compatibility router over music services; dedicated revocable tokens; negotiated JSON/XML | Real client browsing, playlist/queue operations, seek, API-key and salted-token authentication |
| Mopidy browsing and playback | Excluded third-party service integration | No implementation or service gates required |
| Podcasts and episodes | Guarded RSS/Atom catalogues, scheduled refresh, origin enclosure streaming, private subscriptions and played state | Real feed producer, enclosures, malformed XML, updates and unavailable episodes |
| Per-user sharing and revocable short links | Explicit track/collection grants and hashed revocable token rows; common authorization on every rendition/asset | Grant/revoke, expiry, cached URL refusal and anonymous preview playback |
| Embeds/widgets/iframes | Dedicated public music embed and player controls; restrict cross-origin serving to the intended public-media contract | Real external-origin embed, captions/transcripts, private refusal and CORS checks |
| Profile/artist/playlist/library/tag/genre RSS and Atom | Feed serializer over the same authorized catalogue; discovery links on entity pages | Valid XML, conditional requests, private collection refusal and item updates |
| Incoming/outgoing Webmentions | Bounded guarded discovery/delivery, explicit public targets and moderation | Real external producer, verification, retries, redirects and unsafe-origin refusal |
| External local folders and write-back | Operator-approved folder roots; reconciliation/watch jobs; intentional tag write-back; no raw path access from browser inputs | File create/update/delete, symlink escape, large sync and rollback |
| S3, SFTP, WebDAV, Dropbox and Google Drive libraries | Excluded third-party service integration | No implementation or service gates required |
| Jellyfin, TIDAL, YouTube and other entity-backed providers present in the pinned source | Excluded third-party service integration | No implementation or service gates required |
| Metadata enrichment via MusicBrainz and Cover Art Archive | Excluded third-party service integration | No implementation or service gates required |
| Local primary file storage | Reuse existing protected local media storage and cleanup debt | Retained asset and cleanup tests; final validation remains open |
| S3 primary file storage | Excluded third-party service integration | No implementation or service gates required |
| Bulk ZIP downloads and progress | Bounded durable export jobs over a fixed authorized source revision | Mid-job revoke/delete, cancellation, cleanup and large archives |
| Stripe track/album sales, memberships, Connect and gated media | Excluded by user instruction, 9 October 2026 | No payment implementation or payment gates |
| User notifications, email preferences/digests, WebSocket and Web Push | Extend existing notification/settings services with music events and digest preferences | Per-type opt-out, retry/deduplication, targeted delivery and digest scheduling |
| Profile privacy, follower approval, moderation and user reports | Existing account/policy services extended to music containers and assets | Every read, feed, search, embed and remote-delivery path respects suspension/defederation |
| API tokens, sessions, OAuth, MFA and account lifecycle | Reuse existing account services; music client tokens get explicit scopes and revocation | Restricted-token denial, session revocation, recovery and account deletion |
| Admin users/settings/invites/audit/task queues | Existing administration plus processing/output/import/export/scrobble job observability | Operator-only controls, cancellation and clear actionable failures |
| Theme, locale/RTL, responsive navigation and PWA | Existing design with translated music controls; explicit offline behavior for a player | All supported browser engines, 320px, no-JS, keyboard and locale tests |

## Data and service boundaries

The current `status_music` table holds bounded descriptive metadata in typed columns.
Audio posts still use the existing `statuses`, `media_attachments`, ownership,
visibility, edit history and delivery jobs. Non-audio timeline pages do not
query the music sidecar; Audio metadata is loaded once per render batch.
Deleting a status does not create a second canonical publication or a surviving
public music document.

The complete library design requires separate durable assets and collection
rows rather than overloading video playlists. Existing `media_attachments`
reference a publication with `ON DELETE CASCADE`. They cannot be the durable
track record: withdrawing an Audio publication must preserve its private
original track, while deleting the track must revoke every publication and
rendition. Music assets therefore need independent storage ownership and
revisioned publication references. Generic attachment cleanup must not delete
an original retained by a music asset.

The retained asset implementation includes owner-scoped artists/albums,
reserved non-bearer storage keys, tag extraction, decoded-audio deduplication,
optimistic revisions, creator-private/local/public access, revocable member
grants and protected original/playback routes. Per-account/global storage debt
remains until cleanup is acknowledged. Typed Library/Playlist containers retain
ordered local/verified remote references, separate grants, revision checks,
source-aware pages and M3U exports. Explicit publication and withdrawal bridge
retained assets to ordinary Audio without replacing their canonical identities.

Implemented music surfaces also include reusable entity metadata and artwork,
track descriptions/credits and owned album selection, device queues and a
persistent player window, explicit replay-safe listening history, private
favourites with local notifications, listening statistics, radio station records
with owner-only configuration, and full readable entity/collection counts and
durations. Empty owned albums count independently; playlist repeats count as
separate occurrences and hidden sources contribute nothing.

The totals candidate passed the focused backend checks, all 21 selected
library/player/radio browser journeys, and all nine selected real Songhive and
Mastodon response-shape cases. Account lifecycle fixes additionally erase empty
music metadata and personal references while retaining physical cleanup debt,
include music-only subscribers in actor lifecycle deliveries, and retract
origin-owned cached music individually. Five dedicated erasure, rollback,
subscription-scope and concurrent-Follow regressions pass. The corrected account
chooser and deletion-cookie handling preserve other live sessions and remove
permanently deleted entries, including stale browser rosters. All 124 selected
backend checks, all 28 selected browser journeys and all ten selected real
Songhive/Mastodon-shape cases passed on this lifecycle candidate. Scheduled
suspension expiry now uses the same atomic music retraction and erasure
transaction, serialized with account restoration. A failed delivery leaves
the suspended account recoverable; restoration winning the lock cancels the
purge. All 130 selected backend checks, 28 browser journeys and ten real-peer
and response-shape cases passed on the expiry candidate.

Explicit collection contents sharing is implemented as a separate owner choice.
Current parent access, membership and matching track ownership determine derived
access, without standalone grants or Audio publication. Revocation preserves
independent direct and other-parent grants. Six dedicated contents-sharing
regressions and the existing focused backend checks pass; all 28 selected
browser journeys pass. Nine real-peer/shape cases passed in the complete selected
run, and the remaining updated Songhive case passed after correcting its client
to use anonymous, strictly verified playback requests.

Personal saved references and the My Music union are implemented in source.
Owned resources, explicit saved references and music favourites remain independent;
related artists derive only from currently readable collected tracks and albums.
Saved metadata resolves from its current source, and unavailable references can
be removed without recovering permission to read the source. The current 150
distinct targeted backend checks pass after correcting one note fixture. The
new candidate passed all 28 browser journeys. Nine real-peer/shape cases passed
in the complete selected run; the remaining Songhive round trip passed after
the fixture explicitly retained a reply stub and checked its deleted placeholder.
That case verifies current saved metadata after origin updates, unavailable
reference removal after an origin Delete, and independent favourite membership.

Independent album access controls are implemented in source: creator-selected
private/local/public or legacy track-derived details, paginated local-member
grants, and separately opted-in sharing of matching owned tracks. Public Audio
credits retain album names and years while private notes and tags are removed.
Readable tracks can still inherit private-album covers. Empty public or granted
albums admit their associated artist metadata without granting private audio.
The current 157 selected music, native Audio, account-session and clock checks,
and five database-lifecycle/source-guard checks pass. Clippy is green. All 28 selected
browser journeys passed across three engines, narrow screens and no JavaScript.
Nine of ten selected real-peer/shape cases passed. The remaining test exposed a
Songhive 0.4.9 limitation: inbound Delete retracts materialized activities but
leaves plain Album cache rows available until a guarded origin refresh. Signed
canonical album fetches already return unavailable; the corrected focused
fixture remains pending. This is not a completed interoperability result.

Revocable share links and member sharing notifications are implemented in source.
Eleven focused sharing cases passed across the selected runs, covering byte ranges,
expiry, current parent membership, playlist repeats, foreign-track restrictions,
API scopes, artwork refusal, one-time native forms, notification rollback and
retraction. Explicit JSON previews return the scoped playback URLs. Migration 157
is not applied to the standing development instance; new sharing browser and
interoperability evidence remains open.

Native podcast subscriptions, bounded RSS/Atom parsing, conditional refresh,
OPML import/export, private played state and native Music workspace forms are
implemented in source. Migration 158 remains unapplied to the standing instance.
Six podcast service/native cases, two bounded parser cases and four generic
transport cases pass in focused runs. Episodes also join existing playlists,
device queues and elapsed listening reports through typed references; 28 focused
collection/queue/listening/podcast checks passed. Same-origin enclosure playback
supports byte ranges and cancels the origin request when the response is dropped.
No completed podcast runtime/browser result is claimed. External podcast sync services are excluded.

Server outputs, remote collection import and additional renditions are implemented
in source with focused evidence below. Local-folder operations, compatibility
APIs, bulk archives and the remaining native inventory are still open.
These targeted checks do not establish full product parity or a final full-gate
result.

Named collections contain references to those assets or verified remote
resources. Membership alone is not an access grant. Public collections must reject
or hide inaccessible entries without disclosing their titles, artwork or URLs;
creator-private collection visibility alone does not make an otherwise public
asset private. Provider/import, playback, export and sharing routes must use
the same asset authorization decision. Proposed models are music assets
and renditions, artists, albums, ordered collections and entries, scoped library
follows, share grants/tokens, per-device playback queues, explicit listen events,
provider configurations, import/export jobs, playback outputs, encrypted
scrobble sessions/jobs and podcast subscriptions/episodes. Payment models are
excluded from this task.
Each mutation must commit its own data and any federation jobs atomically.

Ordinary attachment filenames currently act as bearer URLs. The shared
`public_file_allowed` gate revokes moderation/cleanup targets and unpublished
native Video outputs, but does not apply ordinary post visibility to attachment
bytes. Audio status metadata visibility must not be presented as a creator-only
music asset ACL. The durable music layer needs its own protected serving routes
and storage namespace before creator-private libraries can be implemented.

Access to a collection must not implicitly bypass an item's own authorization.
Creator-private music sharing therefore requires an explicit asset access rule,
not adding private posts to a public collection. Changing visibility or revoking
an entitlement must govern original bytes, transcoded bytes, download archives,
artwork, transcripts, feeds, embeds, caches, streams and provider redirects.

Remote resources use the guarded federation/HTTP clients and bounded fetches.
Only verified-origin Create/Update/Delete may replace origin-owned metadata.
Remote artist/album/library links never trigger unbounded recursive imports.
Scoped library followers must receive music deliveries without acquiring a
social follow or access to unrelated follower-only posts.

## Verification order

1. Formatting, static checks and all-target Clippy.
2. Focused metadata, publication, rollback, ACL and pagination tests.
3. Complete portable Rust tests, rustdoc, SQLx prepare verification, dependency
   policy, documentation checks and harness policy tests.
4. Real Songhive exchange, then the entire required interoperability matrix.
   Missing required peers, deselections and unexplained skips are failures.
5. Full browser regression plus direct manual QA of the affected music pages.
   Screenshots support review but do not replace playback or interaction checks.
6. Calibrated serial/query-count budgets and contention benchmarks, without
   competing builds or tests, after correctness gates.

A green Audio exchange does not establish library, external-provider,
Subsonic or full product parity. Payment integrations are excluded. Mark a row complete only after its implementation
and corresponding evidence exist. Any partial gate, skipped provider or
unimplemented row remains explicit in the completion report.

Podcast source now also includes OPML file upload, non-UTF-8 XML decoding,
page queue append/replace, selected-episode played controls and atomic collection
addition, current same-origin artwork, and unknown-duration resume positions.
These controls remain inside the single Music workspace. Focused Clippy is green
through the artwork/queue candidate; new selection/refresh tests and focused
browser verification remain open. The standing runtime remains schema 156;
source migrations 157–159 have not been applied there. This is not a completion
or manual-QA claim.

The podcast controls candidate passed nine focused checks across the initial run
and one corrected fixture rerun, plus two isolated Chromium browser journeys
(with JavaScript and without it). The browser producer serves real RSS, audio
and artwork; elapsed reports and native controls are exercised. Screenshots of
show, episode, queue and episode-containing collection pages were visually
reviewed. Visual review found an empty metadata separator and crowded bulk
controls; source now omits missing metadata and uses the existing spaced form
layout. That cosmetic follow-up is not yet represented by the captured images.
This evidence does not complete the remaining native product families, the final
browser matrix, or manual interactive browser QA.

Native collection RSS/Atom syndication now passes three focused checks (XML
round trips, access before conditional responses, typed podcast references),
and profile RSS/Atom passes a focused check with real native Audio metadata,
private posts, blocks and account retirement. Fixture repairs were rerun alone.
Collections and local profiles advertise RSS links; collection pages also
advertise Atom. The artist release feed passed its focused check: readable
albums suppress standalone album tracks, hidden albums leave independently
readable tracks visible, and newer private records do not consume public limits.
Exact genre/tag browse pages and feeds now pass their focused checks. The
10-case feed family passes XML round trips, current permissions and ETags, mixed
tagged posts, exact capability child scope/revocation, and bounded large podcast
notes with metadata-edit timestamps. A follow-up processes duplicate episode
captions once per feed; the large-description and Unicode/path follow-up checks
pass together in 1.43 seconds. The shared artist batch privacy/query-count check
also passes. The two isolated Chromium journeys pass, with and without JavaScript, including real RSS/Atom parsing and exact shared collection feeds. Their screenshots were visually reviewed; interactive manual QA remains open.
Entity activity feeds remain open.

The source audit confirmed library/playlist tagging as an additional native
requirement. Source now has bounded normalized collection tags, owner-only
revisioned add/replace/remove APIs, ordinary native forms, and current tag search.
Its three focused checks passed, including normalization and no-op revisions,
owner-only mutation and stale revisions, and filtering before pagination. There
is no new runtime or browser claim.
Reference favourites are tracks or cached remote objects; album/artist/library
membership uses the separate saved-resource model, already represented by My
Music. Radio records have no tags in the pinned reference.


Public music embed controls are implemented in source for tracks, artists,
albums and collections. They provide audio, small HTML track lists, Markdown,
script widgets and iframes, preserving playlist repeats and current anonymous
source permissions. Protected embed audio/artwork routes ignore account or share
capabilities. Root artwork, friendly unavailable frames, bounded widget resize
messages and exact local music URLs through the native oEmbed provider are
implemented. Four focused embed cases and four directly affected existing
framing/oEmbed checks pass. The native legacy-album/private-artist link check
and public-media CORS check also pass. External publisher journeys pass on the
polished candidate, with no JavaScript and with real cross-origin byte reads.
The JavaScript fixture header error was repaired and rerun alone. Final manual
QA and full matrices remain open. Entity embed links follow anonymous root
availability; the editor retains signed-in navigation while frames stay anonymous. This does
not complete the remaining native families or start final full gates.


Native rendition implementation is in progress. All five encoder formats have
passing real-container checks; source-aware Vorbis negotiation preserves channels
and sample rate and refuses impossible bitrate ceilings. Source now includes
persistent single-flight jobs, bounded queue/concurrency/scratch admission,
generation-specific output keys, requester access checks during encoding,
protected Range serving, and cache quota/cleanup debt. Eight distinct durable worker/cache checks pass across the initial run and the selected follow-up, along with the directly affected upload/quota check. Migrations 162–165 are source-only; the standing runtime
remains schema 156. Native format controls pass two isolated Chromium journeys (JavaScript and no JavaScript), including all five codecs, decoded MP3, current public-to-private byte access and owner cache removal. Six screenshots were captured and visually reviewed. Exact capability rendition APIs and native forms now pass three new focused checks (private preparation and revocation, principal retirement without anonymous fallback, and exact collection membership/foreign-source refusal). Their two isolated Chromium journeys (JavaScript and no JavaScript) pass, including real private FLAC/MP3 preparation, lossless labelling, MP3 decoding and revocation. Screenshots were captured and visually reviewed. Cold streaming is now implemented for retained local assets, including exact shares, leased listener limits, cancellation on disconnection and protected cache publication. Nine new focused checks pass, including real progressive decode of all five codecs and active share/visibility revocation. Native Play now controls pass two isolated Chromium journeys: MP3 with JavaScript and exact shared AAC without it. Both decoded audio before the real encoder finished and then served cached ranges; screenshots were captured and visually reviewed. Anonymous embed audio endpoints accept codec parameters with their current anonymous permission gate, and the new focused denial/cap/CORS test passes. Vorbis, FLAC and Opus also pass actual cold browser decode, completing five physical codec checks across the two focused browser runs. Embed editor/frame codec controls now pass the new focused check and two real external publisher journeys (Opus script widget with JavaScript and iframe without it), including anonymous caps, decoder playback, cached ranges and revocation. The pinned frontend has no global codec preference: remote attachments and RSS episodes use their explicit stream URL unchanged. Its native codec endpoint transforms local stored tracks; approved local-folder imports will reuse the retained asset pipeline, so this is not a rendition parity completion claim.
Full gates remain deferred until all native families are implemented.

Tag and genre browser pages now use bounded typed keysets instead of stopping
at 100 results. The focused regression reaches every source in a three-page
catalogue, preserves resources sharing numeric IDs and rechecks visibility
between requests. Existing term-feed checks pass; RSS/Atom retain chronological
bounded feeds. Two new isolated Chromium journeys pass (JavaScript and no
JavaScript), including native next-page navigation at 320px and four captured
screenshots. This is focused evidence; the final full matrices remain deferred.
The direct bitrate-authority regression also passes: permission bits govern
member/moderator/admin ceilings, role names cannot elevate playback, and instance
and requested ceilings still apply.

Remote resource lookup and private remote Saved membership are being implemented
in source. A separate cache retains canonical URLs and verified controllers;
imports do not create local catalogue entities, social follows or public library
membership. Embedded resources with another controller are independently fetched
at their canonical URL before trusting their metadata. Five new focused backend
checks pass, including real signed inbox Update/Delete and stale-fetch ordering;
two parser checks and twelve directly affected Saved/My Music checks also pass.
Remote library contents, collection expansion, playback and real-peer/browser
acceptance remain open. Schema 166 is source-only; the standing runtime remains
at schema 156.

Remote lookup now loads one guarded first library page, matching the reference's
lazy discovery. It validates the page's identity, parent, controller and public
addressing; cached contents fold Audio renditions into canonical tracks before
pagination. The current twelve focused remote backend checks and four parser
checks pass, including canonical artist/album authority, inaccessible album
redaction without hiding a public track, same-origin permalink discovery, native
CSRF controls and API read/write separation. Protected streaming is implemented
in source and under focused validation. Deeper page lookup, remote metadata and
playback integration, real-peer/browser acceptance and the remaining native
families are still open.

Remote protected playback now has passing focused Range/body, active source
withdrawal, token revocation and finite-listener checks. Derived Track metadata
retains its nearest verified Audio proof, so withdrawing Audio hides its Saved
Track without hiding independently served album metadata. Explicit deeper page
lookup and native Load more pass their focused parent/origin/next-page checks;
no automatic crawl follows the next pointer. Typed remote metadata, protected
artwork, actual raster decode, changed artwork during origin fetch and active
OAuth revocation pass the new focused checks. Raw artwork URLs stay server-side;
remote documents never become locally owned catalogues. These are focused
implementation results, not final real-peer/browser or parity acceptance.
Remote collection/player integration and the other native families remain open.

Remote canonical references now join persistent device queues, private favourites
and explicit listening history. The four new focused cases pass, as do six
selected local music/podcast dependency cases. Remote statistics also pass the
new numeric-namespace regression and selected calendar, canonical-publication
and query-count checks. Inherited Audio audiences now depend on a bounded live
proof chain; the new library-withdrawal test preserves independently public
Audio while denying inherited metadata and bytes. Cycles and changed proof types
cannot authorize old derived metadata. These remain focused source results;
legacy library aliases, collection membership, multiple rendition proofs,
real-peer/browser acceptance and the remaining native families are still open.

Native HTTP outputs are implemented in source migration 170. The single Music
workspace includes Streams and a playback destination chooser. Durable worker
leases, queue revisions and checkpoints govern server playback independently of
browser playback. Private listener links are shown once, support query and
Bearer capabilities, and grant bytes without becoming account API credentials.
Real FFmpeg tests cover MP3, Ogg and Opus, late Ogg/Opus initialization,
podcast/verified-origin inputs, opt-in ICY metadata, listener revocation and
HEAD requests without admission. Sixteen distinct focused output checks passed
across the selected runs; all seven directly affected queue checks passed.
Worker recovery and seeking preserve playback intent and do not create an extra
listening session for a seek. Role ceilings and failed encoder cleanup are
covered. This is source and focused service evidence: migration 170 remains
unapplied to the standing development instance, and actual process recovery,
manual browser QA and the final interoperability gates remain open.

Private bulk ZIP downloads are implemented in source migration 171. Jobs retain
source references and container revisions, expose progress/cancellation/expiry,
reserve storage through the existing music quota and cleanup journal, and use a
bounded streaming writer. Music pages offer container downloads and track
selection. Download bodies enforce current source and token access, including
ranges and revocation after response headers. Completion notifications commit
with the ready artifact, use private-safe references in both notification APIs,
and retract with the download. All eight focused archive cases passed across the
selected runs, alongside affected asset quota/erasure and sharing-notification
checks. Native forms use real browser login sessions. The latest progress
markup/JavaScript passed Clippy and syntax checks; timed browser behavior remains
unverified. Migration 171 has not been applied to the standing runtime. The
pinned archive service builds ZIP; TAR is not an implemented reference feature.

### Native client compatibility candidate

The pinned Subsonic reference registers 41 structured methods and four media
endpoints. The candidate maps these to Music services, with dedicated revocable
client credentials, XML/JSON/JSONP, form POST, current catalogue access, atomic
playlist edits, private favourites, explicit completed-play reports and
consented now-playing sharing. Reference methods for ratings, play queues,
videos, bookmarks and lyrics retain their documented no-op/empty behavior.

Focused checks cover all structured method families, native credential forms
and lifecycle, scopes, current private sources, repeat/order/replacement, play
opt-out/replay, real original WAV bytes and ranges, artwork/avatar serving,
revoked bodies, and an actual native worker producing decodable MP3 under the
role bitrate cap. These checks are source-level acceptance. Latest runtime,
actual-peer interoperability, browser QA, full final gates and benchmarks are
still required; this is not a complete-parity declaration.

Mounted-folder implementation is in progress in source migration 174. The
original resolver now covers Subsonic, native and shared downloads, ZIP jobs,
HTTP outputs and rendition inputs using held file descriptors. Operator roots
and native library assignments have bounded durable scans, periodic watching,
PCM deduplication, per-file failures, missing-file detection and removal
tombstones. Main-menu navigation remains one Music entry.

The focused scan batch passed 12 cases: three filesystem/configuration unit
cases, three folder lifecycle/permission cases, the mounted original read and
cleanup case, physical MP3/OGG/Opus output cases, and the two affected ZIP cases.
Source-mutation and mounted-publication tests are being added; their current
code, actual runtime, browser interactions, artwork and final interoperability
acceptance are still pending. Migration 174 is unapplied to the standing
schema-156 runtime. This evidence does not establish completed parity.

Mounted source mutations now pass the focused real-file case (owner and operator
capabilities, rename without clobber, tag remux with unchanged decoded PCM,
explicit deletion). Mounted playback and publication also pass the actual CLI
worker/FFmpeg decoding case, alongside all four existing publication checks.
The native writer holds its own storage-reservation fence without widening
ordinary reader access to incomplete files. Account purge's existing regression
passes with mounted originals excluded from deletion debt. These are source and
isolated database/codec checks; actual runtime and manual/browser acceptance
remain pending.

Embedded artwork now passes the real JPEG fixture for mounted and uploaded
originals, per-owner deduplication and current private artwork denial. Source
cover writeback passes physical MP3/FLAC/OGG/Opus/M4A cases with unchanged
PCM hashes. Source tag-only changes pass the current public Audio update case,
and later disk changes preserve an owner's native title override. Four directly
affected index/artwork/publication/edit checks also passed. Current selected
original container and stale-location paging changes are being verified; these
source checks do not replace final runtime, peer, or manual browser acceptance.

Webmentions are being implemented as a separate website interaction surface:
anonymous bounded form receipt, durable source/target identity and revision-fenced
verification, complete guarded HTML fetches, HTML/HTTP Link endpoint discovery,
outgoing public-post delivery and edit/deletion re-verification, current-public
resource checks, plain-text native cards and an owner archive in the Music
workspace. Website cards never create an ActivityPub account. This family still
requires focused acceptance, native interaction actions and complete notification,
privacy, actual-peer and browser verification; it is not a parity-completion claim.

Focused Webmention acceptance now includes 11 database/native cases, five bounded
microformat parser cases, three real HTTP transport cases and the changed Audio
publication/privacy transaction case. Native website replies/likes/reposts/quotes
are explicit public Notes with transactional website edges. Raster author photos
are decoded to bounded local JPEG/PNG responses; original photo URLs are omitted
from card DTOs. Cards on canonical libraries also appear on their native pages,
and archive cursors advance across hidden records. These checks cover source175
in isolated test databases, not a promoted runtime or real-peer/browser acceptance.

Native notification delivery now has per-kind in-app, email and daily digest
preferences, explicit read/unread/delete inbox controls, and a permanent mentions
archive combining local, federated and verified website mentions. Inbox defaults
preserve existing delivery; both email channels require opt-in and a current
confirmed active recipient. SMTP summaries link to a private email inbox, including
email-only kinds. The existing supervised SMTP worker reserves durable batches,
checks current channel/sender/audience state, fences stale claims and only stamps
receipts after SMTP acceptance. Account erasure cancels queued batches; ordinary
security-mail envelopes keep their existing behavior. Delivery is at least once:
a process failure after SMTP accepts a message but before the database commits
can cause a duplicate after retry.

Selected Clippy and 19 focused delivery/dependency checks passed, including real
local SMTP refusal/acceptance and the security-mail path. Explicit inbox checks
passed 3 new cases plus 2 read-state dependencies; permanent archive checks passed
4 cases plus its native website-action dependency. Seen-only retention, current
runtime/peer/browser acceptance and final gates remain on the completion checklist.

Seen-only notification retention is now configured through `[notifications]`
(defaults: 90 days, purge hour 3, digest hour 8; hours are UTC). Hourly maintenance
drains up to 2000 eligible old seen rows per tick after the purge hour, preserving
explicit unread overrides and permanent addressed-status/website mention records.
Website verification retains its first-announcement state, so edits do not
recreate a dismissed or expired notification. Five distinct retention/schedule
checks passed, with eleven affected Webmention and eight delivery checks. One
bulk fixture lacked mandatory reasons; only that failed case was rerun after
repair and passed. Runtime and final acceptance remain open on the checklist.


### Resource activity browsing (source family 177)

Native `/music/{kind}/{id}/activities` and authenticated
`/api/v1/music/{kind}/{id}/activities` expose exact artist, album, stored track,
collection, radio and verified remote-resource activity, with source/type filters
and keyset paging. Resource pages link to Activities inside the existing Music
workspace. Native cards reuse ordinary post interactions and current audience
checks; verified website cards remain separate from ActivityPub actors. Website
response edges retain the original entity target atomically with the authored
Note. Replies, accepted quotes, boosts and likes use native conversation and
interaction relations. Updates render current rows; deletion, undo and quote
revocation remove their current associations. Artist/album feeds do not aggregate
child-track publications or manufacture CRUD activity history.

Selected library/test Clippy passed. Six focused activity tests and the existing
atomic website-response and native subscription checks passed. The sole initial
failure was an anonymous-preview fixture omission; its corrected case passed
separately. Runtime promotion, actual-peer acceptance and manual browser QA remain
open in the tracked completion checklist.


### Final native read-side and operator review

Subsonic now-playing checks report credential expiry for own and foreign reports,
scans bounded pages beyond inaccessible rows before applying the visible result
limit, and batches current song rendering. Favourites choose canonical
representatives before paging, preventing legacy Audio/Track aliases from
reappearing on later pages. Native status attachment URLs share the guarded
music URL mapping used by ActivityPub.

Mounted mutations persist intent before changing originals, retain exact mount,
parent and inode proofs, coordinate scanner generations and recover across
process/database failures. Tag writeback uses the existing bounded encoder
launcher. Disconnecting a pending index safely cancels its uncommitted change.
Native music-job administration adds counts, active/pending inspection and
single/bulk audited termination through worker leases; filesystem recovery and
completed artifacts remain protected.

The [pinned native review](SONGHIVE_NATIVE_REVIEW.md) closes this implementation
inventory. Final immutable runtime, actual Songhive E2E, manual browser QA,
screenshots, full gates, benchmarks and the signed commit remain open.
