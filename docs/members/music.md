# Music

Open **Music** to search for songs, artists and albums known to your server,
or paste a supported music-site, album or track link. Use **Play** to listen;
saving or subscribing is optional. Remote availability and access can change.
**Live radio** and **Podcasts** are available from the same workspace.

## Upload and publish

Choose **Upload audio**, select a file and check its title, artist and album.
**Save privately** keeps it in **My music**. **Publish now** also makes it public
and shares an Audio post. Open the resulting track to edit metadata, artwork,
access and publication. Publishing directly through the composer adds ordinary
post description, warning and interaction controls; Audio posts cannot currently
be scheduled.

Uploaded tracks are stored independently of posts. **Withdraw Audio publication**
keeps the stored audio; publishing again creates a new post. Making a stored
track private/local or deleting it retracts its active publication. Ordinary
Audio posts use ordinary post visibility and attachment URLs instead.

## Organize your library

**My music** combines your uploads, saved references and music favourites.
Saving remote music keeps a reference, not an owned copy or permanent access.
**Saved references** lets you remove references that have become unavailable.
A music favourite is distinct from favouriting the social post. Saving another
local member's track can notify its owner under their music notification policy.

Create artists and albums from **Artists**, or edit the metadata found in
uploaded tags. Track details support notes, additional artist credits, tags,
genre, year and track/disc order. **Choose album** assigns an existing album
without changing the recording. Artists, albums, tracks and collections can
have artwork. Reload a stale edit form rather than overwriting a newer edit.

Libraries and playlists hold ordered references to tracks, Audio posts and
podcast episodes. **Add a track** accepts its link. Libraries deduplicate entries;
playlists permit repeats. Move or remove an entry without deleting its source.
Use **Manage tags** to organize collections; tag and genre links open related
music. Album/artist deletion has separate options for retaining or removing
tracks: review the confirmation before a recursive deletion.

## Access and sharing

Stored music can be public, limited to signed-in local members, or private with
explicit member grants. A collection's visibility does not automatically expose
its tracks. Enable **Also share my tracks** only when you want the album or
collection's readers to access your contained tracks, including private ones.
Other people's tracks and restricted Audio posts retain their own permissions.

**Create share link** produces a revocable link, with optional expiry. Copy it
when shown; anyone holding it can use the granted access. Artist links share
the artist's details, not every private track. Albums and collections include
your private tracks only with **Also share my tracks** enabled. Use **Sharing**
to revoke links and member grants. Revocation cannot recall downloaded copies
or erase old operator backups.

Public resources provide **Embed music** and RSS/Atom feeds where supported.
Embeds use public access even if you are signed in; a private share link is not
permission to put that music in a public widget. Collection share previews offer
separate capability-bearing feed URLs: protect those like the share link itself.

## Player, queues and listening history

The player stays in the current tab. **Browse music while listening** opens a
browsing window; **Open in music player** offers a separate player. Use **Queue
options** to append/replace selections, and **Advanced playback** for saved
players, output destinations and precise seeking.

Queues support ordering, repeats, shuffle and saved position. Reloading does
not start sound automatically. Removing a queue entry does not delete music;
a queue never bypasses current sharing permissions. **Forget this player**
removes that saved queue.

**Listening preferences** controls private listening history. Actual playback
counts after the configured threshold; metadata fetches, seeks and downloads
do not. Automatic reports need JavaScript. **Listening statistics** shows your
recorded plays by track, artist, album, genre, date and local time. Erasing
history clears these statistics; turning it off cancels pending sessions.
Music listening history and video watch history are separate settings.

## Subscriptions, podcasts and feeds

**Music subscriptions** follows updates to a music library or track. It is
separate from following the owner's account and does not unlock private music.
A remote subscription can remain pending until accepted.

In **Podcasts**, **Follow a podcast** accepts a feed or a website advertising one.
Play episodes directly, add them to a queue/collection, or mark them played.
**Podcast tools** contains refresh and bulk controls. **Manage subscriptions with
OPML** imports/exports subscriptions; review failed-feed results. Unfollowing
removes your subscription without deleting the shared catalogue. Failed refreshes
retain the last usable information and show an error.

Library, playlist, artist-release, tag and genre pages offer feed links. A feed
reader still needs appropriate access to private resources. A feed request does
not count as a listen.

## Downloads, formats and client apps

**Playback formats** prepares MP3, Ogg Vorbis, FLAC, AAC or Opus copies within
server/role limits. Cold streaming may not be seekable until preparation finishes.
A conversion does not change ownership or access to the original.

**Download ZIP** prepares selected accessible audio in the background. Open
**Downloads** for progress, per-item failures, cancellation and the expiry time;
review missing items before treating an archive as complete. Downloading is not
a server backup.

In **Music clients**, create a dedicated password for a Subsonic/OpenSubsonic app.
Use your server address and local username. Read-only credentials browse/play;
management credentials also allow supported library changes and listening
reports. Copy the password once, set expiry as needed, and revoke it here.
Now-playing sharing requires explicit opt-in.

## Live radio and server streams

A saved **Radio station** stores station rules/settings; it does not by itself
start a broadcast. **Live radio → Create stream** creates a server-hosted output.
Choose its format, bitrate, stream address and public/private visibility, then
use **Manage stream queue** and **Play**. Playback continues when you close the
page; use **Pause**, **Stop** or **Disable** to stop it.

A private listener link is shown once. **Replace listener token** invalidates
the old link and disconnects existing listeners. A public stream rebroadcasts the
queued audio to its listeners, using the owner's source access. Choose a private
stream and protect its listener link when the output should not be public.
Broadcast only material you are entitled to share.

## Mounted folders

**Local folders** shows libraries assigned by the operator. Scanning indexes
originals in place; **Files to include** controls recursion, extensions and
exclusions. Watching detects changes. Removing a track from Plamenu leaves its
source file; **Reindex this source file** can restore its index. Disconnecting a
folder also preserves originals.

Rename, tag writeback and permanent source deletion are separate controls,
available only when the operator permits them. Edit track details before tag
writeback; explicit deletion removes the actual file. Customized metadata is
preserved across scans. If recovery reports a changed mount or file, ask the
operator to restore it; cached playback files are not substitutes for originals.

See [account safety](account-and-safety.md) for account deletion and exports,
[media operations](../operators/media.md) for quotas and mounted storage, and
[the music API](../api/music.md) for integration.
