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

This is a one-time copy. Automatic source synchronization, source replacement
and trimming are separate workflows, not implied by importing a URL.

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
