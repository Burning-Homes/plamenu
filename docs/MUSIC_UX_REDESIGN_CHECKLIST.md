# Audio usability redesign

> Mobile usability acceptance below was disproved by the user’s real phone
> screenshots. The follow-up [mobile correction](MUSIC_UX_MOBILE_CORRECTION.md)
> supersedes that acceptance and records the corrective checks.

## Scope and resumption

Requested 13 October 2026. Design for listeners and creators who do not know
ActivityPub and will not read documentation. Keep useful advanced capabilities
through progressive disclosure. One Music entry in the global navigation.
Payments and third-party provider integrations remain excluded.

Starting baseline: `2ab6dc900d6d`, deployed to staging as
`0.7.0-staging.237+sha.2ab6dc900d6d`; service active, zero restarts, migrations
through 180 successful. Previous owned runtime cleanup completed and restored
standing services before reboot. Reboot-time local availability is a separate
check; old process IDs are not evidence of current availability.

Resume at the first unfinished item. Update this file after each completed
vertical journey. Never describe a implemented screen as accepted until the
ordinary browser journey works. Earlier protocol/E2E success is not UX acceptance.

## Sequential stages

- [x] 0. Recover interrupted deployment; verify live version, service and schema.
- [x] 1. Audit normal-user browser journeys and inventory useful advanced features.
- [x] 2. Record the information architecture, primary actions and capability mapping.
- [x] 3. Implement discovery → listening → saving as a complete journey.
- [x] 4. Implement understandable library, playlists, radio/live and podcast journeys.
- [x] 5. Implement upload → publish → manage; retain advanced controls contextually.
- [x] 6. Accept desktop/mobile, keyboard, no-JS fallback and real Songhive playback.
- [x] 7. Run required final gates after implementation is complete, benchmarks last.
- [x] 8. Signed commit, staging deployment, verify deployed journeys and screenshots.

During stages 3–5 run Clippy first and focused affected tests. Do not repeat the
multi-hour full gates between unfinished features. Keep evidence under ignored
`.dev/songhive/music-ux-redesign-v1/`; publish findings and contracts here, never
credentials, local private configuration or private source URLs.

## Audit findings

| Journey | Observed baseline | Required outcome |
| --- | --- | --- |
| Enter Music | Empty post feed, two separate search/open forms, four primary pages, hidden 18-section menu | Content and understandable listening choices visible immediately; one search accepts names and links |
| Find outside music | Requires already knowing a Songhive URL; subscription is a separate competing action | Familiar search/browse, sources reusable after discovery; following optional |
| Open album | Activity/source links and metadata, save and collection management precede songs | Artwork, artist, prominent Play, songs; secondary actions later |
| Play a saved song | Opens separate player preview, then Add to queue, then choose track | Play means start listening; queue management remains optional |
| Continue browsing | Inline audio is lost on navigation; player requires learning a separate window | Explicit, consistent listening behavior; avoid silently stopping playback |
| Keep music | My music, favourites, saved manager, libraries and playlists overlap | Explain save versus like; a coherent library with contextual playlists |
| Live radio | Source exposes radio definitions and actual streams separately | Distinguish playable broadcasts from rules without requiring technical knowledge |
| Publish | Social audio composer and stored-track upload have different management routes | Clear upload/publish/manage path and visibility explanation |

The entry-page finding was reproduced in an ordinary authenticated staging
browser on 13 October: no content, “No audio publications match this view” plus
“Nothing here yet”, despite separate native catalogues existing.

## Acceptance contracts

- Enter Music without knowing a URL: see available content or useful next actions.
- Search names or paste a server/album/song link using the same obvious entry.
- Known album link → album → audible Play, with no Follow/Import/Save prerequisite.
- Track Play starts that track; does not require adding and selecting it manually.
- Saved content returns in the library and remains playable there.
- A live source is labelled playable/offline honestly; empty radio rules do not
  masquerade as broadcasts.
- Podcast subscription, episodes and listening are discoverable from Music.
- Upload, publish, edit and find one's own audio without learning storage models.
- Advanced capability inventory has a destination and concise contextual help;
  no capability is removed merely to simplify the default screen.
- Browser evidence proves decoded audio with advancing time and no media error.

## Advanced capabilities to map before redesign acceptance

Queue persistence, repeat/shuffle/reordering, named players and output selection;
playlist/library editing and permissions; favourites/history/statistics;
RSS/OPML and podcast progress; radio rules and live outputs; local folders and
file mutations; downloads/renditions/exports; scoped music clients;
shares/embeds; subscriptions and activity/mentions; artist/album metadata and
MusicBrainz identifiers. Operator job controls stay in administration.

## Next action

All eight stages are accepted. Source commit `ee1cc24ed4b7` is deployed as
staging build 238 with the qualified portable iteration/musl settings. Final
acceptance evidence and a 34-image gallery are under the ignored invocation
folder. Both original local runtimes were restored exactly and are ready; the
owned test runtime and podcast producer are stopped. Songhive peer services
remain running and healthy. No pending implementation work from this redesign;
use this completed record and member documentation for follow-up changes.

The checkout is actually `feat/ap09-native-video`; preserve that existing branch
and its user-authored video commit. No branch was moved, rewritten or pushed.
The unrelated user proposal remains untracked and untouched.

## Implementation checkpoints

13 October, checkpoint 1 (uncommitted):

- One hub search accepts names or links; shows public native stored tracks and
  authorized cached remote resources. Audio posts remain reachable. Known
  Songhive sites have reusable catalogue links, with no fan-out network search.
- Albums/artists include readable remote entries; Live radio includes remote
  broadcasts and links to known sites' live catalogues. Radio rules are secondary.
- Track and playlist Play is a CSRF-protected POST selecting playback immediately.
  Remote catalogue Play resolves the reference and validates current availability
  before replacing the queue. Queue-add preview remains under Queue options.
- My music has contextual playlists/likes/history and explains saving. Unavailable
  references, sharing and explicit lookup remain under Library tools.
- Player precise seek and destinations are under Advanced playback; disclosure
  state survives enhanced queue refreshes. Repeat/shuffle remain available.
- Podcast subscription is open on an empty page. OPML file import is secondary.
  Episodes have direct Play without first configuring a queue.
  A podcast website's advertised RSS/Atom feed is resolved automatically with
  at most one extra unsigned bounded fetch. Script/textarea markup is ignored;
  unsupported links do not cause a recursive crawl. The existing feed identity,
  episode history and current catalogue are preserved when joining via a website.
- Upload opens the file form directly, accepts optional title/artist/album and
  offers private Upload or explicit public Publish now. Existing metadata import,
  editing, artwork, access grants and social Audio composer remain available.
  Publication status explains where the audio can be found. If processing or
  publication fails, do not claim a successful publication; inspect the retained
  upload and surface recovery before acceptance.

Pending checkpoint acceptance: focused Rust tests, new ordinary-listener browser
journey and existing Music/advanced-player regressions; actual Songhive playback
on the candidate; verify user-facing errors and mobile layout. No final full
gates have been started for the redesign.

Focused evidence so far: 26/27 storage/publication/queue/saved checks passed;
the new combined listener test exposed a typed SQL error (fixed), then reached
its final block-policy assertion and exposed a missing block ID in the fixture
(fixed using the real block helper). Five other catalogue checks passed.
Current server/federation all-target Clippy is green. The expanded catalogue,
podcast and queue checks are running before creating the immutable browser
candidate. This is partial evidence, not a final acceptance claim.

## Chosen information architecture

Music remains the single sidebar destination. Its default view is a listening
hub: one search accepting names or links, available songs and albums, and direct
entry points for live radio and podcasts. Include both native stored audio and
social audio, and readable remote metadata already known to the instance. Never
invent a worldwide search index, silently contact every server, or suggest that
a metadata listing guarantees a still-public media file: playback rechecks it.

The primary workspace actions are Browse, My music and Upload audio. Play opens
the persistent listening window immediately through an explicit CSRF-protected
POST. An ordinary GET must not clear or select a queue. Keep inline controls for
single-page listening and no-JS fallback. The player provides a Browse link in
another window so browsing does not stop it; advanced destinations/precise seek
are secondary. Existing queue-add preview remains available as a secondary tool.

My music contains owned, saved and liked music. Playlists, likes and history are
contextual library links. Save means keep a reference in My music; liking is a
separate preference. Unavailable saved references are managed under library
tools. Upload opens the existing private-first storage flow with the form open;
publication, visibility and editing follow on the resulting audio page. The
social Audio composer remains available for users who want a post directly.

| Capability | Destination and presentation |
| --- | --- |
| Songs, albums, artists | Browse results, metadata and contextual artist/album links |
| Save, likes, playlists, libraries | My music; primary Save, contextual collections and secondary organisation |
| Persistent queue, repeat/shuffle | Player; basic listening first, queue below |
| Named players, outputs, precise seek | Player's Advanced playback disclosure |
| Live listening | Browse → Live radio; online/offline state, primary Play |
| Radio scheduling/rules | Advanced music tools → Stations; explain that rules require an output to broadcast |
| Podcasts, episode progress | Browse → Podcasts; subscription and episodes first |
| OPML, feed sorting, bulk management | Podcast tools disclosure |
| Upload/edit/publish/visibility/artwork | Upload → own audio page; audience and publication status explained |
| Folders, file mutations, scoped clients | Advanced music tools; contextual purpose and help |
| ZIP/downloads/renditions/exports | Audio/collection secondary tools and advanced download manager |
| Sharing, permissions, embeds | Relevant resource's secondary actions; advanced sharing manager |
| History/statistics | My music contextual links; filters within statistics |
| Subscription/activity/mentions | Resource details and advanced tools; optional for listening |
| MusicBrainz and technical metadata | Resource details/edit metadata disclosure |
| Operator jobs | Existing administration, unchanged |

## Browser checkpoint 2 — ordinary journey accepted, regressions being resolved

Candidate 5: all seven desktop/mobile/JavaScript-disabled ordinary-listener
journeys passed: optional metadata, one-action publication, album Play, safe
repeated upload, name search, My music, immediate playback, continued browsing,
and advanced playback controls. The core regression run completed non-fail-fast:
19 passed / 16 failed. Failures comprise a renamed membership expectation
(7), an inaccessible notification link (6), two narrow-screen overflows, and
one player fixture assuming an empty queue after another journey (1).
Membership assertions and explicit queue replacement were corrected; the
notification link is now underlined. Narrow-screen failures are being diagnosed
rather than hidden by weakening the assertion.

Manual candidate-5 playback of the user's actual Songhive album decoded its
234-second track, advanced past 45 seconds without a media error and continued
past 67 seconds while browsing. Browser autoplay permission required the native
Play control on this browser; no import/save/queue-add/track-selection step was
needed. The player was paused afterwards. This local manual account is an
operator; the automated journeys use ordinary member accounts.

Manual inspection also found social Audio cards still leading through queue
preview. Direct Play is now implemented even without optional music metadata;
Notes do not gain music actions and guests receive no queue mutation form.
Clippy 13 passed. The new renderer test and social-publication browser assertion
must pass on the next candidate before accepting this additional fix.

## Manual checkpoint 3 — ordinary member, candidate 7

Created a member only in the owned clone (no operator permissions). From Music,
opened the known site's catalogue and searched by artist. The reported album
URL resolved natively; album Play autoplayed a decoded 234.033938-second track.
Saving the album made it visible in My music while playback continued past
10 seconds. Real Fabio's Radio decoded and advanced past 26 seconds, error null.
Both checks were paused afterwards.

An owned synthetic podcast website advertised RSS. The first attempt hit an
old clone-only host backoff from a retired loopback producer; the policy was
preserved, and the owned producer was addressed through the existing local
hostname. Website → show → episode Play then decoded 9 seconds and advanced
past 4.48 seconds. Upload → Publish now also worked manually with that synthetic
audio, optional artist/album and a clear public publication result.

This inspection exposed final presentation issues being corrected: Live radio
was still titled Streams; two identically named album/artist cards lacked type
labels; RSS terminology led the podcast empty screen; upload's empty state
incorrectly said Audio publications; withdrawal/deletion/embed/download controls
competed with listening on one's audio page. These are being corrected before
feature-complete acceptance. Advanced controls and permission semantics remain.
Candidate 7 actual Songhive E2E completed: all 6 passed in 132.81 seconds.

The candidate-6 browser handoff failed before launching tests because the build
result was written before the immutable file copy completed. No browser cases
ran or count for that failed handoff. The corrected handoff waits for the
immutable candidate; core regression 2 runs on candidate 8. All failures must
finish and be collected before any affected rerun.

## Final presentation checkpoint — candidate 10

Candidate 9 source Clippy and build passed. Manual screenshots confirmed the
published-audio page presents publication/audience before listening; withdrawal,
deletion, embeds and downloads remain available in Details and tools. Native
sharing links remain contextual because sharing is also a normal social action.
Song/Album/Artist labels distinguish identically named search results. Final
layout adjustment groups workspace navigation, removes duplicate margins and
puts listening results before additional source-site links. Candidate 10 is
being built and will be accepted visually before final gates.

Core regression 3 is queued after regression 2 completes, using candidate 10
and the fixed navigation-aware sharing/podcast assertions and explicit queue
Clear. It includes the two affected external embed workflows; no duplicate live
browser runs are being launched. Keep all unsuccessful regression evidence;
only the final accepted identities can close the stage checklist.

## Implementation freeze

Candidate 10 Clippy/build passed. Its ordinary-member desktop view puts the
first Play at y=610.75–652.91 within the actual 720-pixel viewport (previously
below the fold). New navigation/results grouping is accepted visually. Stages
3–5 implementation are complete, supported by the seven ordinary basic journeys
and manual catalogue/album/save/live/podcast/upload evidence. Stage 6 remains
open for the corrected core/advanced browser matrix; stage 7 is final validation
of the frozen implementation, not an implementation checkpoint.

The IAB viewport override requested 320 pixels but read back 1280, so its
screenshot is not mobile evidence. Restore the override; use actual engine
mobile regression artifacts and verify their dimensions. Do not claim a mobile
manual result from an unsuccessful emulation request.

## Final gates started — collect all results before repairs

Frozen candidate 10: formatting, full-workspace/all-target Clippy, tracked syntax,
SQLx effective metadata and documentation passed. ShellCheck was absent from
PATH after reboot; the existing official v0.11.0 cached binary was verified
against its pinned archive and reused, and the identical shell checks passed.
No system package or user service was changed for this correction.

The security gate reports RUSTSEC-2021-0157 for ammonia 4.1.4, requiring 4.1.7
on its existing minor branch (or 4.2.3). Preserve the active non-fail-fast
workspace and browser runs, collect their outcomes, then apply the minimal
supported dependency correction and run necessary affected validation. Do not
waive the advisory or claim this gate green before the correction. Candidate 10
actual Songhive E2E completed successfully (6 cases). Native real-album manual
proof: duration 234.033938, time 7.271004, readyState 4, error null; paused at
13.054853. No benchmarks have started.

Regression 2 finished: 21 passes / 14 failures. All failures are the two known
fixture/UI-control issues across seven projects (hidden sharing state after
navigation and an absent podcast-only Replace checkbox in a single-song
preview). Regression 3 uses the corrected assertions and explicit queue Clear,
plus affected external embed workflows. Its outcome is still pending.

## Browser regression 3 findings — repair batch

The queue fixture tried to Clear an already empty queue, where no Clear control
is intentionally rendered. It now checks entries first; when clearing is needed,
it reopens the chosen-song preview after the HTML command returns to the queue.
This establishes state through visible controls, not an undocumented API reset.

One Firefox-mobile podcast run decoded audio but made no listening-start request
and never marked the episode Played. The deferred enhancement only listened for
a future Play event. Add an already-playing initialization check that starts
observing from the current position, never retroactively counting elapsed media.
The browser scenario now deliberately delays the real enhancement script,
starts native playback, releases the script, and requires an enabled real
listening session and a Played result. Test this repair with the security
patch after the current non-fail-fast runs finish; do not count it accepted yet.

## Final full-run outcomes and narrow repairs

Full workspace completed non-fail-fast in 1026.672 seconds: 3321 identities,
3312 passed, 9 failed, no skips. Collect all nine before rerunning:

- actor_reconciliation: held-old-origin case timed out at its two-second fence;
- hls_proxy: lifecycle-change case returned 200 for permanent mode;
- music_feeds: obsolete navigation-select assertion;
- music_jobs: four fixtures inserted JSON `{}` into the typed download-selection
  column introduced by schema 179. Build the fixture through the real download
  creation API instead;
- music_remote: obsolete content-heading and generic selection-label assertions.

The UI assertions now check current navigation, Tracks and actual selection
controls. Preserve all forged-member/CSRF/stale-revision checks. The two unrelated
behavioral failures will first rerun in their complete binaries with bounded
concurrency; do not weaken their assertions or claim timing without reproduction.

Browser core 3 completed: 49 cases, 39 passed / 10 failed. Seven queue fixture
failures, one missing podcast listening session, and two embed-revocation checks
sent requests before awaiting the visibility-edit navigation. The latter tests
now await actual HTML completion before demanding 404; revocation expectations
remain strict. The late-script podcast fix and conditional queue setup remain
unaccepted until their reruns pass.

Security correction: Cargo.lock changes only ammonia 4.1.4 → 4.1.7 and its
checksum; all other dependencies remain pinned. The patched version is supported
by the primary RustSec advisory. No application code calls the advisory's
filter_style_properties API. Repair source 1 includes full AP/federation package
checks, the complete failed binaries, affected music binaries, view/i18n checks,
workspace Clippy and a fresh security gate. Its immutable candidate is 11 only
if all repair checks pass. Preserve the 3312 unchanged successful identities;
final qualification is cumulative, not a fabricated fresh single-run baseline.

## Source repair checkpoint

Repair source 1 stopped at Clippy before tests: the fixture called a crate-private
constructor. Use the public Source fields; no application API visibility changed.
Repair source 2: workspace Clippy/security/build passed; 438 affected checks ran,
437 passed and one failed (2883 excluded by selection, not newly skipped tests).
Both unrelated actor/HLS cases passed unchanged in their complete binaries.
Only the durable recovery fixture still used the old JSON mutation-intent shape.
Its replacement uses a valid typed-record fixture and retains cancellation denial.
Repair source 3 runs the complete four-case music_jobs binary and rebuilds
candidate 11. Its first launcher had a syntax typo before any command executed;
corrected and compiled before relaunch. Browser core 4 waits for candidate 11,
then exercises all seven variants of player, podcasts and both embed workflows.

Secret scans: complete Git history and current tracked diff passed using the
verified pinned scanner. SSH signing was tested against the existing private
key without a resident agent; signing and signature validation passed. No key
material was copied into the repository. Commit signing must use that same key
pair and local verification; no unsigned fallback and no push.

## Acceptance repair checkpoint — candidate 11

Repair source 3 passed Clippy/security/build and all 4 music_jobs tests.
The complete 3321-test workspace run is qualified cumulatively: retain its
3312 passes, the 438-case repair (437 passed), and the final recovery-fixture
repair. Actor and HLS assertions passed unchanged; no behavior was waived.
Documentation rebuilt and linked successfully after the sanitizer update.

Browser core 4: 22/28 passed. Every player/embed identity passed; all six failures
were the new early-script podcast fixture waiting for preload before pressing
Play on preload=none audio. Correct native Play order; browser podcasts 5 then
passed all 7 variants, including the actual late-script listening-session and
played-state assertions. No fake media events or time advances were used.
The seven remaining advanced Music workflows now run non-fail-fast on the same
immutable candidate; preserve every failure before any repair.

Actual candidate-11 Songhive peer E2E: 6/6 passed. Ordinary-member manual inline
album playback: decoded duration 234.033938 seconds, currentTime 4.598095,
readyState 4, error null. Real radio: currentTime 13.936343, readyState 3,
error null. Podcast decoded and changed Unplayed to Played. Evidence snapshots
are in the ignored invocation folder. The IAB retained an error document in its
named player window after runtime replacement; its subsequent POST popup route
returned ERR_INVALID_RESPONSE. Do not count those attempts as fresh one-click
acceptance. Earlier ordinary candidate-7/10 one-click checks and all seven native
browser listener/player identities remain accepted; deployed QA must use a fresh
working player window. No browser security warning was bypassed.

One manual label correction: Songhive catalogue kind `stream` must render Live
radio rather than the default Song label. Candidate 12 changes only this match
arm, with Clippy and catalogue test/build qualification; earlier protocol/player
proofs are retained because those paths are unchanged. Benchmarks must use the
staging iteration profile, musl target and configured CPU flags, with no competing
build/tests, after all functional acceptance is green.

## Advanced matrix repair batch

Candidate 12 passed Clippy/security/catalogue tests/build, and manual stream
labels now show Live radio. The actual public album web POST was also checked
with a separate ordinary-member HTTP session: it follows the 303 to the selected
native player and returns 200. IAB POST-popup ERR_INVALID_RESPONSE remains a
browser-tool limitation; inline decoding is accepted, without claiming failed
popup attempts succeeded.

Advanced browser run 6 completed: 67 cases, 26 passed / 41 failed. All failures
collected before repair: 31 hidden Playback formats links, 7 hidden RSS links,
2 shared-format navigation races, and 1 cold encoding fixture lacking the
existing opt-in real-time encoder. Restore that invocation setting, open the
existing contextual disclosures and await completed job/edit/revoke navigation.
Keep Encoding-before-Ready, actual decoding, Range, strict revoked 404 and feed
content assertions intact. Run only the four affected workflows across all
seven variants (46 cases); retain 21 unchanged erasure/radio/tag-paging passes.

Python script/budget gates: 156 passed, 44 subtests passed; three scanner checks
were initially skipped because the pinned scanner was outside PATH. All three
scanner integration checks then passed with the verified cached binary exposed
only to that process. Strict dependency policy also passed with warnings denied.

## Final six browser identities

Advanced run 7: 46 cases, 40 passed / 6 failed. Feed/disclosure, sharing,
strict revocation and most rendition/cold cases passed. Remaining: Chromium
mobile Ogg reached Ready before the short fixture was observed; four desktop
WebKit cold cases pressed Rewind rather than Play on unloaded wide controls;
WebKit mobile audited during cached-format removal navigation. Preserve all
assertions. Cold-only fixtures now use real 30-second PCM rather than 9 seconds;
the ready wait reflects that longer input. Native test controls choose their
observed unloaded/metadata-loaded layout; no fabricated playback state.
Remove-cache POST now awaits HTML navigation before Axe. Resume the same
six-case, three-selection invocation, then aggregate all 116 identities.

## Functional acceptance closed

Advanced repair 8: Chromium-mobile Ogg 1/1, WebKit-desktop cold formats 4/4,
WebKit-mobile native renditions 1/1. All six passed. The cumulative Music matrix
contains all 116 expected identities, each latest status passed, zero skips.
Ordinary listening/publication/library, podcasts, queues, embeds, feeds, formats,
shares, erasure, radio rules and tag pagination are covered across desktop,
mobile and JavaScript-disabled variants. Initial failures remain in evidence;
this is cumulative qualification, not a fabricated fresh single-run success.

Source qualification retains 3312 workspace passes plus the exact failed and
affected repairs; all 3321 identities are qualified. Latest six catalogue checks
passed after the sole stream-label match correction. Python gates qualified all
159 cases plus 44 subtests after the scanner PATH correction. Source formatting,
Clippy, strict dependency policy, effective SQLx metadata, book/link checks,
ShellCheck and secret scans are green. Benchmarks have not yet been accepted.

## Performance invocation correction

The first iteration/musl invocation copied server-specific znver5 flags onto
this Intel i7-11700F. Both harnesses exited SIGILL before producing measurements;
the strict grader correctly rejected the stale older manifest. No timings from
that invocation qualify this source. Its owned PostgreSQL container/volume were
removed and the failure receipt is retained. The second invocation uses the
same iteration/musl profile with portable (empty) RUSTFLAGS. Deployment will use
the identical flags through a private per-invocation environment copy; the
operator's original staging configuration remains unchanged. Do not waive stale
manifest checks, redefine timing ceilings or relabel the failed run as passing.

## Performance accepted

The second invocation completed all 70 hot-path cases and all three contention
scenarios with strict grading: zero failures, warnings, known-bad cases or stale
measurements. Iteration profile, x86_64 musl and portable RUSTFLAGS match the
selected staging invocation. The owned PostgreSQL container/volume were removed
with identity/label checks. Timing ceilings and query/size limits are unchanged.
Measurements belong to the dirty tested source tree recorded by the receipt;
they are not represented as a clean release baseline or filed in release history.

## Final deployed acceptance

Source commit `ee1cc24ed4b73465707bbc5f571c7c9562aadcd0` is SSH-signed with
DCO and locally verified. The complete clean-tree quick gate passed after that
commit, including contribution/public-tree checks and full historical secret
scanning. The last documentation build/link check also passed.

Staging build 238 matches that exact clean source commit and local stripped
binary hash. Public health is 200; service active/running with zero restarts;
all migrations through 180 successful. A verified PostgreSQL dump, previous
binary/configuration copy and confirmed Incus snapshot precede deployment.
The deployment used a private per-invocation portable environment copy; the
operator's original configuration remains unchanged.

Ordinary staging member: Music shows available songs, artist/album type labels
and real live radio immediately. The reported album opens natively; its inline
player decoded 234.033938 seconds, advanced to 6.254115, readyState 4, error null.
Fabio's Radio advanced to 12.840919, readyState 3, error null. Both paused after
inspection. Saving the album made its native link visible in My music.
A nine-second synthetic recording uploaded privately, then its audience was
explicitly made public and Publish Audio created a native Audio post. The owned
track shows Published/Public before listening. Withdrawal, deletion, formats,
embeds, exports and source actions remain under Details and tools. The blank
podcast page presents its website/feed input immediately. Real podcast playback,
website discovery and progress acceptance are in the isolated seven-variant
matrix; do not claim an external podcast was followed on staging.

The IAB POST-popup error is retained honestly in manual evidence. Deployed
manual real-source decoding used native controls. Direct Play/persistent queues
passed the independent browser matrix, and the actual real-album web POST was
verified separately through its native player redirect. No browser error or
security warning was bypassed or relabelled as success.

Gallery: 34 actual images covering staging, ordinary creation/listening,
320-pixel browser layouts and retained advanced capabilities. Test media and
publications are clearly labelled QA. No remote-media download or mockup is used
as screenshot evidence.

Cleanup verified executable/configuration hashes, full argv/cwd/environment,
ready endpoints and original database schemas 156/140. Both standing services
run in independent units and survive controller exit. The owned test runtime,
producer and benchmark containers/volumes are stopped/removed; the Songhive
peer is healthy. Original standing TLS/configuration/media were not modified.

The resulting ordinary journeys do not require ActivityPub terminology or
queue setup: Music → search/link → album/song → Play; optional Save; Music →
Upload audio → Publish now; Music → Podcasts → website → episode. Advanced
capabilities are retained contextually and documented. Search covers known
readable content and a selected site's public catalogue; it provides no global
music-server index. The interface explains this boundary next to search.
The prior 390-case federation acceptance remains documented separately; this
redesign adds fresh Songhive peer checks and the full 116-identity Music matrix,
not a claimed new 390-case run.
