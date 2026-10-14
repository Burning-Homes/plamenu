# Clients and APIs

Plamenu provides a Mastodon-compatible REST API, OAuth 2, WebSocket streaming,
Web Push, and an experimental Lemmy `/api/v3` adapter.

## Sign in from an app

Enter your server's HTTPS hosting domain in a Mastodon-compatible client. The
app opens Plamenu's authorization page, where you sign in and approve access.
On a split-domain installation, use the hosting domain (for example,
`social.example.com`), even if your handle ends in `@example.com`.

## Build a client

- [OAuth and authentication](authentication.md)
- [Lemmy API](../LEMMY_API_COMPATIBILITY.md)

Test authentication, timelines, posting, uploads, and notifications against the
exact Plamenu version you intend to support. Plamenu-specific APIs may change
incompatibly during the experimental release series.

## Video playlists (experimental)

These are native Plamenu endpoints; see the [playlist guide](../VIDEO_PLAYLISTS.md).
Reads use `read:statuses` when authenticated; mutations require `write:statuses`.

- `GET /api/v1/video_playlists`: public catalogue, optionally `mine=true` for
  accessible native collections and `max_id` for older results.
- `POST /api/v1/video_playlists`: `channel_id`, `name`, optional `description`
  and `visibility` (`private` default, `public`, `unlisted`).
- `GET /api/v1/video_playlists/{id}`: metadata, revision and ordered entries.
  Each entry has a string `id`, position, nullable status/title/Video URI,
  `available`/`unresolved`, start seconds and optional stop seconds. Inaccessible
  Videos do not leak titles, status IDs or media. `can_edit` is channel-scoped.
- `POST /api/v1/video_playlists/{id}`: required current `revision` and operation:
  `profile` (name/description/visibility), `add` (status_id or video_url and clip
  bounds), `move` (entry_id, one-based position), `clip` (entry_id and bounds),
  `remove` (entry_id), or `delete` (confirm equal to the exact playlist title).
  Clip bounds are integer `start_seconds` and optional `stop_seconds`; omission
  of the stop clears it. Duplicate Videos are distinct stable entries.
- `POST /api/v1/video_playlists/import`: `url`, guarded canonical metadata import
  or refresh; remote ownership is retained and editing is not granted.

IDs are strings; `revision` is an integer. JSON and URL-encoded bodies work.
Unauthorized/private/deleted reads return 404, missing OAuth scope 403, invalid
limits/bounds/confirmation 422, and stale revisions 409. Native playlist and
outbox mutations are one transaction. Account/channel lifecycle and accepted
collaboration determine authority. Public/unlisted AP roots, pages and elements
are independently readable for PeerTube's auxiliary fetches; private ones are
never exposed. This does not weaken authorized fetch on general status routes.

## Native video (experimental)

These are Plamenu APIs, not a PeerTube REST API implementation. ActivityPub
video interoperability is tracked separately in the [AP09 inventory](../AP09-FEATURE-INVENTORY.md).

Mastodon-compatible Status entities add `video_channels`: an array of Account
entities identifying verified publication channels visible to the requesting
viewer. Ordinary posts carry `[]`. This describes where the video was published;
it does not enumerate the author's channel memberships or collaborator grants.

- `GET /api/v1/video_channels` (`read:accounts`): list channels you own or have
  accepted access to, with `owner_id` and `role` (`owner`, `publisher`, `editor`).
- `POST /api/v1/video_channels` (`write:accounts`): create a channel with
  `username` and `display_name`. Responses include string `id`, `username`,
  `display_name`, actor `uri`, and `owner_id`.
- `GET /api/v1/video_channel_invitations` (`read:accounts`): pending collaboration
  or ownership invitations, with `channel_id`, `display_name`, invited `role`
  and `ownership_transfer`. Invitations confer no authority until accepted.
- `GET /api/v1/video_channels/{id}` (`read:accounts`): accessible channel profile
  and your role. Only the owner receives its collaborator grant list.
- `POST /api/v1/video_channels/{id}` (`write:accounts`): channel lifecycle action.
  Send `operation`: `profile` (`display_name`, `description`, at most 1000
  characters); `invite` (`username`, `role`); `revoke` (`username`); `accept` or
  `refuse` (your invitation/accepted collaboration); `transfer` (`username`);
  `cancel_transfer`; `accept_transfer` or `refuse_transfer` (recipient only);
  `delete`. Ownership acceptance and deletion require `confirm` equal to the
  immutable channel username. Success is `{ok:true}`. Unauthorized targets
  return 404; validation returns 422; an unsafe active handoff returns 409.
  Role changes require fresh consent. Transfers revoke all former collaborator
  grants and invitations and confer no implicit privilege on the former owner.
  Finish/cancel unpublished uploads, scheduled videos and live broadcasts before
  accepting ownership. Existing Video authors, URLs and social identities remain
  unchanged. Channel deletion removes its videos, including collaborators'
  publications, but not their accounts. See [the channel guide](../VIDEO_CHANNELS.md).
- Upload video through the shared media endpoint, then
  `POST /api/v1/videos` (`write:statuses`) with `channel_id`, `media_id`, `title`,
  optional `description`, `language`, and `visibility` (`public` or `unlisted`).
  Optional `category` (1–18) and `licence` (1–9) use PeerTube's numeric IDs;
  integer strings work in form bodies. Zero clears either field. Neither is
  inferred: unspecified does not grant reuse permission. Local licence choices
  mean CC BY/BY-SA/BY-ND/BY-NC/BY-NC-SA/BY-NC-ND 4.0 (1–6), CC0 1.0 (7),
  Public Domain Mark 1.0 (8), or All rights reserved (9).
  `spoiler_text` is a content warning of at most 500 characters;
  `sensitive` marks the media sensitive, and a nonempty warning also does so.
  Write `#hashtags` in the description to use the existing tag system.
  The authenticated creator must own or have accepted access to the channel;
  the unattached processed media must belong to that creator. Publication
  retains the actual creator as the Video author, not the channel owner.
  Publication returns the ordinary Status entity with a playable
  video attachment. Titles have 3–120 characters; descriptions have a separate
  10,000-character budget, also bounded after HTML serialization.
  Supply an RFC 3339 `scheduled_at` at least five minutes ahead to reserve
  the processed video instead and receive a scheduled-status entity. List,
  reschedule, or cancel it through `/api/v1/scheduled_statuses`. The video stays
  private until its due publication, which uses the same channel and metadata.
  The first-party `/videos` review form also accepts a wall-clock publication
  time in the author's configured time zone without JavaScript.
- `PUT /api/v1/videos/{media_id}/metadata` (`write:statuses`): sparsely edit
  `category` and/or `licence` on your published native Video. This ID is the
  primary media attachment ID, not the Status ID. Omitted fields stay unchanged;
  zero clears. Invalid IDs return 422 and unauthorized publications return 404.
  Owners/editors can manage all channel videos; publishers can manage only their
  own. The same authority applies to the normal Status edit/delete API, caption
  and chapter managers. Revocation takes effect even for the original author.
  The response is `{category, licence}`, each null or `{id, name}`; licence
  also has `url` (null when no versioned link is established). Changes enqueue
  the normal signed Video Update atomically; an exact no-op does not edit the
  timestamp. Edit title, description, language and warnings through the existing
  Status editor. `/videos/{media_id}/metadata` is the equivalent owner-only HTML
  editor, linked from `/videos` and the Video's ordinary edit form.

JSON `PUT /api/v1/statuses/{id}` additionally accepts `media_accessibility` entries
`{id, transcript, decorative, audio_described, visuals_conveyed_in_audio}` for
kept attachments. Omit the array to retain alternatives; each supplied entry
fully replaces that attachment's structured alternatives. Null/empty transcript
clears it, a 50,000-character limit applies, and the two soundtrack claims
cannot both be true. The update shares the status edit/outbox transaction and
channel authority. The Status media entity includes equivalent keys and
language-specific caption/transcript links. See [Video accessibility](../VIDEO_ACCESSIBILITY.md)
and the [federated alternative vocabulary](../federation/media.md).

Video attachments include a Plamenu `video_metadata` extension when metadata
has been stored. This uses the same category/licence shape as the edit response,
in draft previews, scheduled statuses and published/imported Status entities.
Remote labels and links are not trusted. A recognized PeerTube licence ID alone
does not establish a version: it gets a generic name (for example, `CC BY-SA`)
and null URL, unless the AP object supplies the exact supported canonical URL.
Explicit clears federate as zero-ID `Unspecified` objects: PeerTube accepts
these markers, whereas omitted/null category/licence fields leave its old
values untouched on Update. Native Video Hashtag names are sent without a
leading `#`, matching PeerTube's tag vocabulary; ordinary Note tags are unchanged.

Account entities include the Plamenu extension `video_channel`: true for
native video channels and false otherwise. A channel still has `group: true`
and federates as an ActivityPub `Group`; it is not a community submission
target. Remote channel classification is not yet represented by this extension.
Clients must not infer channel ownership or authorization from this flag.

### Planned live starts (iCalendar)

For a locally authored native live attachment, the Status media entity's
Plamenu `live` extension includes `calendar_url` and `channel_calendar_url`.
These are HTTPS iCalendar subscription URLs, not PeerTube REST endpoints:

- `GET /videos/live/{status_id}/calendar.ics` lists future starts of one
  public or unlisted broadcast. An unlisted feed is accessible to anyone with
  its URL; do not put it in a public directory. Private/deleted broadcasts
  return 404.
- `GET /videos/channels/{channel_id}/calendar.ics` lists future starts from
  public broadcasts only. Unlisted broadcasts never appear there.
- Append `?reminder=15` to either URL to include a 15-minute display alarm,
  or `?reminder=0` for no alarm. Other values return 400. The subscriber's
  calendar app, not Plamenu, delivers the reminder.

Both feeds are readable without an API token so ordinary calendar clients can
subscribe. They support `ETag`/`If-None-Match` and have a five-minute cache
lifetime. The feed describes *planned starts*, not guaranteed on-air times or
durations. Importing an `.ics` file once may not receive later reschedules;
subscribe to its URL for updates. Calendar-client-specific refresh behavior
varies.

### Public status embeds

`GET /api/oembed?url={local-status-URL}` returns an iframe snippet for a
public or unlisted local post, including native videos and live broadcasts.
Its frame URL is `GET /web/statuses/{id}/embed`; this is read-only and always
renders as an anonymous viewer, even if the browser holds a Plamenu session.
Private, direct, local-only and deleted posts cannot be framed. The ordinary
first-party HLS player, captions, descriptions and PeerTube-compatible P2P
choice are present inside the frame. Peer sharing defaults on where supported;
the choice appears **before** Play, explains network-address disclosure, and
can be turned off to use the server only. With JavaScript disabled, the
native/progressive video remains available without P2P. The frame is a
Plamenu contract, not a claim of PeerTube's full embed API compatibility.

## Identity statements

FEP-c390 uses signed statements to link an actor to a user-owned key. These
Plamenu endpoints are separate from the retired Mastodon Keybase API:

- `GET /api/v1/accounts/:id/identity_statements`: original verified JSON statements.
- `POST /api/v1/accounts/identity_statements`: publish a signed statement as the JSON body.
- `DELETE /api/v1/accounts/identity_statements`: remove one with `{"subject":"did:key:…"}`.

Writes require OAuth `write:accounts` and return the remaining statements.
Publishing replaces a statement with the same subject; up to ten are allowed,
with a 16 KiB limit per document. Invalid statements return 422. Both writes
send an actor Update to connected peers.

Supported proofs use Ed25519 `did:key` subjects, `DataIntegrityProof`,
`assertionMethod`, and `eddsa-jcs-2022` (also its legacy name `jcs-eddsa-2022`).
The verification method must be the subject DID or its canonical `#z6Mk…`
fragment. The statement must name the exact actor ID in `alsoKnownAs`.
Expired proofs are excluded from responses. Ethereum and legacy prehashed
Minisign proofs are not supported.


## Emoji reaction notifications

`pleroma:emoji_reaction` notifications carry `emoji`: Unicode text or a
colon-wrapped custom emoji shortcode. Custom reactions also carry `emoji_url`,
using the instance's media URL or proxy. These fields are present on both v1
notification entities and v2 notification groups. V2 includes the reacted
`status_id` and the post in `statuses`, including on the single-group endpoint.
Streaming uses the v1 entity shape.

Use the notification's `emoji_url` to display a custom reaction. Libraries
that convert response keys to camel case expose this as `emojiUrl`.

Notification responses also include reaction images in the reacted post's
`emojis` dictionary for clients that use that fallback. The post's text emoji
are preserved. If a reaction shortcode names a different image, `emoji` uses a
separate display alias from that dictionary. These aliases are not picker
entries or identifiers for reaction actions; those use `emoji_reactions.name`.
Ordinary status endpoints and stored/federated content are unaffected.
Scoped channel/group reaction fallbacks include the emoji in their HTML title.

## Managed actor notifications

`GET /api/v1/notifications` and `/api/v2/notifications` include Plamenu's
`plamenu:actor.*` extension types: `follow`, `follow_request`, `favourite`,
`reblog`, `emoji_reaction`, `comment`, `mention`, `quote`, `status`, `invitation`,
and `transfer`. The recipient is a real local user; the notification's `account`
remains the person who performed the action. Never interpret a channel request
as a personal `/api/v1/follow_requests/{person}/authorize` operation.

An `actor_contexts` array identifies each relevant hosted actor. Each entry has
`actor` (public Account), `kind` (`channel` or `group`), `event` (the suffix above),
and an absolute `action_url`. Native review destinations authenticate the current
viewer and check their current role and CSRF token. Notification possession does
not grant permission. Post-bearing types embed `status` in v1 and use the ordinary
`status_id`/top-level `statuses` relationship in v2. A personal mention or quote
and a channel reason for the same post share one canonical notification ID;
channel reasons take presentation priority.

These extension types carry Mastodon's `fallback` object (`title`, `summary`,
`details`) with escaped HTML and ordinary links. It is also included when
`supported_types[]` is absent, so streaming and push-triggered single-notification
fetches remain readable. Phanpy already supports this fallback contract; its
source does not need modification. Clients with their own extension UI can use
`actor_contexts` instead. Group-scoped `admin.report` events also supply a safe
fallback and a public group context. Notification reports use Mastodon’s public
Report/Account shape, including for staff; administrative email/IP information
stays on the separately authorized administration endpoints.

Scoped kinds are individually selectable in notification delivery preferences.
For Web Push, an explicit scoped alert switch wins; otherwise its existing
Mastodon switch applies (`comment` maps to `mention`, invitations/transfers to
`follow_request`). Other channel events map to their corresponding standard
switch. Withdrawals remove all recipients' copies of that exact source event,
without removing unrelated personal notifications. Notifications stranded on
hosted actors before this feature are reassigned to their current owner while
preserving IDs/timestamps and avoiding fresh push deliveries; historical editor
eligibility is not inferred.
