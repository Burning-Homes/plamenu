# Plamenu discovery implementation plan

Implementation plan and progress, 15 October 2026. Initial source baseline:
`ddcf8798eb2a6c4a8eb030e581497445d4c4afdd`.

Build discovery in small increments. First demonstrate that a member with no
follows can browse verified remote sources and eligible people. Then make source
acquisition continue independently of follows. Add public post search and more
content types after that foundation works.

The supplied `plamenu-discovery-implementation-plan.md` and
`plamenu-discovery-source-notes.md` provide the wider proposal. Their suggested
tables, adapters, quotas and ranking percentages are design inputs. This plan
narrows the first milestone and gives each change an observable completion test.

## First milestones

**Milestone A: one source works from end to end.** An operator adds a supported
server. A background job verifies it and reads a bounded public directory. A
signed-in member with no follows sees eligible profile cards, can open a profile,
and can choose to follow through the ordinary workflow. Revocation, expiry and
blocks already remove cards. This proves the mechanism with manual seeding.

**Milestone B: discovery works beyond the initial sources.** A small replaceable
seed list starts the process; bounded peer hints lead to additional verified
sources without member activity. The system survives restarts, respects its
budget, and explains empty or partial coverage. This is the first independently
useful discovery release. Manual browsing alone does not satisfy this milestone.

Proposed initial choices:

| Decision | Starting choice | Revisit |
| --- | --- | --- |
| Content | Sources and eligible people from a Mastodon-style directory | After milestone B; add one content family at a time |
| Audience | Signed-in members; discovery disabled until the operator enables it | Anonymous browsing needs a separate audience review |
| Integration | A new Discover page; existing People, Explore and client search keep their current contracts | After a usable prototype |
| Storage | PostgreSQL and a supervised in-process worker | After measured capacity problems |
| Metadata | Text cards with canonical links; no automatic avatar, thumbnail, audio or video downloads | Meter media separately later |
| Personalization | Existing blocks, mutes and dismissals where applicable; simple source diversity | Explicit topic preferences later |
| Missing discovery signals | Allow by default, independently configurable for profile discovery and public-post indexing; explicit refusals always exclude the corresponding use | Settled for step 1 |
| Public post search | Public visibility and the indexing policy below, through a separate query path | Before adding posts |

The missing-signal defaults were agreed on 15 October 2026. Other defaults remain
the proposed rollout settings; the broader cross-media ambition remains in the
later backlog.

## Eligibility contract

An absent signal uses local policy and defaults to allow for both `discoverable`
and `indexable`. Store absence faithfully; a locally permitted result does not
become a claim of explicit origin consent. An explicit false excludes the
corresponding use. Invalid fields and failed verification never receive the
missing-field fallback.

| Use | Required evidence and signals |
| --- | --- |
| Verification fetch | Enabled acquisition method and current access policy; this permits only a bounded attempt through guarded transport |
| Retain source facts | Verified public source facts with fresh evidence |
| Retain profile metadata, list or recommend a profile | Verified public profile and its `discoverable` signal or configured missing-field default |
| Retain public-post metadata or index its text | Verified public post, canonical author and the author's `indexable` signal or configured missing-field default |
| Recommend a public post | Both discovery and indexing decisions permit it, because the recommendation also promotes its author |
| Retain or search video metadata | Canonical public Video, anonymous local published REST detail, verified uploader and channel ownership; indexing permitted by all three documents |
| Recommend a video or browse its channel | Video indexing and discovery permitted by the object, uploader and channel |
| Share with peers | Unavailable in step 1; a later exchange contract must define permitted scopes |

Every retention or presentation decision also requires current moderation,
viewer and remote-access checks and unexpired authoritative evidence. Unlisted,
followers-only, direct, local-only and members-only material is excluded. Unknown
visibility or authority requires verification. Personal retrieval remains a
separate existing contract.

The policy has a master switch, independent missing-field defaults, and separate
switches for source inspection, peer exploration, directories, outboxes, metadata
caching, profile listing, public-post indexing, recommendations and future peer
sharing. Disabling an acquisition method does not erase otherwise eligible cached
results; disabling a presentation use suppresses that use on reevaluation.
Tightening a missing-field default also applies to already cached records.

Step 1 implements these as pure policy values, parsing and tests. Step 2 persists
the settings and maps them back to the policy. Administrator routes/UI and fetch
workers remain later increments. The rollout master switch starts disabled;
when enabled, absent discovery/indexing signals default to allow.

## What the current code changes about the proposal

| Finding | Implementation consequence |
| --- | --- |
| [Discovery queries](../crates/db/src/discovery.rs) and [suggestions](../crates/db/src/suggestion.rs) use stored accounts and relationships. | Add independent acquisition; ranking alone cannot find an unknown source. |
| [Actor parsing](../crates/ap/src/actor.rs) defaults absent `discoverable` and `indexable` fields to false. | Preserve field presence in new discovery policy evidence. Existing account booleans cannot distinguish unknown from explicit refusal. |
| [Status search](../crates/db/src/status.rs) includes viewer-visible public, unlisted and some restricted posts, without an `indexable` predicate. [The route](../crates/server/src/routes/search.rs) requires a viewer for status search. | Build a distinct public discovery query. Do not tighten the existing query globally and accidentally remove personal retrieval. This is a scope distinction, not an anonymous exposure finding. |
| [History persistence](../crates/db/src/remote_history.rs) already has leases, coalescing, cursors and backoff. [Ingest](../crates/server/src/ingest.rs) distinguishes history, explicit resolution and delivery. | Reuse patterns and narrow helpers, keeping discovery work separate from delivery and profile-history queues. |
| [The federation wrapper](../crates/server/src/federation.rs) supplies policy checks for AP redirects; [the client](../crates/federation/src/client.rs) supplies transport limits. | Verify the exact transport path for each adapter. An AP helper is not automatically a suitable anonymous JSON API helper. |
| [Remote actor refresh](../crates/server/src/remote.rs) also schedules media and link enrichment. | Background discovery must not call the whole refresh path indiscriminately. Parse verified metadata without triggering those jobs. |
| [Songhive browsing](../crates/server/src/music_remote/catalogue.rs) and [consented collections](../crates/server/src/collections.rs) already exist. | Defer Songhive/audio discovery until the audio system and interoperability are mature enough. Future collection discovery must retain collection consent rules. |

## Incremental implementation backlog

Implement these changes sequentially. Each should be reviewable independently;
split a change if it grows across unrelated concerns. Keep the feature inactive
until its first complete user journey passes.

### Change 1 Define and test eligibility

**Deliverable:** a small discovery policy module and a table of permitted uses.
**Completed on 15 October 2026.** The [signal parser](../crates/ap/src/discovery.rs)
preserves origin declarations; the [policy module](../crates/server/src/discovery/policy.rs)
evaluates current settings, access and evidence without I/O.

- Add `crates/server/src/discovery/mod.rs` and `policy.rs`, with decisions for
  fetch, metadata retention, profile listing, public text indexing,
  recommendation and future sharing. Return a reason and evidence freshness.
- Preserve `allow`, `deny`, `missing` and `invalid` observations from authoritative
  input. Add presence-aware parsing near the existing AP parser without changing
  the meaning of existing account booleans.
- Require verified identity, current local policy and fresh evidence before
  listing a profile. Respect its `discoverable` declaration when present and use
  the configured default when absent. A third-party claim cannot override refusal.
- Define public text indexing separately: public addressing, the author's
  `indexable` declaration or missing-field default, and applicable restrictions.
  Profile-listing eligibility does not itself grant post-indexing permission.
- Default both missing-field policies to allow. Provide independent controls to
  deny missing signals or disable individual discovery mechanisms. Malformed
  signals and unverified/expired evidence require verification rather than fallback.

**Done when:** table-driven tests cover missing/false/true/invalid flags, both
configurable defaults, independent mechanism switches, the two flags disagreeing,
expired evidence, blocked sources, public/unlisted/private/local visibility, and
a third party claiming consent. The policy code performs no
network or database writes. Existing search behavior remains covered by its
regression tests.

**Validation:** 65 selected Rust tests passed, including 21 new parser/policy
tests and the existing actor and search regressions. `./dev check` passed
formatting, workspace Clippy with warnings denied, offline workspace checks and
18 benchmark-budget unit tests. Static syntax checks and document links passed.
The [policy tests](../crates/server/src/discovery/policy/tests.rs) cover the agreed
defaults, independent switches, refusals, malformed input, evidence expiry,
visibility, third-party hints and reevaluation after settings change.

### Change 2 Add bounded storage and resumable work

**Depends on:** change 1.

**Deliverable:** discovery settings and migrations with sources, work
items and catalogue entries. Add DB modules alongside existing discovery code.

**Completed on 15 October 2026.** Migrations
[0185](../crates/db/migrations/0185_discovery_store.sql) and
[0186](../crates/db/migrations/0186_discovery_pending_withdrawals.sql) add the bounded
store and pending-withdrawal guards. No fetch worker or public/admin route is
activated by these changes.

- A source records its origin and endpoint, verified identity, observed
  capabilities, introduction provenance, cooldown and policy revision.
- A work item records target, operation, cursor, lease token, attempt count,
  next run and introducing source. Unique target/operation keys coalesce work.
  Unverified hints stay in this bounded queue, without displayable snippets.
- An entry records canonical URI, kind, optional existing account reference,
  bounded display fields, consent evidence, verification/expiry times and state.
  Withdrawn entries retain minimal identity/revision information and clear their
  display payload. A separate withdrawal table is unnecessary at this stage.
- Enforce atomic admission caps for queued hints, retained sources and entries,
  including per-origin and per-introducer caps. Bound field sizes and diagnostic
  retention as well as row counts.
- Reject stale commits using lease tokens and current source/entry revisions.
  Keep network requests outside transactions. Restart recovery preserves
  attempts, cursors, retry deadlines and admission usage. Persisted HTTP request
  and byte reservations arrive with the transport in change 3.

Implementation uses [discovery persistence](../crates/db/src/discovery_store/mod.rs)
and the [server policy mapping](../crates/server/src/discovery/store.rs). Short
mutation transactions lock the settings singleton before admission counts or
state changes. Settings edits use an expected revision; changes fence existing
claims. Claim tokens, source revisions and per-target entry revisions prevent
older work from committing. Source attempt totals survive completed jobs and
worker restarts.

Retained denied/withdrawn entries count toward catalogue caps. A verified
withdrawal before the first catalogue write keeps the already-admitted queue row
as a guard, with its lease and cursor removed; it counts toward queue caps and
survives ordinary pruning. Raising capacity never clears these guards. An
operator cannot lower limits below retained usage through the settings API.
Duplicate hints preserve the original provenance, retry deadline and attempt
count. URLs are normalized before origin quotas are applied.

The internal entry reader includes expired/withdrawn records for maintenance;
it is not a public listing API. Rendering must use current settings/source facts
and access restrictions through `evaluate_entry`. Pause stops acquisition but
allows otherwise eligible cached browsing; disablement hides results.

**Done when:** DB tests demonstrate duplicate coalescing, concurrent admission at
the cap, expired-lease recovery, pause/resume and rejection of stale completions.
An input stream larger than the cap cannot grow storage without bound. Generate
SQLx metadata with the repository tooling.

**Validation:** 82 selected Rust tests passed, including 17 new persistence and
policy-mapping regressions. The [database tests](../crates/db/src/discovery_store/tests.rs)
exercise simultaneous admission/claims/writes, all quota scopes, source and
settings changes, stale leases, retry/expiry cleanup, withdrawal before/after
the first write, payload clearing and field bounds. Existing policy, actor and
search regressions passed. SQLx metadata was regenerated and its effective
metadata check passed; `./dev check` passed formatting, Clippy with warnings
denied, offline workspace checks and 18 benchmark-budget unit tests. Static
syntax checks and document links passed. Runtime HTTP budgets and workload
calibration remain part of the worker integration.

### Change 3 Connect the worker to guarded transport

**Depends on:** change 2.

**Completed on 15 October 2026.**
The [request-budget migration](../crates/db/migrations/0187_discovery_requests.sql)
adds durable rolling accounting and source outcome timestamps.

**Deliverable:** one supervised discovery worker, with guarded AP and public JSON
fetch operations through the existing federation abstraction.

- Register with the existing worker lifecycle and cooperative shutdown.
- Check local instance policy before the initial request, every redirect and
  result commitment. Apply SSRF protection, deadlines, decoded-body ceilings,
  content-type checks and bounded parsing to the actual production path.
- Use the instance signer where appropriate for AP. Public catalogue requests
  use anonymous reads; never export member tokens or retry denials using a
  member's identity.
- Account for redirects, retries, actor verification and auxiliary requests.
  Observe `Retry-After`, use jitter and negative capability caching, and yield
  capacity to normal federation traffic.
- Expose pause, last success, failure reason, queue depth and budget consumption
  before starting automatic acquisition.

**Done when:** transport-level tests cover blocked redirects before contact,
oversized/chunked responses, malformed JSON, timeouts and cancellation. Pausing
stops new acquisition and fences results from superseded work. A crashed worker
resumes through its persisted leases.

The [worker](../crates/server/src/discovery/worker.rs) is registered with the
existing supervisor. It currently claims only `inspect_source`: an admitted JSON
endpoint can establish `source_inspection` for 24 hours. It discards the document
and does not infer directories, peers or outboxes from software labels or links.
Other operations remain queued without consuming attempts until their adapters
are installed. No seeds or recurring source expansion are enabled here.

The [transport boundary](../crates/server/src/discovery/transport.rs) passes
through `FederationApi` into the
[guarded HTTP client](../crates/federation/src/client/discovery.rs). AP requests
require the instance signer; anonymous JSON requests cannot receive that signer
or member credentials. Redirects stay on the admitted source origin, with SSRF
and mutable instance policy checked before contact. Each signature retry and
redirect needs its own reservation. JSON parsing retains the existing recursion
limit and 2 MiB decoded-body ceiling; a job has a 30-second wall deadline.

[Durable accounting](../crates/db/src/discovery_store/budget.rs) reserves one
request and 2 MiB before each send. Complete bodies settle to their actual size;
failed, oversized, interrupted or crashed reads retain the full charge until
24 hours elapse. Their network slot remains held for at most 30 seconds, even
if an operator pauses and immediately resumes. Thus the reported byte budget
includes conservative reservations, not only successfully received documents.
The receipt table is bounded by the configured request allowance, whose hard
maximum is 100,000. Old receipts are pruned on subsequent admission.

The pilot allows 500 requests globally and 50 per origin per rolling day, with
64 MiB of body allowance, two concurrent jobs and one request per origin.
Discovery also yields when the shared federation fetch pool has eight or fewer
slots available, and takes the hidden-service gate only if it is immediately
available. Capacity/budget deferrals preserve cursors and do not consume
remote-failure retries. Endpoint refusals and unsupported responses receive a
24-hour negative cache; transient failures use backoff with jitter. Rate limits
apply across the source origin, including subsequently added endpoints, and honor `Retry-After` up to the seven-day
backoff bound. Pause cancels active acquisition on the next policy check (at
most 250 ms between checks, subject to database availability), while revision
checks fence stale commits.

Operator controls are available before the browsing UI:

```sh
plamenu discovery status
plamenu discovery pause
plamenu discovery resume
plamenu discovery configure --enabled false
plamenu discovery configure --allow-missing-discoverable false --allow-missing-indexable false
plamenu discovery configure --disabled-mechanisms directory,outbox
plamenu discovery configure --requests-per-day 500 --requests-per-origin 50 --bytes-per-day 67108864
```

`configure` changes only supplied controls; an empty `--disabled-mechanisms ''`
clears mechanism exclusions. `status` reports settings, queue/guard counts,
retry reasons, global and origin charges, next charge expiry, source cooldowns,
and last success/failure diagnostics. Source and origin detail lists are capped
at 200; queue reason groups at 50. Source `attempts_total` counts job claims;
actual HTTP attempts are counted by the request ledger. A source retains its
last failure reason after recovery, alongside the newer success timestamp.
Status inspection never triggers acquisition.
The feature remains disabled by default; source admission and the directory
adapter are the next increment, with the operator browser view in change 5.

**Validation:** 199 selected Rust tests passed, including 21 new budget,
transport and worker regressions, plus existing federation, actor, search and
remote-history coverage. After the final hidden-service admission refinement,
all eight focused gate/worker tests passed. Owned loopback proxies exercise the
real transport without relaxing its HTTPS/SSRF rules. Checks cover signature
fallback accounting, anonymous headers, redirect-time blocks, oversized declared
and chunked bodies, invalid/deep JSON, the 30-second deadline, stream cancellation,
pause/resume fencing, crash recovery, origin backoff inheritance and absence of
account/post/follow imports. CLI help and read-only status were exercised against
the disabled development instance with no discovery requests started.

`./dev check` passed formatting, workspace/all-target Clippy with warnings denied,
offline compilation and 18 benchmark-budget tests. SQLx effective metadata passed
for 1,803 queries; static syntax, whitespace and all document links passed.
The new 187th migration crossed Clippy's 16 KiB threshold for SQLx's generated
constant migration array, so `clippy.toml` now documents a 32 KiB array threshold.
This changes lint configuration, not runtime allocation. Migrations were applied
only to the local development/test databases. Discovery remains disabled and no
live peer support or production deployment is claimed by this increment.

### Change 4 Verify one directory and maintain its entries

**Depends on:** change 3.

**Completed on 15 October 2026.**

**Deliverable:** one Mastodon-style directory adapter, exercised through an
operator-added source and the real worker.

- Probe a short explicit endpoint list; record successful capabilities rather
  than trusting software branding. Start with the public local directory.
- Treat directory responses as candidate addresses. Resolve each canonical
  actor, validate its identity and current profile-listing permission, then
  retain only bounded text metadata. Deduplicate by canonical identity.
- Handle disabled/auth-required endpoints as unsupported or denied capabilities
  with backoff. One denied endpoint does not prove the whole source is deleted.
- Revalidate profiles and their policy evidence. Suppress expired, denied and
  withdrawn entries; distinguish transient unavailability from authoritative
  deletion. Never accept a third-party deletion claim as authority.
- Apply current block/moderation changes at serving time and invalidate stored
  eligibility. Revocation must win against an older in-flight result. Re-entry
  requires fresh authoritative evidence, not another copy of an old hint.

**Done when:** one source yields permitted profiles, applies the configured
missing-signal default, and excludes refused or unverified ones. The same fixture
subsequently revokes consent, denies access or deletes
an actor, and the result disappears. Test the revocation race and expiry while
refresh is unavailable. Pin the first real-peer check to a named version and
configuration before describing adapter support.

The [directory adapter](../crates/server/src/discovery/directory.rs) probes the
single explicit `/api/v1/directory` endpoint. It requests local accounts in pages
of 20, following the
[Mastodon directory API](https://docs.joinmastodon.org/methods/directory/).
It ignores remote pagination links, rejects oversized/malformed page shapes,
deduplicates addresses and excludes foreign-origin hints. A successful empty
array establishes the capability without inventing profiles. Directory software
names and profile text confer no authority.

[Page admission](../crates/db/src/discovery_store/directory.rs) saves the observed
capability, admits bounded profile jobs and persists the next sampling position
in one transaction. It uses an advertised actor URI when present. Older directory
responses containing only a profile URL get at most one same-origin canonical-ID
handoff; that canonical address is fetched independently before saving a card.
Neither directory assertions nor a permalink response can substitute for that
verification. Existing quotas, lease revisions and withdrawal guards apply to
every admission and completion.

The [profile verifier](../crates/server/src/discovery/profiles.rs) accepts canonical
Person, Service and Application actors, preserves missing/invalid consent signals,
and retains only a bounded plain-text name and biography (1,024 and 8,192 bytes).
Names remain literal text; biography HTML is reduced to text. Explicit restricted
profile audiences, local-only/suspended/moved actors and invalid discovery flags
are ineligible. Follow-approval requirements alone do not make a public profile
private. A malformed display field cannot override an explicit refusal.
Profiles can be listed when `indexable` is false: profile listing follows the
independent `discoverable` policy. No account import, follow, post, notification,
media fetch, feature sync or outbox hydration is triggered.

Maintenance rechecks sources and retained non-withdrawn profiles near the
24-hour freshness boundary, using the existing queue and HTTP budgets. Each
scheduling pass considers at most 32 sources and 64 profiles. Cached results
expire when evidence cannot be refreshed; the serving helper also checks current
instance suspension/silence and existing account moderation. Viewer-specific
filters remain part of change 5. A read never starts a fetch, although a newly
observed local restriction clears the stored profile payload.

Actor endpoint failures are isolated from directory/source backoff, except for
origin-wide rate limits. A canonical 401/403/404 suppresses the profile without
claiming deletion. A canonical 410 or identity-matched Tombstone withdraws it.
Authenticated self-Updates can revoke discovery immediately; authenticated
self-Deletes withdraw already-admitted entries. Unauthenticated deletion hints
cannot do either. These paths invalidate older leases, including explicit
rechecks. Non-withdrawn refusals can recover through fresh canonical verification;
withdrawal guards require an operator-requested canonical recheck and cannot be
cleared by another directory hint. The
[operation migration](../crates/db/migrations/0188_discovery_directory.sql)
introduces this separately admitted recheck operation.

The CLI now provides the operator path for this increment:

```sh
plamenu discovery configure --enabled true
plamenu discovery add-source https://peer.example
plamenu discovery run-once
plamenu discovery profiles
plamenu discovery recheck https://peer.example/users/example
plamenu discovery status
```

`run-once` processes one bounded batch; the supervised server worker continues
queued work automatically. `profiles` reads at most 200 stored candidates and
prints only currently eligible text. `recheck` accepts only already-known
canonical identities, including retained withdrawals. The feature remains off
by default; these example commands were not used to enable the development or
production instance. The real-peer test uses an isolated disposable database.

**Real-peer check, 15 October 2026:** the existing owned instance at
`mastodon.local` reported Mastodon **4.7.3**, running image
`sha256:0bf26ebfb7d694bddd2fcf0c5769642ac29d997eb42169c540963cf0f27ee6ca`.
Its public local directory was available, `AUTHORIZED_FETCH` was unset and
`DISABLE_RACK_ATTACK=true`. Using Plamenu's existing instance signer, the actual
worker verified one eligible profile in **two metered requests**, with zero
ordinary accounts imported. The actor declared `discoverable=true` and
`indexable=false`. This verifies that version/configuration; other server
versions and authorized-fetch configurations are not claimed by this check.
The opt-in `owned_mastodon_directory_round_trip` test in
[the adapter regressions](../crates/server/tests/discovery_directory.rs) preserves
the same reproducible path using `PLAMENU_DISCOVERY_LIVE_CONFIG` and the owned peer.

**Validation:** 218 selected regression tests passed across the discovery store,
actor parsing, federation transport, inbox, search and remote-history paths. The
final adapter suite passed all 12 deterministic tests, including first-fetch
withdrawal guards, explicit canonical re-entry, domain silence, malformed refusal
metadata, bounded pagination and an in-flight revocation race. The opt-in live
Mastodon check passed separately.

`./dev check` passed formatting, workspace/all-target Clippy with warnings denied,
offline compilation and all 18 benchmark-budget tests. `./dev sqlx-check` passed
for 1,822 effective query records. Static syntax, document links and whitespace
checks passed. CLI help, status and profile listing were exercised against the
disabled development instance: zero discovery requests and no cached profiles.
Migration 188 was applied only to local development/test databases. This increment
does not expose a browsing page; that remains change 5.

### Change 5 Expose the first complete browsing journey

**Depends on:** change 4. **Completes milestone A.**

**Completed on 15 October 2026.** Milestone A now has a tested browsing journey;
source acquisition still starts with an operator-added server. Milestone B remains
the next acquisition milestone.

**Deliverable:** a server-rendered Discover page and a small operator source view.

- Show compact profile and server cards with clear destinations and actions.
  Keep verification diagnostics in the operator view; omit repeated consent and
  verification explanations from member cards. Plain text and local monograms
  avoid remote embedded media and unsanitized markup.
- Apply viewer blocks, mutes, domain exclusions and relevant dismissals before
  assembling a page. Match canonical URIs to existing accounts for those checks.
- Keep pagination stable and cap repeated origins. Show loading, paused,
  unsupported, exhausted-budget and no-results states.
- Opening a card uses explicit resolution; Follow stays a separate action.
  Mere display must not import statuses, create follows, enqueue enrichment,
  notify anyone or influence existing trend/popularity counts.
- Add localized labels and keyboard, mobile and no-JavaScript coverage.

**Done when:** a browser journey starts with zero follows and reaches a verified
remote profile. Side-effect assertions cover follows, statuses, notifications,
outgoing activities and media jobs before the explicit user action. Blocking a
result removes it immediately on the next request.

**Checkpoint:** assess whether the cards help a person choose something to
explore. Adjust the experience here before growing the acquisition workload.

The first member surface is [`/discover`](../crates/server/src/web/discovery.rs),
with separate People and Servers views, English/Russian labels, and
server-rendered navigation. It requires a browser session. Cards contain local
monograms, escaped names and bios, server links and explicit profile actions.
The 16 October interface revision below replaces the initial explanatory labels
and adds progressive pagination. No remote image or other media is embedded.

The [browse service](../crates/server/src/discovery/browse.rs) reevaluates source,
entry and policy freshness, instance moderation and personal exclusions. Existing
accounts are matched by canonical URI on each read, including accounts imported
after the card was verified. Blocks in either direction, active mutes, domain
blocks and dismissed follow suggestions apply. A member can also hide an
unimported card; [migration 189](../crates/db/migrations/0189_discovery_dismissals.sql)
stores that personal dismissal against the retained entry without creating an
ordinary account. Foreign keys bound those records by members and retained entries.

[Pagination](../crates/db/src/discovery_store/browse.rs) uses an admission watermark
and a cursor across stable per-origin positions. It returns at most 20 profiles,
with at most four from one origin, and scans at most 200 candidates per request.
Later arrivals appear on a new browsing journey; a dismissal or expired entry
cannot shift another retained entry across the cursor. Source browsing likewise
uses bounded scans and a continuation. Empty, disabled, paused, pending,
unavailable and budget-limited states are explicit. Pausing acquisition still
permits fresh cached cards; disabling presentation hides them.

Opening a profile is a CSRF-protected form action, works without JavaScript and
checks current eligibility again. Only this explicit action invokes ordinary URL
resolution and redirects to the selected local profile. Follow remains on that
profile as a separate action. Canonical links also provide an explicit way to
visit the original profile. Both browsing surfaces use private, non-cacheable
responses.

The [operator view](../crates/server/src/web/admin/discovery.rs) at
`/admin/discovery` requires the federation-management permission. It shows bounded
source diagnostics and budget usage, admits a source for background verification,
enables/disables sources, and edits the master switch, pause, missing-signal
fallbacks and individual mechanism switches. Policy forms reject stale revisions;
all mutations require CSRF protection. Controls for later acquisition/content
families are clearly labelled as reserved. Reading either view performs no remote
work. Standing development/production discovery settings remain unchanged.

**Validation:** 52 selected discovery, directory, web, search and remote-history
regressions passed. The final [web suite](../crates/server/tests/discovery_web.rs)
passed all nine tests, including the opt-in
[Chromium journey](../accessibility/discovery-journey.mjs). It exercises desktop,
390px and 320px layouts, keyboard navigation, paging, sources and operator views,
plus desktop/mobile interaction with JavaScript disabled. Automated axe checks
passed for the member and operator pages in the scripted browser contexts;
screenshots were visually reviewed. The browser starts with zero follows, proves
browsing creates no accounts, posts, follows, notifications or acquisition/media
jobs and loads no remote resources, then explicitly opens one profile and finds
the separate Follow action. No-JavaScript interaction tests run separately from
axe, whose scanner requires page timers.

All 18 navigation density regressions passed after allowing the desktop sidebar
to fit its extra destination; touch target sizing remains unchanged. Seven
localization/layout unit tests passed, including complete Russian message
coverage. `./dev check` passed formatting, workspace/all-target Clippy with
warnings denied, offline compilation and all 18 benchmark-budget tests.
`./dev sqlx-check` passed for 1,828 effective query records. Static syntax,
document links and whitespace checks passed. Migration 189 was applied only to
local development/test databases. A final read of the standing development
database confirmed discovery disabled, both missing-signal defaults allowed,
zero sources and zero discovery requests.

### Change 6 Bootstrap and expand beyond the seed list

**Depends on:** change 5. **Completes milestone B.**

**Completed on 15 October 2026.**

**Deliverable:** replaceable seeds, source import/export and bounded recurring
exploration via successfully observed public peer-list capabilities.

- Bundle a small reviewed source list with provenance and a review date. Actual
  addresses are a release input: select and verify them before enabling the
  feature by default. Also support an operator-supplied list.
- Export addresses and public source facts only. Imports receive ordinary
  untrusted-hint admission; neither path conveys trust or consent.
- Reserve a nonzero exploration allowance separately from refresh demand.
  Admit newcomers with a small sampling budget, cap introducer influence, and
  rotate the active working set rather than letting initial seeds fill it forever.
- Bound peer-list depth, pages, response size and retained hints. Coalesce cycles
  and duplicates. Discovery must continue without clicks or follows.
- Rank eligible profiles with a simple stable rotation across sources. Do not
  implement the proposal's illustrative percentage mixture or reward raw volume.

**Done when:** a generated network starts at A, finds B and C through published
edges, and admits a later independent source without member activity. Duplicate
storms and a prolific source cannot take all queue or display capacity. Restart
preserves progress. Blocking all project-controlled sources still permits
discovery from an independent seed/import. All failed seeds produce a truthful
empty state and an import option.

**Implementation (15 October 2026):** migration 190 adds durable peer depth,
list-window positions, provenance, exploration accounting and stable catalogue
positions. Acquisition probes the public Mastodon-compatible
[`GET /api/v1/instance/peers`](https://docs.joinmastodon.org/methods/instance/#peers)
endpoint anonymously through the existing guarded transport. An observed list
proves only that endpoint; every new origin and profile is independently checked.

Pilot bounds are two hops from an operator root, eight admitted peer hints per
round, 256 inspected list rows, at most 262,144 rows inside the existing 2 MiB body
limit, and one recurring round per source per day. The window and next-round time
survive restart. Unsupported peer endpoints have their own daily backoff and do
not invalidate a working directory. Origin rate limits still cover both adapters.
Each directory round selects at most three candidates from one bounded page.
The persistent walk described below continues across rounds and restarts.

One in five scheduling turns prefers exploration, with idle capacity available to
either lane. Pending admissible exploration reserves 20% of request allowance and
at least 20% of decoded-body allowance (one full body at minimum). Peer admission
leaves 20% of source and queue capacity available for independent roots. Existing
origin and introducer limits still apply. Full working sets can rotate automatic
leaf sources after 24 hours and active cards after 12 hours; explicit operator
sources, disabled sources, backoff, refusal/withdrawal guards and personal hides
are preserved. Guard saturation therefore reports capacity instead of erasing
negative evidence. Profile ordering rotates source order daily; each continuation
retains the rotation and watermark, with persistent positions across eviction.

The bundled manifest contains three profile, three community and three video
origins, with explicit catalogue endpoints, provenance and a review date. The
public endpoints passed bounded availability probes on 16 October 2026. These
checks establish endpoint availability, not catalogue quality or complete
interoperability. These are optional operator-selected starters,
not an automatic network dependency; recheck addresses before release. The
feature remains disabled by default. The admin page offers **Use bundled
starters**, bounded JSON import, replacement of bundled sources, and export.
The CLI equivalents are `discovery sources bootstrap`, `discovery sources import
PATH [--replace-bundled]`, and `discovery sources export`.

A version-1 manifest has `sources: [{"origin":"https://example.org"}]`, with an
optional same-origin `endpoint` selecting a supported profile, community or video
catalogue. It has a 64 KiB / 100-source bound. Parsing validates the whole list before admission.
Extra facts are ignored on import. Export includes the first 100 addresses and
their selected endpoints,
observed capabilities and public verification times; it excludes local policy,
consent, relationships, queue internals and errors. Explicitly importing a peer
can make it an independent root, subject to root quotas, without clearing its
backoff or changing verified facts. A disabled source stays disabled.

**Validation:** the full selected regression run exercised 98 cases. Its only
failure was the new graph harness treating a locally deferred job as finished;
the harness now waits for durable backoff, and its focused rerun passed through
both graph expansion and the independent import (62 seconds). The other 97 cases
passed, including catalogue rotation, source adoption, quota reservation,
withdrawal fencing, directory refusals, personal exclusions, import/export
permissions and the Chromium browser journey. Seven localization/layout tests
also passed, including complete Russian message coverage. The browser check
covers desktop, mobile, 320 px, keyboard use, no JavaScript, accessibility, sources,
operator controls and explicit profile opening without an automatic follow.

An isolated local review instance populated twelve fictional profiles across
three sources through the real worker. Chrome has Discover, Sources, the operator
view and an opened profile available for review. Its database and loopback peer
fixture are separate from the standing development instance.

`./dev check` passed formatting, workspace/all-target Clippy with warnings denied,
offline compilation and all 18 benchmark-budget tests. `./dev sqlx-check` verified
1,850 effective query records. Static syntax, whitespace and all 41 local plan
links passed. Migration 190 was applied to local development/test databases. A
final read confirmed the standing development instance still has discovery
disabled, both missing-signal defaults allowed, zero sources and zero discovery
requests. The review instance has three sources and twelve verified sample cards.


## Discovery interface revision — 16 October 2026

The member journey now uses compact profile cards (local monograms, verified
names, server links, plain-text bios and explicit profile/hide actions) and
server cards with browse and visit actions. Consent explanations, repeated
verification labels and the introductory instruction paragraph are removed.
People and Servers remain visible navigation links at narrow widths. Remote
images are not requested while browsing; this presentation uses the existing
bounded catalogue without changing acquisition or importing accounts.

JavaScript progressively appends cursor pages near the end of the list. A
session-scoped automatic-loading switch, polite status announcements and a
manual retry keep the controls usable. Automatic loading preserves focus;
explicit loading focuses the first new heading. Duplicate/repeated cursors,
redirects and failed requests stop automatic retries. Three successive empty
pages also stop automatic loading. Ordinary pagination links work without
scripts, and every page still uses the existing eligibility checks.

The operator page groups policy controls, readable usage/allowance metrics and
source management. Mechanisms, future settings, source transfers and per-source
diagnostics use native expandable sections. Every existing policy value remains
in the form even when its section is closed.

Validation: all 17 selected web, localization and layout tests passed. The
browser journey covers desktop, mobile, 320px, dark mode, disabled JavaScript,
keyboard operation, appended results, focus preservation and failed-load retry.
A final browser run also passed accessibility checks on server cards and expanded
operator controls. Browsing still creates no remote requests, imported accounts,
posts or follows. `./dev check` passed Clippy, offline compilation, formatting
and all 18 benchmark-budget tests; static syntax and whitespace checks passed.
The isolated Chrome review instance was rebuilt and visually checked, including
server-filtered automatic pagination. People, Servers, administration and the
previously opened profile remain available in Chrome. No schema/query changes
were needed for this revision.
This interface revision preceded **change 7: public posts and local discovery
search**, recorded below.


### Change 7 Public posts and local discovery search

**Completed on 16 October 2026.**

The new signed-in `/discover/posts` view browses and searches independently
verified public excerpts. Search reads only the local discovery catalogue; it
does not resolve the query or widen into remote search. Existing People,
Explore, personal status search and client search contracts remain unchanged.
Post browsing requires both discovery and indexing permission. An explicit text
query requires indexing permission independently of profile discoverability.

The [outbox adapter](../crates/server/src/discovery/outbox.rs) uses an outbox
advertised by an independently verified canonical actor. Each daily author round
fetches at most the collection and one first page, inspects at most 32 items,
and admits one canonical object hint per author round, rotating the observed
position on later rounds. It ignores Announce
activities, foreign origins, mismatched authors and remote continuation links.
Even embedded objects supply only hints; the [post verifier](../crates/server/src/discovery/posts.rs)
fetches each canonical object separately. This follows the ActivityPub
[outbox](https://www.w3.org/TR/activitypub/#outbox) and
[object retrieval](https://www.w3.org/TR/activitypub/#retrieving-objects)
model with stricter local public-discovery limits.

Supported text objects are Note, Article, Page and Question. A canonical id,
single matching author, valid publication time and public `to` are required.
Public `cc` alone, narrower audiences, hidden recipients, local-only objects,
explicit refusals and invalid declarations are excluded. No media, linked pages,
quoted posts or ancestors are fetched during acquisition. Excerpts are plain
text, bounded to 8 KiB, with 1 KiB titles/content warnings.

[Migration 191](../crates/db/migrations/0191_discovery_posts.sql) adds bounded
outbox routing evidence, sampling deadlines, publication time, sensitive-content
flags and a PostgreSQL text-search index to the existing catalogue. Posts share
all existing source, queue, request, byte and retention budgets. At most half of
the shared catalogue and per-origin entry allowance may be post records, with
four active excerpts per author. Retained refusal/withdrawal guards count toward
the shared caps. Old positive excerpts may rotate after 12 hours; guards and
personal dismissals are preserved. Post work does not consume the reserved
exploration lane. At most eight author rounds enter the queue per scheduler pass.

Fresh actor evidence bounds each post's expiry and fences its commit. Authenticated
author indexing refusals erase dependent excerpts and cancel pending checks.
Post updates clear old text until canonical revalidation; deletes retain minimal
withdrawal guards, including during a first fetch. Relaxing a policy does not
restore erased text. Personal domain blocks, imported-account blocks/mutes,
dismissals and public-context content filters apply on every result page.
Sensitive posts and warning/blur filters use native disclosures that also work
without JavaScript.

Opening a result is an explicit CSRF-protected action through the existing URL
resolver and `ExplicitResolution` ingest path. Canonical URI storage and the
existing delivery-effect claim reconcile later federation delivery with that
row. Following and notification effects are not created by browsing or indexing.
The existing delivery path owns legitimate notifications when delivery arrives.

**Validation:** all 192 selected Rust tests passed, including discovery policy,
store, transport, directories, expansion, post acquisition/lifecycle, search,
inbox, remote-history and localization regressions. The browser journey passed
in six contexts: desktop, mobile, 320 px narrow, dark, and desktop/mobile with
JavaScript disabled. It covers local search, query-preserving pagination,
content-warning disclosures, overflow, and automated accessibility checks.
`./dev check` passed workspace Clippy with warnings denied, formatting, offline
checks and all 18 benchmark-budget tests. Static syntax checks passed.

The isolated local Chrome review has three sources, 12 profile cards and 16
verified post excerpts from four authors. Its real worker used 24 additional
HTTP requests for this increment (41 total, 29,776 decoded bytes across both
journeys), finished with no queued work, and created no ordinary statuses or
follows. The `garden` text predicate returned nine matches in 0.218 ms on this
small fixture; this is a correctness/smoke measurement, not a production capacity
claim. Posts, a garden search, People, Servers and Administration remain open in
Chrome. The standing development configuration remains disabled with no sources.

`./dev sqlx-check` passed fresh-database effective metadata verification for all
1,868 queries, including migration 191 and the new catalogue queries.


### Change 8 Public community discovery

**Completed on 16 October 2026.** Communities were selected
as the next content family because Plamenu already has canonical group pages and
explicit join controls. Songhive/audio remains deferred; videos and events remain
separate future adapters.

A signed-in `/discover/communities` view renders community cards, sensitive-content
disclosures, personal dismissals and progressive pagination. Server cards lead to
the appropriate people or community view. Opening a community uses the existing
Group resolver and `/!name@server` page; browsing does not import accounts or join
groups. Acquisition does not fetch community posts, outboxes, images or media.

The first adapter supports Lemmy's v3 public community API. An operator can supply
`/api/v3/community/list` directly. An origin initially tries the existing public
profile directory and, only after HTTP 404/405/501, one community-directory probe.
Authentication failures, throttling and malformed directory responses do not
trigger fallback. Subsequent rounds use the verified community capability.
Sampling reads one bounded page of 20 local records and selects at most three
rotated candidates. Its persistent page position continues on later rounds. All
work shares the existing
source, request, byte, queue, retention and exploration limits.

A listing supplies only a canonical actor hint. Before retaining metadata, the
worker separately retrieves the canonical Group and the origin's individual
community record. Both identities must agree. The API record must explicitly be
local, public, unhidden, undeleted and unremoved; the canonical actor must pass
public-access and discoverability checks. Missing discoverability follows the
agreed local default; indexability does not control a community directory card.
The earliest source/actor expiry bounds presentation, with a maximum 24-hour
verification lifetime. Origin changes without notifications take effect on
refresh or expiry. Authenticated Group updates clear old metadata pending fresh
verification; deletion and access refusal use the existing revision/withdrawal
guards. Tightening the missing-discoverability default erases affected community
text until it is independently reverified.

The `community_directory` acquisition switch is independent of profile-directory
and outbox acquisition. Metadata caching, profile listing, source enablement and
viewer/moderation exclusions retain their existing contracts. Sensitive descriptions
use native disclosures; the community name remains visible.

Protocol evidence: the [Lemmy API documentation](https://join-lemmy.org/docs/contributors/04-api.html),
[federation documentation](https://join-lemmy.org/docs/en/contributors/05-federation.html),
and a checked-in canonical Group fixture from source version 0.19.19, commit
`6c9bc1ce80d12cf1cdb1924e91d5740e91ff7601`. The adapter does not claim v4 support.

**Validation:** all 239 selected Rust tests passed, including 12 community
adapter tests and the existing discovery, group-consumption, inbox and search
regressions. The expanded browser journey passed in six configurations: desktop,
mobile, 320 px narrow, dark, and desktop/mobile without JavaScript. It checks
community cards, pagination, overflow and automated accessibility. Workspace
Clippy, formatting, offline compilation, static syntax and all 18 benchmark-budget
tests passed.

The local review acquired six community cards from one fictional Lemmy-compatible
server using 15 requests and 11,498 decoded bytes, including capability probing.
Acquisition created no ordinary community accounts, statuses or follows. Opening
one card in Chrome then resolved its normal Group page with an explicit Follow
button. The Communities and Group pages remain open for review. Migration 192
extends the existing bounded catalogue and controls without adding tables.
The CLI reference was regenerated and documentation checks passed, including the
book link check after correcting a pre-existing reference to a repository-only
audit. Fresh-database SQLx verification passed for all 1,870 queries.

### Change 9 Topics, saved views and public connections

**Completed on 16 October 2026.**

`/discover/topics` offers explicit hashtags from the local discovery catalogue
and public collection topics chosen by their curators.
Selecting a topic shows mixed people, post and community cards, with the same
source diversity, keyset pagination and optional automatic loading as the other
views. Tags come only from `Hashtag` objects on verified canonical documents;
biographies, post text, directory hints and collection membership do not imply
extra interests. At most 64 tag records are examined and 16 distinct normalized
tags are retained per entry. Tags share the entry's authority, freshness and
revision fence. A new verification replaces them atomically; refusals, withdrawals
and authenticated updates clear old claims. Existing records acquire tags at
normal refresh or an operator-requested canonical recheck.

Members can save up to 20 topics as private views, remove one or clear all saved
topics. Saving is not a follow and does not alter ranking or acquisition. There
is no inferred-interest model or stored navigation history. Clearing saved views
preserves separately chosen blocks, mutes and hidden results. Account deletion
cascades to saved views; catalogue removal cascades to tag edges. Migration 193
adds just these two bounded tables, reusing the existing author relationship and
collection membership records for other connections.

“Explore connections” opens a local trail from a result to its explicitly tagged
topics, its canonical author or that author's sampled public posts, and eligible
public collections featuring it. Every hop reevaluates current policy, source and
entry expiry, viewer blocks/mutes/dismissals and public-post content filters.
Topic results and trails require recommendations to be enabled; searchable
index-only authors are not promoted. Sensitive cards keep their disclosures, and
the topic hub does not extract labels from sensitive results. Links and forms
work without JavaScript. Browsing never fetches, imports or follows remote actors.

Collections initially come from **locally managed, public collections** with a
currently discoverable, visible owner. The topic is the curator's explicit tag;
member cards require accepted membership, a recorded authorization for remote
members, and fresh independently eligible discovery evidence. Pending, rejected,
revoked or inaccessible members are omitted, including for the collection owner.
The discovery collection page renders only those eligible cards. Collection
attribution remains visible; a collection never grants discovery permission to a
member. Up to 100 collection candidates are examined for at most 20 cards, and
existing 25-member collection limits apply.

Remote collection acquisition is a separate future adapter: current imported
collection data does not establish discovery freshness or revalidation of remote
approval stamps. Likewise, inferred similarity, broad topic crawling, language
and media preference panels, and a persisted trail history are outside this
increment. Videos, events and audio retain the scope decisions below.

**Validation:** the 255-test regression selection passed. The final focused
collection/web rerun passed all cases after correcting an expiry fixture to
move both verification and expiry into the past; its original one-second offset
could violate the database's evidence interval constraint on a fast run. The six
Chromium configurations passed again, including mixed-topic pagination, private
save/reset forms, connections, keyboard access, narrow layouts and automated
accessibility. Workspace formatting, Clippy, offline compilation, 18 benchmark
budget tests, static syntax, documentation/CLI comparison and book links passed.
Fresh-database SQLx verification matched all 1,881 effective queries.

The isolated Chrome review now has 60 explicit topic edges across 12 profiles,
16 posts and six communities. Canonical rechecks added 40 requests and 33,259
bytes to the existing fixture run; the resulting total is 96 requests and 74,533
bytes with no remaining jobs. Browsing the topic and connection pages added no
discovery requests. The review account saved one topic and still follows nobody.
A fictional approved local collection demonstrates curator attribution and topic
navigation; this fixture is not a live remote approval handshake. Topics, the
mixed gardening view, an author trail and the collection remain open in Chrome.
The standing development instance remains disabled with no discovery sources.

## Change 10 — wider search on explicit request

**Completed on 16 October 2026.**

People and Communities now have local catalogue search. After entering a query,
a member can expand “Search wider”, review up to three named servers, select
which ones receive the query and submit a CSRF-protected POST. Merely searching,
opening the disclosure or paging local results sends no remote request. Search
forms and result cards work without JavaScript; JavaScript adds a busy state.
The normal client/account search and local public-post search retain their
existing behavior.

The people adapter uses anonymous Mastodon-style `GET /api/v2/search` with
`type=accounts`, `resolve=false` and `limit=3`. The community adapter uses Lemmy
v3 `GET /api/v3/search` with `type_=Communities`, `listing_type=Local`, `sort=TopAll`,
`page=1` and `limit=3`. Only currently verified, eligible source origins with the
corresponding directory capability can be selected. Unsupported and authenticated
endpoints are cached for a day; malformed responses for an hour, transient
failures for a minute and rate limits according to bounded `Retry-After`.
These search capability failures do not disable a working directory adapter.
Rate limits encountered during canonical actor or alias verification use the
shared origin cooldown, stopping remaining candidates and ordinary acquisition
while preserving already eligible local cards.

Each submission has one 15-second deadline, at most three candidate hints per
server and 24 wire attempts including canonical verification, redirects and
signature negotiation. All ordinary request, body, concurrency, catalogue,
moderation and viewer restrictions apply. Every result must belong to its
selected origin and pass independent canonical profile or community verification.
Older account responses with only a human URL can resolve identity on that same
origin before a separate canonical fetch. Search snippets and off-origin hints
are discarded; pagination links are not followed. Canonical explicit refusals,
malformed consent signals, withdrawals and later local policy changes still win.
Nothing imports an account or creates a follow until the existing Open action.

Migration 194 adds one private receipt per account and a small source capability
failure cache. The receipt contains a query digest, source IDs and result IDs,
not plaintext query history or remote snippets. It expires after five minutes;
identical completed requests reuse it, and different requests have a 30-second
per-member throttle. Rendering reevaluates all current visibility rules. Verified
cards join the local results even if the remote server matched a handle or alias
that is absent from the canonical text. Local cards and completed partial results
remain available when a source fails or reaches the deadline.

Interactive leases are excluded from background claiming and expire at the
original deadline. Completion cancels remaining work; abandoned receipts and
leases are pruned by ordinary maintenance. First-fetch withdrawal guards detach
from the receipt before cleanup so a later search cannot erase a deletion guard.
Operators have a separate “Search wider on request” switch alongside the existing acquisition
and listing controls.

**Protocol evidence and limits:** the official [Mastodon search documentation](https://docs.joinmastodon.org/methods/search/)
permits anonymous account search but requires authentication for post full-text
search, so this increment adds no remote post-text query. Local Mastodon source
`7d9b24bba24eb46a23a207882718feb61138fada` (4.7.0-beta.1) also exposes an optional
asynchronous FASP retrieval path. The public search request supplies the
`Mastodon-Async-Refresh-Id` header that suppresses this known path, in addition to
`resolve=false`. Plamenu never forwards beyond the selected origins; remote
operators' independent handling of a received query cannot be guaranteed.
Lemmy source `6c9bc1ce80d12cf1cdb1924e91d5740e91ff7601` (0.19.19) establishes the
public v3 community search contract and the explicit local listing filter.
No credentials are borrowed from members; ActivityPub verification continues
to use only the instance signer. Video, event and audio adapters remain deferred.

**Validation:** the 48-case focused run passed, covering wider search and the
directory, community and web regressions. The final 25-case rerun passed after
adding canonical/alias rate-limit coverage; all eleven wider-search tests pass.
The other cases in the 284-test regression selection passed. These runs exercise
anonymous public HTTP, canonical verification, same-origin aliases, consent
refusals, partial results at the 15-second deadline, request caps, throttling,
private cached receipts, current filters, canonical rate limits, pause/block
cancellation and durable withdrawal guards. They verify that browsing and wider search create no ordinary
accounts, follows, posts, media jobs or background search continuation.

All six Chromium configurations passed, including the expanded source selector,
local-query filtering, narrow layouts, dark mode, keyboard navigation, no-JavaScript
fallback and automated accessibility checks. Formatting, Clippy, offline
compilation, 18 benchmark budget tests, syntax checks, CLI/documentation checks
and book links passed. Fresh-database SQLx verification matched 1,897 effective
queries.

The isolated Chrome walkthrough found two people and one community omitted from
its ordinary directory pages. The two wider searches spent 13 requests and
10,365 bytes, bringing the fixture to 37 entries, 109 requests and 84,898 bytes.
No jobs remain. Ordinary account, follow and status counts stayed at 3, 0 and 0;
the three new results remain unimported catalogue entries. People, Communities
and the operator controls remain open for review. These are deterministic local
fixtures, backed by the source contracts above, not a live remote-peer rollout.
The standing development instance remains disabled with no discovery sources.

## Video increment, before cooperative exchange

**Scope decision, 16 October 2026:** implement PeerTube public on-demand videos
and channel browsing next. Songhive/audio and events remain deferred. This extends
the completed steps 1–10; it does not change the personal video library or add
remote video full-text search.

The public `/api/v1/videos` adapter samples one page of at most eight local,
non-live records, ordered by publication time. The operator can seed this
endpoint directly. An instance-origin seed tries it only after unsupported
Mastodon and Lemmy directory endpoints; authentication failures, throttling and
malformed responses never cause speculative fallback. Capability is established
from a valid response, not a NodeInfo software label.

Each hint supplies identities only. Before retaining text, the worker checks the
canonical ActivityPub Video, an anonymous individual REST video record, the
uploader actor and the channel actor. All belong to the admitted origin. Both
video documents must agree on the uploader and channel, and the channel must
attribute itself to that uploader. The REST detail must be local, public,
published and not live. ActivityPub requires public `to`, matching attribution,
no private recipients or narrower audience other than the verified channel.
Classic PeerTube Person/Group attribution and owner-only attribution with channel
audience are supported. Explicit refusals or malformed signals on any of the
three canonical documents defeat permissive missing-field defaults. Index-only
results can appear in local query results but do not promote the channel.

`/discover/videos` has compact text cards, channel attribution, durations,
collapsed sensitive descriptions, local search, source and channel filters,
Hide and explicit Open actions. It shares the existing accessible pagination,
automatic loading and no-JavaScript fallback. Verified tags join topic and
connection views. Open video resolves into the existing native video page;
Open channel resolves into the channel's existing video library. Browsing does
not create ordinary accounts, statuses, follows or media jobs, and it does not
load thumbnails or playback files.

Separate operator switches control catalogue acquisition and video indexing.
The two video tables are children of the already-bounded job and entry stores.
Pending owner/channel bindings are recorded before verification I/O; confirmed
metadata commits with the ordinary lease/revision fence. Authenticated actor
updates cancel dependent work and clear cached text, while actor or owner video
deletions preserve withdrawal guards. Expiry, blocks, mutes, local filters,
dismissals, pause and source controls apply to presentation and acquisition.

Supported-version evidence comes from PeerTube 8.3.0 source commit
`81bf3179bbdddbd6bfb431aeac4320e8f7c87c03`: the public video controller,
`Video`/`VideoDetails` interfaces, API and ActivityPub video formatters,
`VideoChannel` ownership serialization, and publication/privacy enums. The
[official REST guide](https://docs.joinpeertube.org/api/rest-getting-started)
documents public video listing. Contract fixtures exercise these shapes over
real guarded HTTP; they are not a claim of live testing against every PeerTube
version or plugin.

**Validation:** all 331 selected regressions passed, including the thirteen
video-discovery tests and the existing discovery, native-video, search, inbox,
group, collection and history boundaries. The tests cover canonical authority,
private/unlisted/live exclusions, object/owner/channel signals, strict defaults,
anonymous REST and signed AP fetches, shared limits, deletion guards, actor-update
races, channel caps, local search, dismissal and explicit native opening.

The six Chromium configurations passed with video and channel views, sensitive
content, local-query pagination, no-JavaScript fallbacks, keyboard access and
WCAG checks. A final CSS-only refinement gives all six tabs a balanced two-row
layout; the complete browser journey passed again with an explicit layout
assertion. Formatting, Clippy, offline compilation, eighteen budget checks,
JavaScript syntax, generated CLI help and documentation/book checks passed.
Fresh-database SQLx verification matched 1,912 effective queries.

The isolated Chrome fixture discovered eight videos in two channels using 36
requests and 32,225 bytes, including unsupported directory/peer probes. It now
contains 45 catalogue entries and five fresh sources, with no pending work.
Ordinary accounts, follows and statuses stayed at 3, 0 and 0: the eight videos
remain catalogue metadata. Video browsing, the Small gardens channel and the
operator's video switches remain open for review. These are local contract
fixtures, not a live remote-peer rollout. The standing development instance
remains disabled and has no discovery sources.

## Initial operating limits

Use one configurable pilot profile before offering multiple resource profiles.
These are proposed test settings, not measured production capacity:

| Limit | Pilot value |
| --- | --- |
| Discovery requests | 500 per rolling 24 hours; maximum 50 per origin |
| Received decoded bodies | 64 MiB per rolling 24 hours; maximum 2 MiB per response |
| Concurrency | 2 globally, 1 per origin, subject to shared federation admission |
| Pending hints | 1,000 globally, 100 per introducer |
| Retained sources and catalogue entries | 100 sources, 200 shared profile/community/post/video/guard entries |
| Source admission | 4 endpoints per origin, 20 per introducer |
| Queue admission per origin | 100 retained work/guard rows |
| Catalogue admission | 40 entries per origin, 100 per introducer; denied/withdrawn entries count toward the 200-entry total |
| Directory sampling | One page of 20 hints, at most 3 canonical candidates per round; persistent bounded walk, at least 15 minutes between rounds |
| Outbox sampling | Once per author per 24 hours; at most a collection and one first page, 32 inspected items and 1 canonical post check |
| Post retention | At most 100 post/guard records globally, 20 per origin and 4 active excerpts per author, within all shared limits |
| Video sampling | One page of 8 hints, at most 2 candidates with different owners; 4 verification documents per candidate, all charged to shared budgets |
| Video retention | At most 50 video entries, including entry withdrawal guards (or half the shared cap), 8 per origin and 4 active videos per channel |
| Job deadline | 30 seconds |
| Explicit wider search | 3 origins, 3 hints per origin, 24 shared wire attempts, 15 seconds total |
| Wider-search receipts | 1 per member, expires after 5 minutes; 30 seconds between different searches |
| Durable job lease and retries | 120-second lease, 5 attempts; ordinary pending work expires after 7 days |
| Profile eligibility freshness | At most 24 hours since authoritative verification |
| Exploration reservation | 20% of the request allowance when admissible work exists |

Stop at whichever budget is reached first. Reserve request/body allowance before
starting concurrent work, then reconcile actual consumption. Include verification
and retries in the same accounting. Expired profiles disappear until refreshed;
if maintenance cannot keep up, shrink the active set rather than silently extend
freshness. Local blocks and verified revocations apply immediately when learned;
unannounced remote changes are bounded by refresh/expiry, not detected instantly.

Pause stops acquisition; expiry and local invalidation continue. Disabling the
feature also hides its surfaces. A discovery reset cancels work and clears its
cached metadata without removing ordinary federated accounts, follows or posts.
Retain minimal withdrawal guards until old leases/hints cannot reintroduce data;
after a backup restore, require fresh verification before serving restored cards.

## Later increments

**Scope decision, 16 October 2026:** defer Songhive and audio discovery because
the associated audio system and interoperability are still too early to justify
building discovery on them. Revisit when those foundations are mature enough.
Communities were selected for step 8, followed by the PeerTube video increment
above. Events remain a future adapter; select and validate one content family at
a time. Each needs supported-version tests, authority rules and lifecycle coverage. Public catalogue availability does
not automatically authorize universal text indexing.

| Order | Deliverable | Completion criterion |
| --- | --- | --- |
| 11 | Optional cooperative exchange | Legacy-source discovery still works alone. Versioned URI/delta hints are verified at origin; withdrawals and replay limits work. Assess FASP drafts at implementation time without promising an existing interoperable search mesh. |

Semantic ranking, embeddings and a network-wide DHT have no dependency on the
first release and remain deferred. New files and tables follow concrete features;
the original seven-table model and full adapter interface are not prerequisites.

## Validation and rollout

Use the existing [server integration fixtures](../crates/server/tests/common/mod.rs)
for deterministic state/race tests, and real guarded HTTP tests for transport.
Extend [search tests](../crates/server/tests/search.rs) and
[history tests](../crates/server/tests/remote_history.rs) when their boundaries are
touched. The first browser journey arrives with change 5; a generated multi-origin
graph arrives with change 6. Live peers supplement these tests and do not replace
reproducible fixtures.

For each implementation change, follow [development checks](DEVELOPMENT.md): run
affected tests without failing fast, regenerate/check SQLx metadata for schema or
query changes, and run required static checks. Measure queries and crawl cost
when those paths change. Record actual adapter versions, source diversity,
requests/bytes, queue peaks, freshness and revocation behavior at milestone B.
Coverage percentages apply only to the known test graph.

Roll out disabled, then enable the pilot on a controlled instance, inspect its
cost and usefulness, and expand only after the milestone B criteria pass. Keep
pause/disable available throughout. Publishing a release and live deployment are
separate follow-up work.

### First staging pilot — 16 October 2026

Deployed the working discovery build to staging after preserving the previous
binary, configuration and a checked PostgreSQL dump. Migrations advanced from
184 to 195; the installed binary matched the locally built SHA-256. Trending
was temporarily removed from signed-in and anonymous navigation, including the
mobile drawer. Its routes and API behavior remain available.

Enabled discovery with the two bundled starters and reduced pilot allowances:
100 wire attempts per rolling 24 hours, 25 per origin, 16 MiB of charged bodies,
two concurrent requests globally and one per origin. Missing-signal defaults
remain allow. No limits were raised to make the live test succeed.

Observed from 11:22:07 to 11:27:24 UTC, with 22 samples over 316.5 seconds
(312.2 seconds after the first wire reservation). All readiness samples passed;
there were no service restarts, discovery persistence/acquisition warnings or
limit breaches. Results:

| Measurement | Observed |
| --- | --- |
| Requests | 50 total; exactly 25 to each starter, then no further traffic during the observation |
| Charged bodies | 4,186,265 bytes, including the retained 2 MiB reservation for an oversized response |
| Concurrency | Peak 2 globally, 1 per origin |
| Busiest origin | At most 2 attempts in any rolling second, 17 in any rolling minute |
| Catalogue | 15 fresh profiles and 5 fresh public posts |
| Queue | Peak 16 rows; 3 jobs left deferred for exhausted origin budgets |
| Whole-service resources | Peak sampled memory 90.1 MiB, final 37.9 MiB; average CPU 4.6% of one core, including other staging workers |

The safety limits worked, but this run did **not** demonstrate peer expansion:
Mastodon.social's peer response exceeded the 2 MiB body cap, and mstdn.social's
peer response failed bounded document validation. Both peer endpoints backed off
for a day while their profile directories remained usable. Follow up with a
bounded way to handle large public peer lists and/or reviewed smaller starter
servers; keep the transport and request caps. Community and video acquisition
were not exercised against live origins in this two-source pilot.

The observer saved aggregate JSONL samples and final CLI diagnostics on staging,
with a local copy under `target/discovery-staging/2026-10-16/`. It stopped after
the observation; discovery remains enabled under the reduced caps. The admin
page and CLI retain their rolling 24-hour counters. Longer historical monitoring
is still a separate rollout task. The five-minute startup check does not replace
the longer soak, restart and revocation checks required for release qualification.

The deployed navigation was checked in Chrome and in signed-out HTML. All 12
navigation tests passed with and without JavaScript; `./dev check` passed
formatting, Clippy with warnings denied, offline compilation and its 18 budget
gate tests. People, server and post catalogues were left open in Chrome.

### Sampling diversity increment — 16 October 2026

The first pilot exposed a selection problem: two large profile sources, newest
accounts first, and several posts from the same author used the available origin
allowance without exercising communities or videos. The next increment changes
candidate acquisition and presentation while retaining all transport, policy,
freshness, concurrency and rolling budget limits.

- Bundled starters cover three origins per family: mastodon.social, mstdn.social,
  social.coop; lemmy.world, lemmy.ml, discuss.tchncs.de; framatube.org,
  tilvids.com, video.blender.org. Typed endpoints avoid unrelated fallback probes.
- Each source keeps its own sampling seed, round, independent active/new offsets
  and next eligible sampling time. Profiles use active order for four rounds and
  newest order for one. Communities use `TopMonth` (monthly active participants)
  for four rounds and `New` for one; Lemmy's `Active` maps to hot rank and retains
  a strong freshness bias. Full pages advance the corresponding
  offset within a bounded 400-candidate walk; short pages wrap. Video pages also
  advance within that bound. This is wider coverage of an accessible catalogue,
  not uniform sampling of the network or an exhaustive crawl.
- Candidate order within a page rotates by a persistent seed and round. Profile
  hints indicating some activity get priority on active rounds; unknown or empty
  profiles remain eligible, and newcomer rounds omit this priority. Follower,
  favourite and view counts do not rank candidates. All hints still require
  canonical verification.
- Outbox rounds choose one canonical post from at most 32 observed items, rotating
  the position on later successful rounds. Video pages select at most two distinct
  owners. Ordinary post/video browse pages avoid repeated authors; explicit
  searches and channel browsing retain their requested scope. Stable author
  ordinals preserve continuation order when other entries disappear.
- Within the existing exploration/maintenance scheduling lanes, request receipts
  are balanced by approximate candidate cost: profile 1, community 2, post 3,
  video 4. This is weighted scheduling, not reserved per-family quotas. Origin
  usage breaks ties. Global/origin/body allowances remain hard limits.
- Peer-list sampling disperses its bounded window across the returned array.
  Lists up to 262,144 rows fit the parser, but the existing 2 MiB response cap
  still wins. Oversized responses continue to back off.
- CLI diagnostics report requests and charged bytes by content family and job
  operation, plus fresh catalogue entries, authors and origins per content kind.
  The admin page shows request costs. These are operational coverage measures;
  they do not claim that a profile is interesting or that spam has been solved.

The reproducible nine-origin guarded-HTTP fixture produced 9 profiles, 9 posts
from distinct authors, 9 communities and 6 videos in 96 wire attempts. Recreating
the application state and making the next round due continued at profile offset
20, then stopped at exactly 100 attempts; every origin stayed at or below 25.
No ordinary remote accounts were imported. The fixture deliberately includes
prolific authors and repeated video owners; these do not gain extra sampling
slots. This demonstrates bounded breadth on a known graph, not live content
quality or a statistically representative network sample.

All 154 selected discovery, policy, store and transport regressions passed after
the scheduler change. The Chromium journey passed desktop, mobile, 320 px, dark
mode and both no-JavaScript layouts, including a populated cost table, keyboard
access and WCAG checks. Static checks passed formatting, Clippy with warnings
denied, offline compilation and 18 budget-gate tests. SQLx matched 1,917 effective
query records. An isolated transaction with 100 sources, 1,000 queued jobs and
500 receipts measured 16.7 ms for candidate selection after pre-aggregating cost
counters, compared with 85.3 ms for repeated per-job counts on the same fixture.
These are local query measurements, not a production throughput guarantee.

The complete hot-path benchmark ran after reseeding its disposable database to
account for the new discovery singleton rows. Its strict gate did **not** pass:
account/group/profile and notification paths exceeded SQL-count budgets, and
web notifications exceeded latency and response-size ceilings (245.4 ms and
354,988 bytes). Several inbox write timings also had less than 25% headroom.
The affected ordinary read handlers are byte-for-byte unchanged from the prior
staging source manifest. This is not a passing release performance qualification;
retain the full report and investigate the general application/budget baseline
separately. No benchmark thresholds were relaxed. The limited staging rollout
uses the passing discovery-specific checks and unchanged conservative allowances.

Historical pilot counters remain intact; a new deployment does not reset the
rolling allowance. Live staging observation lasted 316.673 seconds, including
more than five minutes after new work began. The remaining 50 requests added
3 profiles, 2 posts, 5 communities and 4 videos. The crawler stopped at exactly
100 requests for the rolling window; every origin stayed at or below 25, with
at most two requests in flight globally and one per origin. Charged bodies
totalled 5,961,596 bytes against the 16 MiB allowance. Readiness stayed HTTP 200
and the observer recorded no stop conditions. Evidence is retained under
`target/discovery-staging/2026-10-16-diversity/`.

The observed community results motivated the subsequent `Active` → `TopMonth`
adjustment. That final sort change is deployed without resetting the exhausted
allowance, so its live results await the next budget window. At the user's
request, no test or benchmark suites are rerun for this small adjustment; only
the build, deployment and basic service readiness are checked. The earlier
validation results above precede this final adjustment. Broader coverage is
demonstrated; content quality still needs observation over later rounds.

### Staging cost calibration and resolved avatars — 16 October 2026

After the 100-request pilot exhausted its allowance, the next staging allowance
is 500 requests per rolling day, 75 per origin and 32 MiB of charged bodies.
Concurrency remains two globally and one per origin; the 2 MiB per-response cap,
200-entry capacity and existing receipts remain intact. This provides room for
several candidate rounds without turning the small pilot into an unbounded crawl.

Peer exploration now requires a verified, fresh profile-directory or peer-list
capability. Known community/video sources no longer receive speculative Mastodon
peer probes, and older queued inapplicable probes retire without network traffic.
Embedded replies are excluded from outbox hints before canonical fetches. The CLI
exposes peer failures separately, and acquisition failures retain their detailed
reason in the service log.

Discovery cards previously drew placeholders unconditionally. They now batch-read
existing resolved account avatars, using cached files or the normal media proxy,
including post authors, video channels/owners and known server actors. This does
not import identities or fetch metadata while rendering. Unknown identities and
servers without a known actor image retain their fallback; no favicon URL is
invented. Images load lazily, and failed loads expose the fallback again with
JavaScript enabled. The user requested no further test suites or benchmarks for
this iteration; verification is limited to the deployment build, live rendering
and bounded observation of the adjusted staging allowance.

The calibration observation captured 301.647 seconds of new work. An additional
109 requests added 14 profiles, 7 posts, 13 communities and 5 videos (39 entries),
bringing the fresh catalogue to 73 entries and verified sources from 9 to 13.
Readiness remained HTTP 200; no limit or concurrency breaches occurred. The
busiest origin reached 40 of its 75 requests. Charged bodies reached 21,570,674
bytes, mostly because unsuccessful sends retained full body reservations.

Detailed logs identified three DNS lookup failures and one timeout among the
new server hints. The subsequent transport correction settles a failed send
with zero body bytes while retaining its request charge; incomplete body reads
and lost workers keep their full reservation. Generic fetch retries now start
at 15 minutes and increase exponentially up to a day, while rate-limit and
explicit refusal handling keep their existing rules. The four currently failing
unverified sources were placed on a one-hour backoff during deployment. Historical
receipts were not reset or refunded. Evidence is under
`target/discovery-staging/2026-10-16-calibration/`.

## Recovery and discovery usefulness plan — 19 October 2026

Status: implemented and deployed to staging on 19 October. This section supersedes the
earlier pilot's one-day visibility lifetime, rotating-source eviction and tiny
catalogue capacities when implemented. The user requests a 1,000-request daily
allowance and explicit prevention of repeat acquisition. Audio/Songhive remains
deferred. The original planning update made no staging changes. The implementation
uses migrations 0197–0201 and a staging deployment; no test suites or benchmarks
are run, as requested.

The investigation is saved in
`target/discovery-staging/2026-10-19-investigation/`. At the snapshot there were
84 eligible profiles, 18 posts and no communities or videos. All six bundled
community/video sources and their entries had been removed. In the retained
24-hour receipt window, 120 of 500 requests went to those six sources. Eleven
stored posts received less than one hour of eligibility after verification; one
expired after 35 seconds. The deployed binary contains the implicated eviction,
expiry scheduling and oldest-first ordering logic. These findings require
correctness changes before further sampling or budget tuning.

### 1. Retention is independent of refresh scheduling and presentation

Proposed operator-configurable defaults:

| Record | Retention | Background recheck target |
| --- | --- | --- |
| Established source, profile, community | No automatic age deletion | Sources and actively recommended profiles: 14 days; communities: 30 days; quiet records back off toward 90 days |
| Post metadata/excerpt | 180 days from first discovery | No routine reread of every post; check on credible change evidence, explicit user resolution or selected maintenance |
| Video metadata | 365 days from first discovery | 90 days for the active recommendation set; earlier on change evidence or explicit resolution |
| Canonical identity and alias ledger | At least 365 days after positive metadata is retired; longer while still encountered | Local lookup only; encountering a known ID does not force network verification |

The recheck intervals are scheduling targets within the maintenance allowance,
not deadlines that hide or delete cards. Keep `first_discovered_at`, publication
time, last content change, last verification, next check and retention deadline
separate. Reverification must not make an old post look newly published or newly
discovered, or silently renew its content-retention clock. The background worker
maintains a bounded active recommendation set; it does not reread the entire
archive on every interval.

Known denials, withdrawals, deletion evidence and local moderation continue to
suppress content immediately and preserve refusal guards. A timeout or expired
directory capability check is not withdrawal evidence. Source health does not
blanket-hide previously verified content. Ordinary resolved/imported content,
bookmarks and follows are outside discovery-cache eviction.

Remove source admission's cascading deletion of jobs and entries. Pin bundled,
operator and imported sources against automatic replacement. Store unverified
server hints separately; expire unsuccessful hints without removing established
catalogues. Admission pressure must not evict useful sources merely because
they have no peer-list children. Restore the six missing starters only after
these protections are installed.

Raise the positive metadata safety ceiling from 200 to a configurable 100,000
records, without preallocating or eagerly filling it. Remove the fixed four-post
author and eight-video-server limits, and the 50-video ceiling; use broad
configurable origin/storage quotas and per-page diversity instead. Use indexed,
bounded reads rather than ranking the whole expanded catalogue on each request.
Monitor actual metadata/index storage. Capacity pressure must be visible and
defer admission when needed; it must not silently override retention to admit
unverified hints. Retire retention-expired positive metadata first, keeping its
identity ledger and all applicable negative guards.

### 2. Do not reacquire known, unchanged content

Introduce a durable ledger keyed by canonical URI, with verified aliases,
content kind, source, last advertised revision/update hint, last verified
revision, outcome, next permissible check and retry state. Backfill it from
retained entries, existing ordinary resolved identities and pending jobs where
canonical identity is known. Reuse existing verified public metadata only after
the discovery-specific consent and visibility checks; ordinary resolution is
not blanket eligibility. Keep job uniqueness across aliases. Stable refusal
guards cannot be erased by alias mapping, capacity eviction or rediscovery.

Run one bounded batch lookup before selecting candidates from any directory,
outbox or video page. Classify each candidate as new, known unchanged, already
queued, changed/due for maintenance, or refused/backed off. Known unchanged and
already queued candidates trigger no individual object or actor fetch. Apply
selection quotas after this classification, filling slots from new eligible
candidates on the already-fetched page. Retired positive metadata must not turn
back into a "new" discovery when its URI reappears.

Keep collection cursors/checkpoints and known IDs across restarts and source
refreshes. Poll productive collection heads to learn about new IDs; use bounded
deeper exploration separately. Do not reset to page one for routine metadata
renewal. Increase polling intervals after empty/duplicate-only responses and
stop a pass after two consecutive zero-yield pages. Retain admitted new hints
so a deferred verification does not require refetching their directory page.

Changed `updated`/version hints may nominate maintenance, with per-object rate
limits; untrusted churn must not bypass the budget. Reuse sufficiently recent
verified actor/channel evidence across multiple new objects. Recheck a parent
before dependent content when required; remove the conflicting fixed 23-hour
refresh exclusion. Conditional HTTP requests are useful when a check is due,
but a 304 still consumes a request and is not a deduplication solution.

Concrete request contract: a directory page containing 20 known unchanged URIs
costs its one listing request and zero individual verification requests. A page
with six new URIs queues only those six, subject to admission and verification
limits. Rediscovering an old URI after its card was retired is a ledger hit.
New content from a known author may still be acquired; the known author need
not be downloaded again merely because each new object names it.

### 3. A 1,000-request budget with productive allocation and pacing

Increase staging's rolling 24-hour request ceiling from 500 to 1,000 after the
retention and deduplication changes are deployed. Preserve current receipts.
Keep the existing 75-request origin ceiling, 32 MiB charged-body ceiling, 2 MiB
per-response cap and concurrency of two globally/one per origin initially.
These were not the binding limits in the incident snapshot.

Start with 700 requests for new content from established sources, 200 for
maintenance and 100 for new-server exploration. The exploration ceiling includes
peer lists, capability fallbacks and initial source validation, not just the
first peer-list request. Unused maintenance allowance may serve new content when
no priority maintenance is due; transfers are recorded. Failed speculative
probes must not count as useful community/video acquisition for balancing.

Within acquisition, protect opportunities for all four content kinds and prefer
verification of already-admitted new candidates over further hint collection.
Maintenance prioritizes useful exposed records and shared dependencies, not
age-only sweeps of every stored object. Use measured new eligible records per
request and per-source failure/duplicate yield to adapt schedules.

Pace background use across the day (approximately 42 requests/hour on average,
with a bounded startup burst), rather than consume the day in one cluster.
Budget-deferred jobs sleep until an allowance can actually become available;
they must not repeatedly claim work every five minutes. Keep logical claims,
wire attempts and failures as distinct counters. Never raise concurrency as a
substitute for reducing duplicate work.

### 4. Default to discovery the member has not already seen

Default ordering is "New to you": prefer unseen eligible identities/content,
then apply activity/recency, topic/language relevance when known, and author and
server diversity. Account creation date is not a positive quality signal.
Use bounded local per-member impression history based on actual visibility,
plus opens/dismissals; sending HTML alone does not prove the user saw a card.
Retain seen history for 90 days, provide a reset/show-seen control, and do not
delete shared catalogue entries when one member sees or dismisses them.

For posts, prefer substantive standalone content published in the last 30 days;
for videos, prefer the last 90 days while retaining evergreen material. These
are ranking preferences, not hiding/deletion cutoffs. Expand the age mix when
the relevant unseen pool is small. Show real publication dates. Penalize
duplicate text/links, repetitive promotional templates and repeated authors;
do not use follower count as the definition of interesting content or blanket-ban
all bots and feeds.

Add explicit sorting: New to you, Recently discovered, Latest published for
posts/videos, and Recently active for people/communities where reliable activity
evidence exists. Add "Surprise me" using an explicit per-request seed and unseen,
diverse candidates; preserve the seed and ordering in pagination. It selects
from retained eligible metadata and does not launch a crawl. Offer language,
topic and server filters, including an unknown-language path. Fill pages with
other eligible authors/servers instead of stopping at the first capped author.
If the unseen pool is exhausted, say so and offer seen/older results rather than
silently replaying the same first page.

### 5. Make usefulness and losses measurable

Keep 90 days of daily aggregates by source, content kind and work purpose:
requests, candidate hints, duplicate/queued skips, distinct additions, actual
content updates, unchanged maintenance, refusals, failures, retention expiry and
evictions. Record each source/entry removal reason and affected counts; those
records must survive deletion of the source. Add source productivity and retry
diagnostics, retained versus eligible/recommendable inventory, daily additions,
and optional local impressions/first-page repeat rate. Track hidden reasons and
newly refreshed records with negligible useful lifetime as explicit defects.

### Delivery order and completion criteria

1. Preserve the investigation snapshot; deploy source protection, independent
   retention/refresh fields and the identity ledger/backfill. Restore the six
   sources and retain their new results. Do not reconstruct unrecorded history.
2. Route every acquisition adapter through deduplication, shared dependency
   checks and paced budget allocation. Apply the 1,000-request staging ceiling
   without clearing prior receipts or retry/withdrawal state.
3. Deliver New to you, sorting/filtering and Surprise me against the persistent
   catalogue, with working pagination and explicit seen-content controls.
4. Evaluate real staging outcomes over 72 hours. The observation must cover an
   old 24-hour boundary, restarts and source-capacity pressure before declaring
   lifecycle recovery complete. Do not substitute a large benchmark run for
   these product outcomes or schedule follow-up work without a separate request.

Required outcomes: no established catalogue erased to admit a peer hint; no
visibility loss caused solely by an expired source-health timer; no fresh post
given seconds of useful life by a stale parent; known unchanged candidates incur
zero per-object acquisition requests; all four kinds retain inventory; default
pages favor unseen content while any exists; every request and removal has an
explainable purpose/outcome. Report baseline versus 72-hour unique additions,
retained inventory, request cost and first-page repetition. Validate the final
defaults against those observations rather than promising a fixed discovery
yield from 1,000 requests.

### Implementation notes — 19 October

- `discovery_identity` retains canonical identity, verified aliases, fingerprint,
  advertised changes, refusal state and recheck schedule separately from catalogue
  text. The legacy entry `valid_until` now means metadata retention. Profiles and
  communities use the non-expiring sentinel `9999-01-01`; posts use 180 days and
  videos 365 days. Reverification preserves publication and first-discovery dates.
- Source rotation and author/video storage caps are removed. Only expired positive
  post/video metadata may make room under configured storage pressure. Unverified
  peers move into a separate bounded hints queue with their prior source record and
  backoff; established sources and their content remain intact.
- The entire fetched directory/video/outbox page is classified before admission.
  Pending jobs and the bounded overflow queue preserve new candidates. Duplicate
  pages back off; outboxes persist their next-page address and periodically revisit
  their head. Cached minimal video owner/channel evidence lasts 14 days and is
  invalidated by authenticated negative evidence.
- Wire receipts carry acquisition/maintenance/exploration purpose independently of
  their content family. The 70/20/10 allocation allows unused maintenance allowance
  to flow to acquisition. A 42-token pacing bucket replenishes at the configured
  daily rate; already-started verification chains may finish within a bounded
  16-token overdraft. Request/origin/byte/concurrency ceilings still apply to every
  wire attempt. Budget deferrals wait for the relevant rolling allowance or token.
- Main discovery tabs share sort, language/topic/server filtering and cached
  browsing journeys. Ranking considers at most 2,000 candidates per journey;
  recent/unseen queries and stable random windows avoid sorting an unbounded
  catalogue. Journeys last two hours and are capped at eight per account. Viewed
  cards are recorded after one second at least half visible; history retains
  90 days and at most 10,000 entries per member. Opens and dismissals also count.
- Daily outcome aggregates retain 90 days, including duplicate/queued skips,
  useful additions, actual metadata changes, unchanged maintenance, failures,
  wire attempts, byte settlements, source removals, capacity deferrals and
  first/repeat visible impressions.
  CLI status and the administrator page expose the inventory and outcomes.
- Existing entries predate the identity ledger: migration 0201 recovers their
  original admission time from the time-ordered entry ID. Future refreshes cannot
  extend that baseline. The same IDs determine “Recently discovered” ordering.
- Deployment readiness, real browser journeys and a bounded live observation are
  the validation for this change. The separate 72-hour usefulness assessment is
  still required; no recurring observer is scheduled by this implementation.

### Staging deployment result

The 1,000-request rolling allowance is enabled with the existing 75/origin,
32 MiB body allowance and 2 global / 1 per-origin concurrent requests. All six
missing bundled sources were restored after migration. The backed-up catalogue
remains present; established sources are not replaced to admit peers.

At the recorded final snapshot: 104 active profiles, 35 posts, one community and
14 videos (one additional denied profile retained as a guard). This is 22 new
active records from 52 wire attempts since the pre-deployment 500-request baseline.
The uninterrupted observer ran for 346.635 seconds, including more than five
minutes after new work started, with no limit breaches. Global/origin concurrency
peaked at 2/1. The first observation correctly paused on an intentional deployment
restart; acquisition was resumed and the observation repeated. A final small fix
propagates canonical video-owner/channel refusals to their other retained videos;
its deployment was followed by readiness, binary-hash and retention inspection.

Evidence: `target/discovery-staging/2026-10-19-recovery/summary.json`, full snapshots
and observer records in the same directory. Browser verification covered New to
you, cached Surprise pagination, Latest published and the recovered video cards.
The signed-in browser account is not an administrator, so the administrator data
was inspected through the CLI; its browser page correctly refused that account.
UI verification contributes the initial impression counters. No test suites or
benchmarks were run. The 72-hour usefulness assessment remains separate.

## Relationship to the preliminary backlog

Changes 1–6 deliver a narrowed form of DISC-01, DISC-02, DISC-03, DISC-04,
DISC-06, DISC-07 and DISC-08. Lifecycle protection from DISC-11 and operational
visibility from DISC-14 are included from the beginning. DISC-05 and content
search follow in increment 7; DISC-12, DISC-09, DISC-10 and DISC-13 follow in
increments 8–11 respectively. Full release qualification from DISC-14 remains
the milestone B rollout gate.

### Discovery usability correction (19 October 2026)

- Each main tab has one GET form: search (including hashtags), sort, and a language
  selector. Language uses the shared native select / searchable JS combobox.
  Separate topic/search text inputs and bottom history/autoload controls are gone.
- JavaScript automatically appends pages; without JavaScript the next-page link
  navigates normally. A failed automatic load exposes that same navigation link.
- For JS, seen still means a card was at least half visible for one second.
  Without JS, a delivered page counts as read: a `noscript` image consumes an
  unguessable, account-bound, one-use page receipt. Receipts expire after two
  hours, are capped at 32/account, and contain only the rendered entry IDs. No
  session or CSRF credential is put into the image URL. Blocking images as well
  as scripts leaves explicit opens/dismissals as the available reading signal.
- On a fresh visit with no eligible unseen results, the selected sort becomes
  Latest published for posts/videos or Recently discovered for people/communities.
  Filters stay intact. Continuations retain their stable ordering; no mid-scroll
  fallback can splice previously seen content into an unseen journey.
- Verified actor usernames are retained separately from titles. Existing imported
  account handles and cached canonical actor evidence backfill them immediately;
  older catalogue actors with discarded usernames get a paced canonical refresh.
  Cards show full person/community/author handles rather than just the origin.
- Server metadata and favicon acquisition share discovery request, byte, pacing,
  policy and concurrency limits. Published icon CDNs are permitted only after
  their URL is stored for that source, and their actual origin is charged.
  Supported images are decoded with allocation/dimension limits and reduced to
  local 96-pixel PNGs. Success is cached for 30 days, failure backs off for a day.
  Rendering a server card never makes an origin request.
- Signed-in normal web search includes eligible catalogue cards, deduplicated
  against ordinary account/status results. Search-context content filters and
  current discovery/indexing/moderation rules apply; seen state is irrelevant.
  `/api/v2/search` exposes unresolved catalogue matches in an additive `discovery`
  array, respecting type/offset/limit; it does not fabricate imported Mastodon IDs.
  Third-party clients must support that extension to display unresolved matches.
  Normal exact resolution still produces standard account/status entities.

Quiet sources also receive dedicated presentation maintenance jobs, which only
refresh branding rather than resample a directory. One scheduler turn in five
prefers due maintenance so a persistent new-content backlog cannot starve old
handle/icon repairs. All existing hard limits remain in force. JS append responses
omit no-script reading receipts, including when parsed by an inert HTML document.

The language audit also found that the original adapters always stored `NULL`.
Canonical profile/post/video verification now retains declared language metadata;
video REST declarations are accepted from the verified public detail response.
Migration 0204 reuses language from already imported canonical statuses. Existing
public video catalogue pages can fill missing language on retained matching videos
without re-fetching each video, refreshing consent or changing retention. A base
language filter includes its declared regional variants. No language is guessed
from a server's country or the apparent language of a person's name.

#### Staging verification of the usability correction

The final build deployed successfully with migrations through 0204. Readiness is
HTTP 200, the deployed binary matches the locally built binary, and its service
invocation has no warning/error journal entries. No test suites or benchmarks ran.

Focused browser checks confirmed the searchable selector, three English-language
post matches, verified author handles, ordinary search returning the catalogue-only
OCaml community, and automatic pagination to 53 unique people and all 37 servers.
The page-receipt count stayed at eight across JS page appends. Cached server images
and an existing manually resolved server avatar both loaded successfully.

The no-script receipt flow was exercised directly: consuming a page of nine unseen
communities made the next visit select Recently discovered and display all eleven
then-retained communities. Native select/next-link markup was inspected. A fully
scripts-disabled Chrome run remains unverified: the browser tool blocks access to
Chrome settings, and no browser-policy workaround was attempted.

Historical metadata repair remains asynchronous. At the recorded snapshot, handles
were available for 23 of 123 retained actors and four of 37 server icons had been
cached; existing resolved images are also reused. Acquisition remained unpaused at
601/1,000 rolling requests, with the busiest origin at 44/75. Older cards can retain
fallback identities/images until their bounded maintenance jobs finish.

Evidence: `target/discovery-staging/2026-10-19-ux/` contains the recoverable backup
reference, deployment status, before/after append counters, and the final UI image.
Six Chrome review tabs were left open for people, posts, communities, videos,
servers and normal search.

#### Handle layout, repair scheduling, and exhausted-unseen feedback

Deployed 2026-10-19 (build 266). Handles occupy their own full-width card row,
with a break opportunity between username and domain. Prefixes stay attached;
exceptionally long components wrap without hiding identity. Missing metadata no
longer renders a canonical URL as a handle. The same rendering covers people,
communities, and post/video authors.

Initial exhausted New to you selections now explain the switch below the filters,
naming Recently discovered or Latest published and saying that seen items are
included. The notice is server rendered and is not repeated on cursor appends.
Removed the obsolete empty-state advice about controls below the page.

Repair scheduling now advances its fairness turn only on the first actual wire
request of a job, rather than on budget-denied claims. Maintenance prioritizes
missing presentation metadata. Scheduling excludes already queued entries from
its bounded selection, so the first 64 cannot mask later actors indefinitely.
Icon admission runs before new work, removes completed redundant icon jobs, and
uses dedicated maintenance jobs instead of blocking unrelated actor acquisition.
Already fetched, same-origin public catalogues can fill missing usernames for
existing independently verified active actors without per-actor fetches or
changing eligibility, consent, or retention. All network limits remain in place.

Focused live checks: World News shows !worldnews@lemmy.ml in a full-width row,
including at 280px card width. The people view displays full handles without
horizontal overflow at that width. Joe's normal Open profile resolution supplied
@joe@wikis.world immediately to the discovery card; this was a manual-resolution
check, not evidence that the historical backfill is complete. Exhausted community
and post searches displayed the correct explanation and selected fallback sort.
The server list appended to all 37 cards; cached local PNG icons loaded. Remaining
historical metadata acquisition is still pending under the paced worker.

One build/deploy completed successfully; no test suite or benchmark ran. Readiness,
binary hash, current invocation warnings and budget counters are recorded in
`target/discovery-staging/2026-10-19-ux/card-repair-verified.json`. Screenshots are
`caught-up-handle.png` and `people-handles.png` in that directory. Chrome review
tabs remain open for people, communities, servers and the post fallback.
