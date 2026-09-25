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

## Native video (experimental)

These are Plamenu APIs, not a PeerTube REST API implementation. ActivityPub
video interoperability is tracked separately in the [AP09 inventory](../AP09-FEATURE-INVENTORY.md).

- `GET /api/v1/video_channels` (`read:accounts`): list your owned channels.
- `POST /api/v1/video_channels` (`write:accounts`): create a channel with
  `username` and `display_name`. Responses include string `id`, `username`,
  `display_name`, actor `uri`, and `owner_id`.
- Upload video through the shared media endpoint, then
  `POST /api/v1/videos` (`write:statuses`) with `channel_id`, `media_id`, `title`,
  optional `description`, `language`, and `visibility` (`public` or `unlisted`).
  The channel and unattached, processed video must belong to the authenticated
  creator. Publication returns the ordinary Status entity with a playable
  video attachment. Titles have 3–120 characters; descriptions have a separate
  10,000-character budget, also bounded after HTML serialization.

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
