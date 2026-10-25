# Video API details

These are Plamenu extensions, not the PeerTube REST API. See the
[client overview](index.md#native-video-experimental) for upload, channels,
live and playlist endpoints. IDs and authorization remain resource-specific.

## Viewer preferences and history


These are Plamenu contracts, not claims of PeerTube REST compatibility.
Bearer authentication is required; no endpoint accepts a target account ID.

| Endpoint | Scope | Contract |
| --- | --- | --- |
| `GET /api/v1/video_viewer_preferences` | `read:accounts` | `history_enabled`, `p2p_enabled`, `p2p_configured`, `history_max_entries`, `history_retention_days` |
| `PUT /api/v1/video_viewer_preferences` | `write:accounts` | At least one JSON boolean `history_enabled` or `p2p_enabled`; omitted choices are preserved; history false also deletes history |
| `GET /api/v1/video_history` | `read:statuses` | `{data, next}`; pass both `next.before` and `next.before_media` as query parameters |
| `PUT /api/v1/video_history/{media_id}` | `write:statuses` | Whole `position_seconds`, optional boolean `completed`, native attachment's `content_revision` (omitted means 0); returns `{saved}` (false when disabled or stale) |
| `DELETE /api/v1/video_history/{media_id}` | `write:statuses` | Forget this account's entry only; idempotent |
| `DELETE /api/v1/video_history` | `write:statuses` | Clear this account's history, without changing its preference |

Positions must be within the known duration and at most seven days. A completed
position must be within the final ten seconds when duration is known. History
entries include current Video/media IDs, title, position, completion, timestamp,
permalink and native resume URL. Metadata-only PeerTube updates retain attachment
identity and positions; signed Delete removes the entry from subsequent reads.
Ordinary status/media entities and federation never include private history.

`p2p_configured` distinguishes an explicit account choice from the default. It
becomes true when `p2p_enabled` is deliberately saved, not when history changes.
Clients should preserve a legacy local opt-out until an explicit account choice.

## Content replacement, reprocessing and trimming


`POST /api/v1/videos/{media_id}/edits` requires both `write:media` and
`write:statuses`, and current publication-management authority. Preparation
returns HTTP 202 with the private candidate ID, status URL and HTML review URL.
Use either body:

```json
{"operation":"reprocess","revision":0}
```

```json
{"operation":"trim","revision":0,"start_ms":1250,"end_ms":8000}
```

Cuts use integer milliseconds and must fit the currently published film.
`GET /api/v1/video_edits/{candidate_id}` requires `read:media` and candidate
ownership. It reports target ID/revision, operation and phase, without source
keys or raw URLs. `POST .../{candidate_id}/retry` requires both write scopes,
a failed private job and current source authority/revision. `DELETE
.../{candidate_id}` requires `write:media` and ownership; cancellation remains
possible after source authority is lost. Storage is refunded only on cleanup
acknowledgement. All these responses are private and non-cacheable.

After processing and reviewing the candidate, apply it with the same confirmed
content endpoint used for replacement below. Do not publish it as a new Video.

`GET /api/v1/videos/{media_id}/content` requires `read:media` and current
publication-management authority. Its private, non-cacheable response contains
the stable media/status IDs, numeric `revision` and up to 100 eligible private
candidate IDs/titles. Storage keys and raw sources are not returned.

`PUT /api/v1/videos/{media_id}/content` requires **both** `write:media` and
`write:statuses`, plus current publication-management authority. Example body:

```json
{"candidate_media_id":"12345","revision":0,"confirm":true}
```

Use the revision from the reviewed publication, not a newly fetched value that
silently replaces the user's decision. A successful response contains the
unchanged media ID and incremented revision; stale/cancelled/otherwise changed
candidates return a conflict. The HTML route also requires CSRF and both write
scopes, including when an OAuth token is supplied as a session cookie.

Native published attachment entities expose additive `content_revision`.
Send that value when [saving watch history](../members/video.md); omitted revision
means the initial revision zero and cannot write positions for an edited film.

## Delivery analytics


Use an ordinary OAuth bearer token, not an ambient browser cookie:

- `GET /api/v1/video_channels/{channel_id}/analytics`: `read:media`, current
  owner/editor; returns policy, operator availability, measurement label,
  at most 1,000 daily rows and `possibly_truncated`.
- `PUT` on the same endpoint: `write:media`, current owner; JSON
  `{"enabled":true,"retention_days":30}`.
- `DELETE` on the same endpoint: `write:media`, current owner; erase totals
  without changing collection or retention.

IDs and ISO dates are strings. Responses and refusals are `private, no-store`.
The HTML mutation additionally requires the existing session-derived CSRF
token, and HTML pages vary on Cookie. No analytics are exposed in ActivityPub.


## Accessible media fields


Client attachments expose `description`, `transcript`, `audio_described`,
`visuals_conveyed_in_audio`, `captions` (including same-origin `transcript_url`)
and `chapters`. A JSON `PUT /api/v1/statuses/{id}` accepts additive
`media_accessibility: [{id, transcript, decorative, audio_described,
visuals_conveyed_in_audio}]`. Each entry fully replaces the structured
alternatives for that kept attachment; omission of the entire array keeps
them. `null`/empty transcript clears it. A maximum of 50,000 Unicode characters
and no NUL applies, and the two soundtrack claims cannot both be true. Ordinary
`media_attributes` continues to edit alt text/focus. IDs outside the validated
kept attachment set are ignored, never used to modify another post.

Canonical native Video Create/dereference/history/Update includes schema.org
`transcript` and explicitly namespaced `visualDescription`, `audioDescribed`
and `visualsConveyedInAudio` extensions. These do not change PeerTube's supported
description, caption or chapter vocabulary. PeerTube 8.3 accepts and ignores
unsupported alternatives; it does **not** preserve these extensions on a
PeerTube re-export, and we do not claim that it does. Compatible Video peers
can retain them across updates. Incoming alternatives apply only to the
canonical primary media, with plain-text rendering and bounded values; no
remote transcript URL is automatically fetched.

Deleted/restricted media does not gain public caption/transcript access.
Captions/chapters continue to use guarded same-origin proxy routes and author
authority; replacing captions versions their public links, and removing a
track revokes its old caption/transcript routes.
