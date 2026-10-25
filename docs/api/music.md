# Music API

Music endpoints are native Plamenu extensions. Use OAuth bearer authentication
and the scope required by each operation. IDs are strings. Many updates require
the current `revision`; on conflict, refetch and ask the user to review their
edit instead of silently applying it to a newer revision. Responses resolve
current access; saved references, cached metadata and queue entries never grant
access by themselves.

| Area | Endpoint family | Scope / behavior |
| --- | --- | --- |
| Stored tracks | `/api/v1/music/assets` | `read:statuses` for catalogue reads; `write:media` for owned metadata/media changes |
| Artists and albums | `/api/v1/music/artists`, `/albums` | Readable resources and `/tracks`; creation/edit/deletion uses `write:media` |
| Track notes and album assignment | `/api/v1/music/assets/{id}/details`, `/album` | Revisioned `PUT`, `write:media`; null album detaches |
| Libraries/playlists | `/api/v1/music/collections` | `read:statuses` / `write:statuses`; ordered references, repeat policy depends on kind |
| Contents sharing | `/api/v1/music/collections/{id}/contents-sharing` | Revisioned `PUT` with `share_tracks`; grants apply only to owned tracks |
| Music subscriptions | `/api/v1/music/follows` | `read:statuses` / `write:statuses`; POST `operation: follow/unfollow` and `url` |
| Saved resources | `/api/v1/music/mine`, `/saved` | `read:statuses`; POST `{kind,id}` or delete a saved row with `write:statuses` |
| Music favourites | `/api/v1/music/favourites` | `read:statuses`; POST/DELETE one `asset_id` or `status_id` with `write:statuses` |
| Queues | `/api/v1/music/queues`, `/{device}` | `read:statuses`; revisioned commands require `write:statuses` |
| Listening | `/api/v1/music/listening/preferences`, `/sessions` | Reads `read:statuses`, changes/reports `write:statuses` |
| History/statistics | `/api/v1/music/history`, `/stats` | `read:statuses`; history erasure `write:statuses` |
| Radio settings | `/api/v1/music/radios` | `read:statuses` / `write:statuses`; not a running audio output |
| Podcasts | `/api/v1/music/podcasts` | `read:statuses`; subscribe/refresh/played-state/OPML import `write:statuses` |
| Server audio outputs | `/api/v1/music/outputs` | `read:statuses` / `write:statuses`; owned stream commands and listener-token rotation |
| Download jobs | `/api/v1/music/downloads` | `read:statuses` / `write:statuses`; background archives, per-item results, cancellation and expiry |
| Share links | `/api/v1/music/share-links`, `/shares` | Create/revoke `write:media`; overview `read:statuses`; tokens shown once |
| Renditions | `/api/v1/music/assets/{id}/renditions` | Request/status `read:statuses`; owned cancellation `write:media` |

This is an entry-point map, not a complete versioned schema. Consult the matching
release's route definitions in `crates/server/src/routes/music*.rs` for all
request fields, streams, exports and folder operations; verify the actual error
and permission behavior when integrating.

## Audio posts and access

The status API accepts `post_kind: "audio"`, a `title`, one `media_ids` entry,
and optional `music` metadata (artist, album, genre, release_year, track_number,
disc_number, musicbrainz_id). The Status has `object_type: "Audio"`. Omitted
music metadata on edit is preserved. A required audio attachment cannot be
removed; scheduling is unavailable.

Post visibility `private` means followers-only. It is different from creator-
private stored tracks. Stored assets enforce access on playback/downloads;
ordinary attachment URLs retain ordinary bearer-URL behavior. Collection
contents sharing and share tokens do not authorize foreign restricted sources.

## Playback state and listening reports

A missing queue device reads as revision `"0"`; its first write creates `"1"`.
Commands include adding sources, selection, ordering, removal, settings, seek
and loading a collection. Queue entry IDs distinguish repeated sources. Use
`asset_id` or `status_id` for a source; use entry IDs for queue changes.

Start listening only when playback begins, with a unique `request_key` and a
source or owned queue selection. Reusing the same key/source is idempotent.
Progress reports contain finite monotonic `played_seconds` and optional `finish`;
exclude paused time and seeks. Credit is bounded by wall time and 15 seconds
per report. Fetching bytes or a HEAD request is not a listen. History and stats
are private, respect user preferences and recheck current resource access.

## Feeds, embeds and external music clients

Resource pages advertise the current RSS/Atom and embed URLs. Conditional feed
responses still require current access. Embed endpoints are anonymous: cookies
and private share tokens cannot upgrade public-widget permissions. Capability-
bearing private feeds must not enter shared caches or logs.

Subsonic/OpenSubsonic uses `/rest/{method}.view`, XML by default and JSON with
`f=json`, including form POST. Create separate credentials through **Music
clients**; do not collect the member's account password. Supported credentials
include direct password, `apiKey` and salted Subsonic authentication. Management
permission, listening preferences and explicit now-playing consent still apply.
