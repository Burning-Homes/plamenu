# Audio and music

Open **Music** to find something to listen to. Use **Search music or paste a
link** for a song, artist or album already known to Plamenu. You can also paste a
music site's address, an album link or a song link. Plamenu opens supported
content here. **Live radio** and **Podcasts** are available from the same page. On Podcasts,
paste a podcast website or feed link; a website that advertises RSS or Atom is
resolved automatically. Subscription import and OPML stay in the podcast tools.

Choose **Play** beside a song or playlist to start listening in the player.
You do not need to subscribe, import a copy or save the song first. The player
opens in your current tab. Audio posts already have their own Play control;
**Music options** holds saving, music likes, update subscriptions and the optional
separate player. **Browse music while listening** opens a browsing window without
stopping the player. If your browser requires another gesture, press the native
audio Play control. **Track details and sharing** opens the current resource;
for published uploads, **Open post · comment and share** takes you to its social
post and the ordinary reply, boost and share actions.

**Save to My music** keeps a song or album for later. **My music** brings together
your uploads, saved music and music you like. Playlists, likes and listening
history are available there. Saving music from another site keeps a reference;
its owner can still remove it or change who can listen. Cached metadata is not a
guarantee that the audio is still available: Plamenu checks again when you play.

Choose **Upload audio**, select a file and optionally enter its title, artist and
album. **Upload audio** saves a private copy in My music. **Publish now** makes it
public and shares it as an audio post. The resulting page shows whether it is
published. You can listen, change its artwork, edit its details, choose who can
listen, and manage the publication there. **Publish audio** under **Advanced
music tools** also opens the ordinary post composer directly, with its description,
visibility, content warning and interaction controls.

A live broadcast is something you can listen to now. Station rules are settings
that choose songs for a broadcaster; they do not themselves guarantee an online
stream. Supported music sites expose playable broadcasts under **Live radio**.
Plamenu does not guess a connection between station rules and a stream.

## Advanced features

**Advanced music tools** preserves the specialist music sections. Queue additions
are under **Queue options**; output destinations, precise seek and saved players
are under **Advanced playback**. Playlist/library organisation and unavailable
saved references are under **Library tools**. Podcast subscription imports and
OPML are under **Import and manage podcast subscriptions**.

Search covers public stored audio, readable cached remote music and audio posts;
it does not silently search every music server. Opening a known site's catalogue
searches that site's public content. Songhive can expose songs in its anonymous
catalogue without publishing ActivityPub Audio posts. These remain references to
their original catalogue authority, and private, paid, deleted or offline media
cannot be unlocked by a subscription or an older cached reference.

An Audio publication keeps the ordinary post visibility model and editing rules.
Its description supports ordinary post formats; attachment descriptions can carry
transcripts. Editing cannot remove its required single audio attachment.
Publication scheduling is currently unavailable. The API accepts
`post_kind: "audio"`, `title`, one `media_ids` entry and optional `music` metadata
(`artist`, `album`, `genre`, `release_year`, `track_number`, `disc_number`,
`musicbrainz_id`). Omitted metadata on edit is preserved. The returned Status has
`object_type: "Audio"`; other statuses retain their Mastodon-compatible fields.

The post API's `private` means followers-only. Ordinary attachment URLs are
bearer URLs. Stored tracks additionally provide independent private storage and
revocable sharing. Payments and third-party provider integrations remain outside
the current scope; see [Songhive parity](../SONGHIVE_PARITY.md).

## Music subscriptions

The **Music subscriptions** section on Music accepts an ActivityPub library or
track URL. It displays pending approvals and lets you unsubscribe. Public and
unlisted Audio cards from other authors also offer **Subscribe to track updates**.
Music subscriptions are separate from account follows: they do not grant access
to follower-only posts. Blocks and instance moderation apply to music delivery.

Each ordinary local actor advertises a public `library` URL. Its ActivityPub
`Library` document lists only that actor's public Audio publications, with
paginated `CollectionPage` documents containing up to 50 Audio items. Unlisted,
local-only, follower-only, direct and deleted posts are excluded.

Clients use `GET /api/v1/music/follows` with `read:statuses` and
`POST /api/v1/music/follows` with `write:statuses`. The POST accepts
`{"operation":"follow","url":"https://music.example/users/artist/library"}`
or the same URL with `operation` set to `unfollow`. Remote subscriptions remain
pending until the resource's verified owning actor accepts the exact Follow.
Public named libraries also have a followable ActivityPub URL, available from **Federated library** on their page. The paginated document contains readable Audio publications; an original stored track appears only after explicit publication. Following a library does not follow its owner. Making the library private or deleting it withdraws its federated document and subscriptions. Artist and album pages provide reusable metadata and their currently readable tracks.

## Stored tracks

Uploaded audio keeps files independently of social posts. Upload one audio
file from **Music → Upload audio**. New tracks are creator-private.
Tags provide an initial title, artist, album, genre, year and track/disc order;
you can edit these fields. Importing the same decoded audio again reuses your
existing track, even when file tags differ. Another member's import has its own
identity and permissions.

Choose public, signed-in local members, or only you and members you explicitly
share with. Share and revoke controls use a local username. Collection membership
alone does not change access. Protected playback and original downloads
check current permissions on every request and support seeking with HTTP byte
ranges. An ordinary attachment URL cannot serve a stored track's internal key.

Edits require the revision displayed by the form; reload after a competing
edit. Deleting a stored track revokes its metadata and bytes immediately and
schedules durable file removal. Pending removal continues to count toward
storage until the worker acknowledges deletion. The operator can configure
`[music]` budgets with `per_account_mib`, `global_mib` and `min_free_mib`; defaults
are 50 GiB per member, 1 TiB overall and 1 GiB free disk space.

Named libraries and playlists contain ordered references to stored tracks or
Audio posts already present in your catalogue. Open **Add a track** and paste the track or post
link to add it, use Move up/down to order it, and remove entries without deleting
the original. Libraries deduplicate entries; playlists can repeat a track.
By default, sharing a collection grants access to its description and readable
entries. Open **Sharing settings** and enable **Also share my tracks** to let the collection's
readers listen to your tracks in it, including private tracks. A public
collection then shares those tracks with everyone. Tracks owned by other people
and ordinary Audio posts keep their own permissions.

Turning the option off, removing a track, or revoking collection access removes
that derived permission immediately. An independent direct grant or access
through another collection continues to work. Exports, artwork, playback and
statistics use the same current permissions. This option does not publish an
Audio post. Clients set `share_tracks` and the current `revision` with
`PUT /api/v1/music/collections/{id}/contents-sharing`, using `write:statuses`.

A public stored track can be published as an Audio post. Downloads and embeds
are available under **Details and tools**. Under **Details and tools**, **Withdraw Audio
publication** retains the stored audio; publishing again creates a fresh post
identity. Stored metadata edits update the active Audio publication. Making the
track local/private, or deleting it, retracts the active publication in the same
transaction. Its protected playback URL follows the track's current access
rules. Standalone Audio posts retain their ordinary attachment behavior
described above.

## Artists and albums

Stored tracks link to reusable artist and album pages. The catalogue searches
names and descriptions, and each page lists only tracks the visitor can read,
in album, disc and track order. Private tracks do not contribute to another
visitor's track count or pagination.

Owners can edit names, biographies or notes, MusicBrainz identifiers and tags.
Album editors also provide genres and release years. Changes propagate to the
stored tracks and their current Audio publications in one transaction. Explicit
track year overrides survive an album year change. Stale forms return a conflict
so an earlier page cannot overwrite a later edit.

Native clients can browse `/api/v1/music/artists` and `/api/v1/music/albums`,
read individual resources and their `/tracks` pages, and submit revisioned edits
with `PUT`. Reads require `read:statuses`; edits require `write:media`.

Music assets, artists, albums and collections support uploaded artwork. Owners
can change or remove artwork through ordinary forms; playlists and libraries
can keep an image and a separate cover. Uploads are decoded, resized to at most
1024 pixels and stored as JPEG or PNG. Artwork uses the current access to its
music, including explicit grants and blocks. Original media filenames do not
provide access to it.

Replacing or deleting artwork schedules durable physical cleanup. Account
retirement also removes artwork, and its storage remains charged until cleanup
is acknowledged. Changes to published music and its artwork federation updates
commit together.

Create an artist from **Artists → Create artist**, then create an album from
that artist's page. New albums start with private details; you can choose who sees them. Empty
artists become readable through their currently readable albums or tracks. Album deletion
can keep tracks and remove their album assignment. Artist deletion requires an
empty artist or the explicit recursive option, which removes the artist's
albums and tracks, withdraws publications and schedules audio/artwork cleanup.
Clients use `POST /api/v1/music/artists` or `/albums` to create resources and
revisioned `DELETE` on individual resources with `recursive: true` for the
cascade. Creation and deletion require `write:media`.

Artist, album, library and playlist pages show the full readable track count
and playable duration, including entries beyond the displayed page. Playlist
repeats count separately. Artist pages also count readable albums; their owner
can see empty albums. These totals follow current source permissions, so a
revoked share or deleted track immediately disappears from the totals.
Clients use `GET /api/v1/music/artists/{id}/stats`, `/albums/{id}/stats` or
`/collections/{id}/stats` with `read:statuses`. Responses are private and
uncacheable, with `track_count`, `total_duration` in seconds and, for artists,
`album_count`.

## Album access

Open **Manage album access** on your album page to choose private details,
signed-in local members, or public details, and grant or revoke access for
specific local members. Older inferred albums can retain **Follow readable tracks**:
their details are readable when the visitor can read one of their tracks.
These settings govern album notes and tags independently of listening access.
A public track still shows its credited album name and year and can inherit
its cover while the album's notes remain private.

**Also share my tracks** is a separate choice. It extends the album's access
to your tracks currently assigned to it. Revoking album access or turning off
that option removes derived listening access; independent track grants and
other shared collections still apply. In **Follow readable tracks** mode, only
explicit member grants extend access to private tracks. Sharing audio does not publish an Audio
post. Private album notes and tags are removed from existing public Audio
credits, and private canonical album resources withdraw their subscriptions.

Clients use revisioned `PUT /api/v1/music/albums/{id}/access` with
`visibility` (`private`, `local`, `public` or `source`) and `share_tracks`.
The owner can list paginated grants at `GET /api/v1/music/albums/{id}/grants`,
and `PUT` or `DELETE /api/v1/music/albums/{id}/grants/{member}` grants or revokes
a local account's access. Reads require `read:statuses`; writes require
`write:statuses`. Access and membership responses are private and uncacheable.

## Track notes, credits and album assignment

Track pages provide a separate editor for descriptions, tags and additional
artist names. Descriptions use the existing plain text, Markdown and sanitized
HTML formats. The description also supplies the published Audio caption:
editing that caption updates the retained description in the same transaction,
and metadata changes or republishing preserve its format. Additional artists
are credited names and do not create accounts. Search includes descriptions,
tags, genres, credits and the album artist.

**Choose album** opens a searchable, paginated list of your albums. Assigning
an existing compilation album keeps the track's performer, year and genre
separate from the album artist and its defaults. A compilation's album artist
is visible through albums or tracks the visitor can read. Removing an assignment
keeps the original audio and publication identity.

Native clients submit revisioned `PUT /api/v1/music/assets/{id}/details`
with `description`, optional `content_type`, `tags` and `extra_artists`, or
`PUT /api/v1/music/assets/{id}/album` with an owned `album_id` (null to detach).
Both require `write:media`; stale revisions fail with a conflict.

## Saved music player

**Open in music player** opens a dedicated player window from a track,
Audio publication or collection. Keep this window open while browsing Plamenu
in the main window. Add the displayed selection to its queue, then choose
**Play this track** or use the audio controls. Playback starts after an explicit
interaction. The player also works as an ordinary page.

The main player uses your default saved queue. Separate browser players and
named destinations can use their own queues. Entries can repeat the same track,
move up/down, or be removed without deleting the source. Shuffle uses a
stable order until it is disabled; disabling it restores the manually arranged
order. Repeat can stop at the end, repeat the current track after it ends, or
wrap through the whole queue. Manual Next still advances when repeat-one is on.
Queue, selection and seek position survive reload; the JavaScript enhancement
also saves position during playback and when pausing, and retains volume/mute
for this browser. Reload does not start sound automatically.

An open enhanced player keeps its audio element when its queue changes or when
a selection arrives from another Plamenu window. The native forms provide
previous/next, selection, repeat/shuffle, seek, ordering and removal without
JavaScript. **Saved players** lists your devices; **Forget this player** removes
that saved queue. A queue can hold 1000 entries and an account can retain 32
players.

Membership in a queue never grants music access. Each read resolves current
source permissions and metadata. A revoked or deleted selection disappears;
the enhanced player periodically refreshes access and stops an unavailable
selection. Retained audio bytes continue to check access for every request.
Queuing a remote Audio preserves its originating identity and does not publish
it under your account. Server output adapters remain separate work.

Native clients use `GET /api/v1/music/queues` to list devices and
`GET /api/v1/music/queues/{device}` to read a queue (`read:statuses`).
`POST` to a device endpoint requires `write:statuses`, the current string
`revision` and an `action`. An absent device reads as revision `"0"`; the
first successful write creates revision `"1"`. Stale writes return a conflict.

For example, add an existing stored track with
`{"revision":"0","action":"add","sources":[{"asset_id":"123"}]}`.
Use `status_id` instead for an already known Audio publication. Selection,
removal and ordering use the returned queue entry IDs, which remain distinct
when a source appears more than once. `settings` accepts `repeat_mode`
(`off`, `one`, `all`) and `shuffle`; `seek` takes `entry_id` and finite
non-negative `seconds`. `collection` takes `collection_id` and optional
`replace`; it imports only entries you can currently read. Revisioned `DELETE`
forgets a device. Queue responses are private and never stored in shared caches.

## Listening history

**Listening history** is private to your account. The enhanced music player
starts a session only when playback begins. It accumulates actual forward
playback time, ignores seeks, and sends progress during playback and on pause
or completion. Fetching an original, requesting byte ranges, loading metadata
or adding a queue entry never records a listen.

The default threshold is 30 seconds or half the track's duration, whichever
comes first. **Listening preferences** changes these thresholds for new
sessions. When duration is unknown, the seconds threshold applies. Each
session records at most one listen; replayed reports cannot create duplicates.
Automatic reports require JavaScript; ordinary audio controls and history
management remain usable without it.

Turning history off cancels pending sessions. **Erase listening history**
removes recorded listens and pending sessions. History entries resolve current
music permissions: revoked sharing, blocks, retired accounts and deleted
sources hide or remove their entries. Queuing or listening does not create a
social post. Dates use your existing timezone and locale preference.

Native clients use `/api/v1/music/listening/preferences` for revisioned settings,
`POST /api/v1/music/listening/sessions` to start and
`POST /api/v1/music/listening/sessions/{token}` for progress. Starts take a
source `asset_id` or `status_id`, or an owned `queue_device` and `queue_entry_id`,
plus a unique `request_key`. Retrying that key for the same source returns the
same session. Progress supplies finite, monotonic `played_seconds` and optional
`finish`; the service limits credited time to elapsed wall time and at most
15 seconds per report. Report regularly and exclude paused time and seeks.

`GET /api/v1/music/history` returns current readable entries, with `max_id`
pagination; `DELETE` erases all your sessions. Reads require `read:statuses`,
writes require `write:statuses`. These responses are private and never stored
in shared caches. Completed play reports from Subsonic apps use the same listening-history preference. External scrobbling services are outside this scope.

## Music favourites

**Save music favourite** keeps a track in your private **Music favourites**
page. A stored track or known Audio publication keeps its original identity;
a music save does not publish it or send a social Like. The ordinary favourite
button on a post still performs that post's social reaction.

Removing a save is idempotent. Revoked sharing, blocks and deleted sources hide
unavailable entries without retaining their titles, artwork or stream URLs.
Saving another local member's track produces a music notification governed by
the recipient's notification policy and separate push preference. Removing the
save or deleting its source retracts that notification. The notification uses
current authorized metadata rather than storing a title snapshot.

Native clients use `GET /api/v1/music/favourites` with optional `max_id`
pagination and `read:statuses`. `POST` saves and `DELETE` removes a source,
using JSON containing exactly one string `asset_id` or `status_id` and
`write:statuses`. Responses are private and never stored in shared caches.

## Listening statistics

**Listening statistics** shows your most played tracks, artists, albums and
genres, plays and genres over time, release years or decades, and the 24 local
hours when you listen. It counts recorded listening sessions, independently
of downloads, prefetches, pending sessions and replayed reports.

The page uses your profile timezone initially. Choose a date range, timezone,
day/week/month/year grouping, first day of the week, release grouping and
ranking limit with ordinary forms. Calendar buckets include periods with no
plays; daylight-saving transitions preserve the correct local date and hour.
The date inputs cover whole days in the selected timezone. Controls and tables
work without JavaScript.

Charts resolve current music access before counting. Revoked shares, blocks,
retired accounts and deleted sources remove those sources from every chart,
including totals and the listening clock. Erasing listening history clears the
charts. Identically named local and remote entities remain distinct. A retained
track and its own Audio publication combine only through their explicit
publication reference and independent access checks.

`GET /api/v1/music/stats` requires `read:statuses` and returns private,
non-cacheable `period`, `top`, `plays`, `genres_timeline`, `releases`, `clock`
and `play_count` fields. Ranked entries include an opaque `key` for identity;
web display URLs and names are not grouping identities. Query parameters are `from`, `to`, `tz`, `group_by`,
`week_start` (Monday `0` through Sunday `6`), `release_group` (`year` or
`decade`) and `limit` (`1`–`100`). Dates accept ISO calendar dates or exact
RFC 3339 timestamps. The default period is the previous 30 days; API timezone
is UTC unless specified. Unsupported timezones and invalid ranges are refused.
A range requiring over 10,000 calendar buckets needs a coarser grouping.

## Radio station configurations

**Radio stations** stores a station name, description and optional settings.
Create a station from the music navigation. New stations are private; choose
public visibility, signed-in local members, or explicitly grant a local member
access. Anonymous catalogue browsing follows the instance preview setting; a
direct station link still follows that station's own visibility. Station settings remain visible only to their owner, including when
the station is public or shared.

Owners can edit a station, revoke member access or delete it using ordinary
forms. Revision checks reject an edit made from an outdated page. Blocks,
moderation and account retirement apply to station views. Deletion removes the
station and its sharing permissions; account purge also removes station settings.

Native clients use `/api/v1/music/radios` to list or create stations and
`/api/v1/music/radios/{id}` to read, update or delete one. Listing accepts
`mine=true`, `q` (name/description search) and `max_id`; updates and deletion require a string `revision`.
Creation defaults to private visibility and accepts `name`, `description`,
`config` and `visibility`. Configuration is bounded opaque text.
`PUT` or `DELETE /api/v1/music/radios/{id}/grants/{member}` grants or revokes
access for a local account ID. Reads require `read:statuses`; writes require
`write:statuses`. Every response is private and avoids shared caching.

## My Music and saved references

Open **My Music** from the music page to see your own music, saved resources
and music favourites. Filter by tracks, artists, albums, libraries, playlists
or radio stations, and search their current metadata. Artists of your collected
tracks and albums appear automatically. The labels explain why an item appears.

Use **Save to My music** on another person's resource to keep a reference to it.
Saving does not grant listening access, copy the source or send a social Like.
Removing a saved reference preserves your music favourite and the creator's
original. Your own resources appear automatically.

If a creator revokes access, the resource disappears from playable My Music.
The **Saved references** page lets you remove unavailable references without
showing their former title or artwork. Source edits appear on the next read.
Deleting the original resource also removes its saved reference when the source
is erased from the database.

Clients can read `/api/v1/music/mine` with `kind` and `q` filters, and
`/api/v1/music/saved` with a `max_id` cursor. Save with
`POST /api/v1/music/saved` and a `{ "kind": "track", "id": "…" }` reference;
delete the returned saved row through `DELETE /api/v1/music/saved/{id}`.
Reads require `read:statuses`; mutations require `write:statuses`.

## Music and account deletion

Deleting your account removes your retained music, artist and album notes,
collections, radio settings, saved references, queues, listening history and music favourites.
It also removes your music subscriptions and grants to other people's music.
Their original tracks remain theirs. Your handle remains reserved. Music links
stop working immediately while file removal continues in the background.

Music-only subscribers receive the affected resource retractions and the
account deletion. They do not receive unrelated social posts. The browser
removes the deleted account from its chooser while preserving other signed-in
accounts. A temporary moderator suspension remains reversible during the grace
period.
If that period expires, scheduled erasure removes the same music data and
sends the same retractions as permanent account deletion. A failed delivery
keeps the account suspended and recoverable until the operation can succeed.


## Revocable share links

Choose **Create share link** on a track, artist, album, library, playlist or radio
that you own. Choose an expiry or keep the link active until you revoke it.
The result displays the URL once; copy it before leaving. **Sharing** lists your
links and member grants and lets you revoke them. New member grants generate
music sharing notifications; repeated grants do not generate duplicate notices.
Revoking a grant removes its associated notice.

A link shares that resource's details. Track links permit playback and original
downloads. Albums and collections share your private tracks only when **Also
share my tracks** is enabled, and only while those tracks remain in the resource.
Artist links do not grant all their private tracks. Tracks owned by another member
and ordinary restricted Audio posts retain their own permissions. Playback,
artwork and downloads check the current link and source permissions each time;
revoked, expired or retired-owner links stop working.

Clients manage links with `POST /api/v1/music/share-links` (`write:media`) and
`DELETE /api/v1/music/share-links/{id}`. Creation accepts
`{"reference":{"kind":"track","id":"…"},"expires_at":null}`; an expiry is an
RFC 3339 timestamp. `GET /api/v1/music/shares` (`read:statuses`) returns the
paginated overview without recoverable tokens. `/music/share?token=…` has both a
native preview and an explicit `Accept: application/json` preview, with scoped
playback/download/artwork URLs. Share URLs do not publish an Audio post or create
an ActivityPub follow.

## Podcasts

**Music → Podcasts** contains your RSS/Atom subscriptions. **Follow a podcast**
accepts a podcast website or feed link. Websites advertising an RSS/Atom feed
are resolved automatically; Plamenu makes at most one additional feed request.
Each show puts episodes and their **Play** buttons first. Open **Episode actions**
to select an episode for a bulk operation; **Podcast tools** contains ordering,
refresh and bulk controls.
Search subscriptions by title, author or description, and sort by latest episode,
episode count, unplayed count or name. Open an episode to listen to its original
source audio and mark it played or unplayed. **Play** starts an episode in the persistent
player; queue additions remain available under Details and tools; the collection selector adds an episode to one of your libraries or
playlists. Actual player reports mark it played when your listening threshold is
reached; seeking, metadata loading and prefetching do not. Played state is private to your
account and remains attached to the same episode when its title or enclosure
changes.

**Podcast tools → Refresh feed** checks for new metadata and episodes. Background refreshes run
for subscribed feeds. A failed refresh retains the last usable catalogue and
shows the error. **Manage subscriptions with OPML** exports your subscriptions
or imports a pasted document or OPML file, including nested folders. Import results list
all failed feeds as well as new and existing subscriptions. On the show page, open **Podcast tools**;
**Queue these episodes** opens an append/replace choice in the player. Episode
checkboxes let you mark selected episodes played/unplayed or add them to a
collection. Libraries deduplicate episode references; playlists preserve repeated
entries. Unfollowing a show
removes your subscription without deleting the shared catalogue.

Podcast routes use `/api/v1/music/podcasts`, its `/{id}`, `/{id}/episodes`,
`/{id}/refresh`, `/episodes/{id}`, `/episodes/{id}/played`, `/opml` and
`/opml/import` and `/opml/upload` subpaths. Reads require `read:statuses`; subscription changes,
refresh, OPML import and played-state changes require `write:statuses`. OPML
imports accept the XML document as the request body; `/opml/upload` accepts a
multipart `file`. `PUT /{id}/played` accepts `episode_ids` (up to 50 string IDs)
and `played` for an atomic change belonging to that show. Feed subscription is
independent of ActivityPub publishing. External podcast sync services are
excluded from the current implementation scope.


## Collection feeds

Library and playlist pages advertise **RSS feed** and **Atom feed** links.
Readers receive currently readable tracks and podcast episodes, with the newest
additions first. Repeated playlist entries have separate feed identities.
Reading a feed or its enclosure links does not record a listen.

Clients fetch `/music/feeds/collections/{id}.rss` or `.atom`, with an optional
`limit` from 1 to 100 (default 50). A browser session or bearer token can expose
its currently granted content; bearer tokens require `read:statuses`. Private
collections remain private. Conditional ETags are evaluated after current access
checks, including source revocation. A changed title or visibility changes the
representation. Existing entries first receive an addition timestamp when feed
support is installed, since earlier schemas did not retain that date.


Local profiles also advertise an RSS feed at
`/music/feeds/profiles/{username}.rss`; `.atom` uses the same feed representation.
Native public/private post permissions apply, including blocks and account
retirement. The existing `/@username.atom` endpoint remains available.

## Collection tags

Owners choose **Manage tags** on a library or playlist, enter comma-separated
tags, and save. Tags are normalized and deduplicated. Collection and My Music
searches use their current tags. Tag edits preserve track permissions and do not
publish an Audio post. An outdated edit requires reloading the collection.

Clients use `PUT /api/v1/music/collections/{id}/tags` to replace the tag set, or
`POST` to add tags. The JSON body contains string `revision` and `tags` (up to 50
strings, at most 100 characters each). Delete a tag with
`DELETE /api/v1/music/collections/{id}/tags/{tag}?revision=…`. Mutations require
`write:statuses` and collection ownership.


## Artist releases and genre/tag discovery

Artist pages advertise RSS and Atom release feeds at
`/music/feeds/artists/{id}.rss` and `.atom`. Readable albums represent their
tracks; independently readable tracks from hidden albums still appear.

Track, artist, album and collection tag labels open exact tag browse pages
inside Music. Genre labels open genre pages. These pages advertise both feed
formats at `/music/feeds/tags/{name}.rss` and
`/music/feeds/genres/{name}.rss` (or `.atom`). Tags merge music resources and
native tagged posts; genres merge tracks and albums. Current permissions apply
before the item limit.


Collection share previews also provide RSS and Atom links. A feed URL carrying
`token` grants the exact collection details and its explicitly shared owned
tracks. Foreign private tracks and unrelated siblings stay inaccessible. Disabling
**Also share my tracks**, removing an entry, revoking the link, expiry or owner
retirement takes effect before a conditional cached response. These feeds use
`private, no-store` and `no-referrer`; the token remains a capability, not a login.


## Public music embeds

Public track, artist, album and collection pages provide **Embed music**. Choose
an audio player (tracks), an HTML track list (collections with fewer than 250
readable entries), a Markdown link, a script widget, or an iframe, then copy the
code. The widget and iframe play current public sources and link back to Music.
Large collections show up to 250 readable entries and a link to the full page.
Playlist repeats remain separate entries.

Embed pages are anonymous even when the browser sends a session. Private member
grants and share-link tokens cannot authorize a widget. Each audio range and
artwork request checks current public access; making a source private or removing
it revokes the embed. The code page supports ordinary forms without JavaScript.
Script widgets resize from bounded geometry messages validated against the
frame's origin and source window. The existing `/api/oembed?url=…` provider also
accepts exact local music page URLs.


Track pages include **Playback formats** inside Music. Signed-in readers can
prepare MP3, Ogg Vorbis, FLAC, AAC or Opus copies and refresh their progress
without JavaScript. The instance and role limit the negotiated bitrate; Vorbis
also accounts for the source channels and sample rate. FLAC is lossless and
ignores the bitrate target. Track owners can cancel preparation or remove a
cached copy. Existing track access still applies to every playback request.

Client APIs use `POST /api/v1/music/assets/{id}/renditions?format=ogg&bitrate=192`
to request a copy, `GET /api/v1/music/assets/{id}/renditions/{job}` to check it,
and the returned `stream_url` for protected Range playback. Cancellation uses
`DELETE` on the job API and requires the track owner and `write:media`; requests
and checks require `read:statuses`. A cached copy above the reader's current
bitrate ceiling must be prepared again within that ceiling.

**Play now** starts a selected format when you press the native audio player.
The page does not prefetch audio. A cold copy streams while it is encoded and
becomes seekable through HTTP Range once the cache is ready. Closing the last
cold listener cancels its work; an explicitly requested preparation continues.
Clients can use `?format=mp3&bitrate=192` on the track stream URL, including
exact share stream URLs. `HEAD` describes a ready cache and does not start work.

## Music client apps

Open **Music → Music clients** to create a separate password for a Subsonic or
OpenSubsonic app. Use the instance address and your local username. Copy the
password when it is created; subsequent visits show its name, permissions and
expiry. Revoke a password here to disconnect that app. Read-only passwords can
browse and play accessible music. Management permission additionally allows
playlist edits, favourites and explicit listening reports.

Now-playing reports remain private unless you explicitly enable sharing when
creating a management password. Sharing respects each listener's current access
to the music. Completed play reports honour your listening-history preference;
stream and download requests do not record plays.

The compatibility API is `/rest/{method}.view`, with XML by default, JSON via
`f=json`, and form POST requests. Apps can use the generated password directly,
as `apiKey`, or with Subsonic salted authentication. Passwords minted through the
Music client management API also depend on their originating OAuth grant.

## Mounted local folders

Music → Local folders shows operator-assigned libraries, scan progress, and
source files. Enable watching to discover added, changed, or missing files. Files to include
controls subfolder scanning, audio extensions and exclusion patterns.
Originals stay in the mounted folder; indexing does not copy them into Plamenu
storage. Identical decoded audio shares a track identity within your account.
Removing a track through its ordinary editor removes its index and leaves the
original file in place. Later scans preserve that removal. Use Source files → Reindex this source file
to add it back without copying the original. Disconnect folder removes the
mounted library and its references, while preserving the files.

Source files has separate rename, tag writeback, and permanent deletion controls
when the operator has enabled those capabilities. Renames keep the parent folder
and refuse an occupied filename. Edit the native track details before writing
its tags to the source. Writeback remuxes the container and checks that decoded
audio is unchanged. Permanent source deletion requires the explicit checkbox;
it differs from removing a track from your library.

Mounted tracks use the normal player, download, playlist and publication flows.
Playback renditions and portable published MP3 copies use the normal storage
quota. The mounted original is excluded from storage debt and account cleanup.

Newly indexed and uploaded audio can extract embedded cover pictures into the
normal, private artwork system. Source tag writeback also writes your selected
track image for MP3, FLAC, OGG, Opus and M4A; inherited artist photographs are
not inserted into source files. The decoded audio hash is checked before the
original is replaced.

Watched source tags update automatically until you change the track details in
Plamenu. Your customized title, artist, album and numbering then stay in place
across later filesystem scans. Changing visibility alone keeps source-tag
synchronization enabled. Automatic updates use the existing publication edit
path so a published Audio keeps its identity and receives current metadata.

## Mentions and notification delivery

The **Mentions** page inside the Music workspace combines local, federated and
website mentions. Filter by source or choose only non-public mentions. Clearing
notifications does not clear this archive; current audience checks, retractions
and deletions still apply.

From Notifications, choose **Manage read state and notifications** to mark
individual items read or unread, delete items, or mark all read. Opening this
inbox does not mark its contents read. **Notification delivery** lets you choose
in-app, email and daily email digests separately for each type. Email requires
a confirmed address and the operator's SMTP configuration; email channels are
off until you enable them. Summaries link to your private email-notifications
inbox, so you can still review types whose in-app delivery you disabled.


Resource pages include an **Activities** link. Use it to browse discussion and
interactions for that exact artist, album, track, collection, radio or remote
resource. Filter by origin or activity type, refresh the page, or load older
entries. Ordinary post cards provide the existing reply, quote, boost and like
controls. An artist's or album's activity page does not combine activity from all
its tracks.


Mounted source changes recover automatically after a server interruption.
If recovery reports that a mount or file changed, restore the original mount and
files. Scans wait while that repair is pending. Disconnecting removes the index;
recovery preserves the operator's original file when cancelling an unfinished
change. Losing an original does not turn a cached playback file into an original
download: unavailable mounted sources remain unavailable to playback and download.
