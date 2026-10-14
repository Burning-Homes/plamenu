# Phanpy and Plamenu feature parity

Audit date: **2026-10-14**. Phanpy was updated with a fast-forward pull from
`origin/main` to [`1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059`][phanpy-commit].
The checkout remained clean. The server snapshot is Plamenu
`25e4f9836715764872d9d336e3d7b6747c45530c`, running on staging as build 256.
Phanpy declares `masto ~8.0.0` and `compare-versions ~6.1.1`.

This compares features that current Phanpy can consume through Plamenu's client
APIs. It separates missing server functionality from client compatibility
problems, disabled operator configuration, and conveniences implemented wholly
inside Phanpy. It is not a comparison of every first-party Plamenu screen with
Phanpy's interface.

The most consequential findings are collection reports losing their selected
target, advanced search operators being interpreted as ordinary text, and
Phanpy's software detection hiding existing server features. That last problem
can change an exclusive list when it is renamed and prevent edited attachment
alt text from being saved.

## Missing or partial server features

| Feature Phanpy offers | Plamenu behavior at this snapshot | Evidence and consequence |
| --- | --- | --- |
| Report an account collection | Reports accept the request but discard `collection_ids`; the response always contains `collection_ids: []`. | [Phanpy sends the selected collection][report-collection]. The [input parser](../crates/server/src/routes/reports.rs) has no collection field and the [report serializer](../crates/server/src/entities.rs) returns an empty array. Reproduced through HTTP with a real disposable collection. The report targets its owner without preserving the selected collection for moderators. |
| Search account collections | `/api/v2/search` always leaves `collections` empty. | [Phanpy renders collection search results][search-client]. The [server search handler](../crates/server/src/routes/search.rs) only fills accounts, statuses, and hashtags. A discoverable collection was readable by ID but returned no name-search result in the HTTP probe. |
| Resolve a collection URL through search | URL search understands accounts and statuses, not collections. | The [server's `UrlResource` and URL search branches](../crates/server/src/routes/search.rs) have no collection variant. Source inspection; no separate remote collection-resolution experiment. Direct collection-by-ID reads are implemented. |
| Advanced status-search syntax | Full-text phrases, exclusions, and `or` work, but Mastodon operators such as `from:` and `before:` are not parsed. | [Phanpy's Year in Posts][year-in-posts] emits `from:<acct> before:<date>`. The [search route](../crates/server/src/routes/search.rs) passes raw `q` to [PostgreSQL text search](../crates/db/src/status.rs). The probe found an ordinary matching post but no result for its author's `from:`/`before:` query. The separate `account_id` parameter exists. Year in Posts falls back to account-post pagination, so its summary is not entirely unavailable. |
| Server-generated annual report / Wrapstodon | `/api/v1/annual_reports/{year}` has no implementation; the probe returned 404. | [Phanpy fetches an annual report][annual-report] and recognizes its notification. Plamenu recognizes the notification type in preferences but has no report generation, retrieval, or real delivery flow. This is distinct from Phanpy's client-generated Year in Posts. |
| Severed-relationship events and history | No event producer, structured notification event, or corresponding history page was found. | [Phanpy expects `event` details and an instance history URL][special-notifications]. Plamenu lists the notification type but lacks the associated model and lifecycle. Finding from source inspection, not a live domain-moderation experiment. Ordinary blocking, muting, and follow management are implemented. |
| Subscribe to an account's posts by email | `/api/v1/accounts/{id}/email_subscriptions` deliberately returns 404. | [Phanpy has an email subscription form][email-form], shown only when an account advertises support. The [Plamenu route](../crates/server/src/routes/emails.rs) explicitly has no newsletter implementation; confirmed through HTTP. Account confirmation and ordinary notification email are separate implemented features. |

Account collections themselves are not missing: create, edit, delete, profile
listing, membership, consent, withdrawal, and federation have implementations
and [integration coverage](../crates/server/tests/account_collections.rs).
The gaps above concern discovery and reporting around that existing feature.

The report parser also ignores Phanpy's `forward_to_domains` field. This audit
does not certify multi-domain collection report forwarding. The reproduced
failure is target loss with `forward: false`, without any remote delivery.

## Existing server features Phanpy hides or mishandles

Staging's public metadata says:

```json
{
  "version": "4.7.1 (compatible; Plamenu 0.7.0-staging.256+sha.25e4f9836715)",
  "api_versions": { "mastodon": 11 },
  "nodeinfo_software_name": "plamenu"
}
```

The first two fields come from `/api/v2/instance`; the last is the software name
in `/nodeinfo/2.0`, abbreviated here rather than presented as an instance field.
The metadata implementations are [instance API](../crates/server/src/routes/api.rs)
and [NodeInfo](../crates/ap/src/nodeinfo.rs).

[Phanpy's `supports()`][supports] checks the NodeInfo software name as well as
the version for features in its [feature table][feature-flags]. A compatible
version does not satisfy an `@mastodon/...` feature when the software name is
`plamenu`. Executing that exact function with staging's metadata and its declared
comparison-library version returned false for every software-specific feature
in the table, while basic lists and profile editing returned true.

| Feature | Effect in current Phanpy | Existing Plamenu support |
| --- | --- | --- |
| Exclusive lists | The [list editor][list-editor] hides the exclusive checkbox, but always sends `exclusive: false` when the field is absent. Renaming an exclusive list can therefore clear its setting. The HTTP probe replayed that payload and confirmed the change. | [List API and exclusive home filtering tests](../crates/server/tests/lists.rs). The server is correctly applying the explicit submitted value. |
| Edit alt text on an existing attachment | The [composer][composer-media] skips uploading attachments that already have an ID. For edits it sends `media_attributes` only when `@mastodon` or the GoToSocial capability matches; neither matches Plamenu. The edited descriptions never reach the server. | [Status edit parser](../crates/server/src/routes/statuses_api.rs), with `edit_applies_media_attributes` in [edit tests](../crates/server/tests/edits.rs). New upload descriptions and ordinary post editing are supported. |
| Filtered-notification policy and request inbox | [Notifications][notification-gates] do not expose/fetch these controls because `filtered-notifications` is false. The bot filter is also hidden behind its own software gate. | [Policy and request endpoints](../crates/server/src/routes/notification_filtering.rs), including accept/filter/drop decisions and bot policy, with [integration tests](../crates/server/tests/notification_filtering.rs). Grouped notifications remain enabled separately. |
| Profile endorsements / account recommendations | [Endorsement display][endorsements] and related controls are gated off. | Endorse/unendorse, profile lists, relationship flags, and pin aliases have [account API coverage](../crates/server/tests/accounts_api.rs). |
| Posts discussing a trending link | The [trending link drill-down][trending-gate] is unavailable because `trending-link-posts` is false. | `/api/v1/timelines/link` is implemented in [trends routes](../crates/server/src/routes/trends.rs), with [trends coverage](../crates/server/tests/trends.rs). |
| Batch hydration of reply ancestors | The [thread hydration path][batch-gate] falls back to individual status requests because its batch-fetch gate is false. This is an efficiency gap, not an inability to open threads. Other Phanpy batch-fetch paths are independent. | `/api/v1/statuses?id[]=...` and request-order/visibility/limit behavior have [client API tests](../crates/server/tests/client_api.rs). |
| Compose a local-only post | The [visibility selector][local-visibility] enables this only for Pleroma/Akkoma version strings. Plamenu is neither, so the option is absent. | Local-only visibility has [client API](../crates/server/tests/client_api.rs) and [streaming coverage](../crates/server/tests/streaming.rs), as well as a native compose option. |

Some newer capabilities use `api_versions.mastodon` instead of NodeInfo
identity: [quotes][quote-version] need revision 7, [profile editing][profile-version]
needs 9, [collections][collection-version] need 10, and
[grouped notifications][notification-version] need 2. Those checks pass with
Plamenu's advertised revision 11. It would be incorrect to classify quotes,
collections, profile editing, or grouped notifications as entirely hidden.

The two write-path problems need special care: a server cannot recover an alt
description the client never sent, and ignoring an explicit `exclusive: false`
would break intentional list changes. A compatibility fix must preserve those
valid operations. Simply relabeling Plamenu as Mastodon would also enable checks
for genuinely absent features such as annual reports; it is not a complete
solution. No Phanpy source changes were made in this audit.

## Moderation warning compatibility

Plamenu includes the standard `moderation_warning` object in both notification
versions: [v1 serialization](../crates/server/src/entities.rs) and
[v2 serialization](../crates/server/src/routes/notifications.rs).

[Phanpy's notification component][special-notifications] reads
`moderation_warning`, but its Masto SDK decodes that field to
`moderationWarning`. A synthetic response passed through Masto 8.0.0 confirmed
the camel-case field exists and the snake-case field does not. The notification
can appear without its warning details because the component tests the wrong
property. This is a client decoding/rendering problem, not a missing server
warning entity.

There is also an action-link problem: the component's
`/disputes/strikes/{id}` URL is relative to the Phanpy origin. Plamenu's existing
[warning history and appeal forms](../crates/server/src/web/strikes.rs) live at
`/settings/strikes` and `/web/settings/strikes/{id}/appeal`. Correct details and
navigation both need to be covered by an eventual interoperability fix.

## Implemented API families used by Phanpy

These entries have server implementations and existing regression coverage.
They were inspected during this audit; this is not a claim that every Phanpy
browser flow or every test below was freshly executed. Specific exceptions are
listed above.

| Phanpy feature family | Plamenu evidence / qualification |
| --- | --- |
| OAuth login, PKCE, tokens and scopes | [Authentication documentation](api/authentication.md), [scope tests](../crates/server/tests/oauth_scopes.rs), [token admission](../crates/server/tests/oauth_token_admission.rs). |
| Account lookup, search, profiles, followers and relationships | [Account API tests](../crates/server/tests/accounts_api.rs), [search tests](../crates/server/tests/search.rs). Includes familiar followers and private relationship notes. |
| Profile editing, avatar/header updates and deletion | [Profile tests](../crates/server/tests/profile.rs), including Masto multipart profile updates and the newer profile API. |
| Posts, replies, content warnings and visibility | [Client API tests](../crates/server/tests/client_api.rs), [posting limits](../crates/server/tests/posting_limits.rs). Local-only selection in Phanpy is the exception above. |
| Media upload and descriptions | [Media tests](../crates/server/tests/media.rs), [media boundary tests](../crates/server/tests/media_input_boundary.rs). Existing attachment description edits encounter the client gate above. |
| Post editing, source, history, deletion and thread context | [Edit tests](../crates/server/tests/edits.rs), [reply-fetch coverage](../crates/server/tests/reply_fetch.rs). |
| Native quotes, approval policy, quote lists and revocation | [Quote tests](../crates/server/tests/quotes.rs). Enabled through the API revision check. |
| Poll creation and voting | [Poll tests](../crates/server/tests/polls.rs), including federation and expiry notifications. |
| Boosts, favourites, bookmarks and pinned posts | [Interaction tests](../crates/server/tests/interactions.rs), [pins/bookmarks](../crates/server/tests/pins_bookmarks.rs). |
| Home, public/local, account, list and hashtag feeds | [Client API](../crates/server/tests/client_api.rs), [account feed](../crates/server/tests/account_feed.rs), [lists](../crates/server/tests/lists.rs), [tags](../crates/server/tests/tags.rs). |
| Streaming and read markers | [Streaming tests](../crates/server/tests/streaming.rs), [marker tests](../crates/server/tests/markers.rs). |
| Grouped notifications and unread handling | [Grouped notification tests](../crates/server/tests/notifications_v2.rs), [inbox tests](../crates/server/tests/notification_inbox.rs). API revision enables grouped reads; filtered-inbox UI has the separate gate above. |
| Unicode and custom emoji reactions in notifications | [Notification tests](../crates/server/tests/notifications_v2.rs), including the existing image fallback for clients with the custom-emoji field mismatch. This previously addressed issue is not a newly missing feature. |
| Direct-message conversations | [Direct message tests](../crates/server/tests/direct.rs), [streaming conversation events](../crates/server/tests/streaming.rs). |
| Push notification subscriptions and delivery | [Push tests](../crates/server/tests/push.rs), including token-scoped lifecycle, encryption, delivery and retries. Staging advertises a VAPID key; this audit did not send a browser push. |
| Follow requests, follow settings, blocking and muting | [Follow requests](../crates/server/tests/follow_requests.rs), [follow settings](../crates/server/tests/follow_settings.rs), [blocks/mutes](../crates/server/tests/blocks_mutes.rs). Severance-event/history support remains absent. |
| Lists, membership and reply policy | [List tests](../crates/server/tests/lists.rs). Exclusivity is implemented but mishandled by Phanpy's gated editor. |
| Content filters, warning/hiding rules and keywords | [Filter tests](../crates/server/tests/filters.rs). Distinct from notification request filtering. |
| Scheduled posts, rescheduling and cancellation | [Scheduled-status tests](../crates/server/tests/scheduled_statuses.rs). |
| Followed and featured hashtags | [Tag tests](../crates/server/tests/tags.rs). |
| Trends, tags, links and posts | [Trends tests](../crates/server/tests/trends.rs). Availability is also operator-controlled; link-post drill-down is gated in Phanpy. |
| Instance announcements | [Announcement tests](../crates/server/tests/announcements.rs), [announcement streaming](../crates/server/tests/streaming.rs). |
| Account/status reports | [Report tests](../crates/server/tests/reports.rs). Collection targets are not preserved. |
| Account collections and membership approval | [Account collection tests](../crates/server/tests/account_collections.rs), [collection federation](../crates/server/tests/collections_federation.rs). Name/URL search and report targets are incomplete. |
| Managed actor notification context | [Actor ecosystem tests](../crates/server/tests/actor_ecosystem.rs) and the [notification compatibility contract](api/index.md#managed-actor-notifications). Existing fallback summaries/actions provide context in clients that do not render Plamenu-specific fields. |

## Configuration and client-only conveniences

**Translation is not an absent Plamenu API.** [Server translation](../crates/server/src/translation.rs)
and [its tests](../crates/server/tests/translation.rs) support configured providers.
Staging currently advertises `configuration.translation.enabled: false`, and
`/api/v1/instance/translation_languages` returns `{}`. This is a deployment
configuration distinction. Moreover, [Phanpy's translation component][translation-client]
uses a browser translator or its configured external Translang service, rather
than Plamenu's status-translation endpoint. Enabling server translation alone
does not change that client path. No post text was submitted to a translation
provider during this audit.

Phanpy's boost carousel, catch-up view, focus deck and multiple columns,
shortcuts, local drafts/recovery, keyboard navigation, text-to-speech, and
client-generated Year in Posts are primarily client behavior over ordinary
APIs. Their presence in Phanpy does not imply a missing Plamenu endpoint.
GIPHY, automatic alt-text generation, and Shazam integrations also depend on
client/provider configuration, not additional Plamenu Mastodon API support.

The reverse direction is a separate question: Plamenu's native channel, group,
music, video, and Webxdc controls are not all expressible in Phanpy's interface.
That is not a feature which Phanpy offers but the server lacks.

## Validation and next priorities

| Check executed in this audit | Result |
| --- | --- |
| Run current Phanpy `supports()` with verified staging metadata and `compare-versions` 6.1.1 | Software-specific flags false; basic list/profile flags true. |
| Decode a synthetic grouped warning with Masto 8.0.0 | `moderationWarning` present; `moderation_warning` absent. No network request. |
| Search a synthetic post normally, then with `from:`/`before:` | One ordinary match; zero operator-query matches. |
| Rename an exclusive list with the payload constructed by Phanpy's editor | `exclusive` changed from true to false. Payload replay, not browser interaction. |
| Read a discoverable account collection and search for its name | Direct read succeeded; collection search returned zero results. |
| Submit a local collection report without forwarding | Accepted, but returned `collection_ids: []`. |
| Request annual report; submit an account email subscription | Both returned 404. |

HTTP mutations used the production build in the disposable local E2E instance
`doomed.local`, with synthetic accounts and isolated storage. The instance was
stopped afterward. Staging was read only; no real users were contacted or
notified, no reports were forwarded, and no email or push was delivered.
Existing integration tests are cited as coverage evidence, not reported as a
new full-suite run. No runtime fixes or deployment were performed for this audit.

Recommended order:

1. Preserve collection report targets throughout validation, storage, moderator
   display, response serialization and, where applicable, federation.
2. Resolve the compatibility paths that lose list exclusivity or edited media
   descriptions, with regressions using actual client request shapes. Preserve
   truthful server identity and legitimate explicit writes.
3. Complete collection name search and URL resolution with visibility, consent,
   pagination and federation safeguards.
4. Implement structured status-search operators, starting with `from:`,
   `before:` and `after:`, without changing viewer visibility restrictions.
5. Make moderation warning details and action links usable in the client;
   account for SDK decoding and the separate client/server origins.
6. Decide whether annual reports, severance history and account newsletters are
   desired product features. They need their own lifecycle implementations;
   advertising their notification names alone is insufficient.

[phanpy-commit]: https://github.com/cheeaun/phanpy/commit/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059
[supports]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/utils/supports.js#L36
[feature-flags]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/data/features.json
[report-collection]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/report-collection.jsx#L50
[search-client]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/pages/search.jsx#L163
[year-in-posts]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/utils/year-in-posts.js#L83
[annual-report]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/pages/annual-report.jsx#L28
[email-form]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/email-subscription-form.jsx#L26
[list-editor]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/list-add-edit.jsx#L29
[composer-media]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/compose.jsx#L1441
[notification-gates]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/pages/notifications.jsx#L284
[endorsements]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/endorsements.jsx#L25
[trending-gate]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/pages/trending.jsx#L150
[batch-gate]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/utils/timeline-utils.js#L259
[local-visibility]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/compose.jsx#L2065
[quote-version]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/utils/quote-utils.js#L4
[profile-version]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/edit-profile-sheet.jsx#L158
[collection-version]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/utils/collections.js#L6
[notification-version]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/pages/notifications.jsx#L82
[special-notifications]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/notification.jsx#L633
[translation-client]: https://github.com/cheeaun/phanpy/blob/1e4e7d063aae3db59d9eaebe10e64b0b6ba6d059/src/components/translation-block.jsx#L40
