# Importing a video into a private draft

Open Videos → Import a video. Choose a channel you can publish in, a title,
source format and public source URL. Confirm that you have permission to copy
and publish the material. This confirmation does not grant you a licence;
check the creator's actual terms and retain any required credit yourself.

Direct video file downloads support public HTTPS file URLs. Public ActivityPub
Video imports also accept PeerTube's public recorded Video/watch URL. The
importer chooses the highest advertised finite MP4/WebM file, including a
fragmented MP4 advertised inside an HLS Link. It does not crawl websites, run
scripts, download torrents, concatenate playlists or import live streams.
Credentials, URL fragments and non-HTTPS clearnet URLs are refused. The existing
configured hidden-service transport remains available to authorized operators.

## Review, accessibility and publication

An import is a private native draft, not a public post or a remote playback
cache. The source bytes pass through the same native container/codec validation,
bounded processing, HLS ladder, poster and storyboard pipeline as uploads.
Opening the import form or draft never publishes it or starts another import.

Provide a visual description and transcript on import where possible. At ready
review, check playback and add captions in each language, chapters, an appropriate
poster and its description. Check category, licence, credit and audience before
choosing Publish or Schedule. The result has a new Plamenu Video identity and
your channel attribution; it does not impersonate the source creator or inherit
their comments, followers, social identifiers or view counters.

The ordinary HTML form and Refresh status link work without JavaScript.
Enhanced draft pages poll only while visible, announce phase changes once and
stop when ready, failed, expired or unavailable. Native private previews work
with scripts disabled. Originals are never HTTP-readable, even by their owner.

This form makes a one-time copy. Scheduled sources are configured separately
below. Published-video replacement and trimming are separate, reviewed
[editing workflows](VIDEO_EDITING.md), not implied by importing a URL.

## Scheduled channel and playlist sources

Open Videos → Scheduled video sources. Select your destination channel, enter
a public ActivityPub channel actor or Playlist URL, choose a check interval and
explicitly authorize periodic retrieval. PeerTube public channels and playlists
are supported. The server performs discovery and copying itself; no external
downloader, task scheduler or companion service is required. Arbitrary website
scraping, authenticated/private sources and live-stream imports are not supported.

Each newly discovered finite Video becomes a private native draft. Nothing is
published automatically. Review the complete video (playlist clip boundaries
do not trim the source), alternatives, captions, credit and actual licence terms
before publishing. Imported category/licence labels and warnings are review
suggestions, not permission to reproduce the material. A reuse licence without a
verified versioned URI stays unselected rather than inventing a new reuse grant.
New publication uses
your own channel and a new Video identity, not the remote creator's identity.

Pause discovery stops further allocations; copies already queued remain private
and continue through the ordinary import/review pipeline. Cancel those drafts
separately if you also want to stop downloading them. Resume and Check now use
the same bounded worker. Manual checks are limited to one per minute. Removing
a source requires confirmation, keeps its copies and forgets deduplication
history; adding that source again can create new copies of old videos.

The ordinary HTML forms, status and Refresh source status link work without
JavaScript. Enhanced pages refresh pending checks only while visible, announce
changed statuses and expose a manual refresh when a status request fails.
Open Review prepared videos to inspect newly created private drafts.

Up to five sources are allowed per account, with intervals from fifteen minutes
to one week. Each check traverses at most four collection documents and twenty
candidate entries, with an eight-MiB metadata budget, forty-eight request ceiling
and forty-five-second network deadline. Durable page-and-entry cursors advance
large playlists over later checks, including PeerTube's PlaylistElement IRIs.
Source URLs and results are private. Source and referenced video servers see
requests from this instance; never supply credentials or private links.

Copies reserve the full configured source ceiling and obey normal draft/storage
quotas. A quota refusal retains its retry position for a later check; existing
copies are not removed. Deduplication survives deleting a draft or published
copy. At 10,000 remembered Video identities a source pauses rather than silently
forgetting history or growing without limit. Channel access loss and suspension
fence discovery and allocation. Restart reclaims an expired lease; generation
checks reject superseded workers. Backups include private source configuration,
cursors and deduplication history along with the ordinary import jobs.

## Failure, cancellation and privacy

The source server sees a request from this instance. A pending or failed source
URL is private database data; do not paste credentials or private links into it.
The URL is discarded after successful download. No source URL is added to the
published Video automatically. Drafts retain the existing seven-day expiry.

Network failure or an oversized/empty source leaves a retryable private import;
nothing is published. Retry downloading deliberately, or confirm cancellation
and start over if the URL is wrong. Once downloading succeeds, a processing
failure retains the original and Retry processing does not download it again.
Revoked channel access or account/channel/current-owner suspension fences source
work and the final commit. Cancellation stops a pending download within its
five-second lease check, removes the queue entry and enqueues cleanup. Quota is
not refunded before deletion is acknowledged.

The full configured source ceiling is reserved before a URL import, because the
small HTML form's length is not the remote movie's size. Downloads stream to a
unique private temporary file, with no whole-movie RAM buffer. The existing
transport checks HTTPS, DNS and every redirect hop against SSRF, rejects an
advertised or streamed length over the reservation, and stops a body stalled for
30 seconds. The import controller also has a one-hour wall-clock ceiling. One
in-process source download runs at a time; ready sources enter the existing fair
native processing queue. No new installation step or companion service is needed.

An unclean restart reclaims an expired two-minute download lease into a fresh
temporary file; it never appends uncertain origin bytes. Crash scratch is swept
on native server startup. A generation fence rejects stale workers, and a short
owned rename/commit window serializes cancellation with source storage. Back up
private draft/import data using the [consistent native recovery procedure](operators/backup-and-restore.md).
