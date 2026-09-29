# Native Video publishing and playback

Open `/videos`, choose a channel, and upload a video or audio source. Upload,
processing, review and publication are separate steps: a failed validation or
encoder does not discard the original. The ordinary HTML multipart form works
without JavaScript. With scripts, the same publisher adds bounded resumable
chunks, progress, cancellation and same-file resume after interruption.

The default source ceiling is 2 GiB. A ready draft is private until explicitly
published or scheduled. Originals are never served publicly, even after
publication. Retry preserves the draft identity and original; cancellation
requires confirmation. Storage is refunded only when deletion is acknowledged.
The [operator configuration](operators/configuration.md#native-video-storage-and-processing)
describes quotas and temporary-space reservations.

## Draft recovery and scheduled publication

Your Videos page lists private drafts separately from publications. Open a
draft for its current processing state and expiry, then **Review your video**
when ready. **Refresh status** works without scripts; enhanced pages update
status while visible. A failed encoder retains its original for **Retry
processing**. **Cancel and delete draft** requires the explicit confirmation.
Unpublished drafts expire seven days after creation; retry does not extend this.

For interrupted resumable uploads, return to Videos in the same enhanced
browser and select the same file to resume its acknowledged offset. A different
file is refused. Without JavaScript, cancel the interrupted private draft and
start a new ordinary multipart upload. No preview or draft URL grants public
access to that source.

On review, either publish now or enter **Publication time** and choose
**Schedule video**. Use the time zone printed by the form and a time at least
five minutes ahead. **Settings → Scheduled posts** lets you reschedule or cancel;
the scheduled film remains reserved and unpublished until the native worker
commits publication. Failure keeps it for retry rather than producing duplicate
posts. Cancellation releases the reservation; the original draft's expiry still
applies. Scheduling needs no browser left open or external scheduler.

Videos also offers [Import a video](VIDEO_IMPORTS.md): a guarded, one-time copy
of a public file or recorded ActivityPub/PeerTube Video into the same private
review pipeline. It never automatically publishes or synchronizes the source.
For periodic private copies, use Videos → Scheduled video sources and review
each imported draft separately; see the same import guide.

For a recorded native-pipeline film, [replace, reprocess or trim its published footage](VIDEO_EDITING.md)
with a separately processed private draft. Explicit review/application preserves
the original post, media ID, author, UUID and social references; the old film
remains available until application commits. Reprocessing preserves viewing
positions; trimming retimes captions/chapters and requires reviewing fresh
accessible alternatives for the selected footage.

## Downloads and embeds

Open **Downloads and embeds** from a ready draft's review before editing its
final publication fields, or from your published Videos list/post editing
links. The ordinary checkbox form works without scripts. Download offers and
first-party framing default to enabled; you can change either independently.
Choices survive scheduling and media replacement. Reusable live recordings
snapshot the broadcast's choices when prepared and keep that snapshot on retry;
each published recording can subsequently be edited separately.

These controls are not DRM. Disabling downloads removes the explicit MP4
download offer, not ordinary playback or the ability to save public bytes.
Disabling first-party embedding returns 404 from its frame route, even for
signed-in viewers. Saved copies, previously buffered bytes and third-party
players cannot be recalled. Captions and accessible alternatives remain available.
PeerTube receives the standard `downloadEnabled` field on the initial signed
Create and subsequent Updates; first-party embed policy is not presented as a
PeerTube-enforced federation field.

The client API offers `GET /api/v1/videos/{media_id}/distribution` with
`read:media`, and `PUT` with two required JSON booleans `download_enabled` and
`embed_enabled`. Writes require `write:media`; published edits additionally
require `write:statuses`. Current channel roles, active accounts, private draft
ownership and CSRF (HTML) remain required. A previously private draft becoming
published cannot upgrade a media-only token's permission. Cookie-backed OAuth
tokens follow these same granular scopes on upload/recovery, caption, chapter,
poster and published-metadata forms.

## Live announcements and recordings

Choose Videos → Create live broadcast. Category, licence, content warning and
sensitive-content choice are available before creating the waiting Video,
alongside channel, title, description, audience, DVR/latency and planned starts.
A warning implies sensitive content. Unspecified licence never invents reuse
permission. A validation error returns the filled-in HTML form for correction;
the ordinary workflow does not require scripts.

The one-shot recording retains its publication and labels. A reusable broadcast
creates separately identified recordings per session; category/licence are
snapshotted when an episode is prepared, and its content warning is preserved
on retry even if the parent is later cleared. A newer parent warning at
publication takes precedence. Edit each published recording separately if its
terms or warning should change. Browser camera/screen controls remain enhanced
options; RTMP/RTMPS/HTTPS-FLV encoder routes remain available without scripts.

Planned live starts are announcements, not automatic capture or encoder starts.
Their management page and waiting player offer a calendar subscription URL and
**Add with a 15-minute reminder**. Subscribe in your calendar app for later
schedule changes: opening a link once may only import a snapshot. The calendar
app delivers the reminder, not a Plamenu notification worker. An unlisted
broadcast's dates require its direct link; the channel calendar includes public
broadcasts only. No-JS exposes these same selectable URLs and links.

Live creation requires both `write:media` and `write:statuses`. Cookie transport
does not elevate a scoped OAuth token. Credential rotation and browser upload
require `write:media`; schedule/playback announcement changes require
`write:statuses`, in addition to current channel authority and CSRF/origin checks.

## Posters and navigation

Choose a poster frame using a numeric time without scripts or the visual
picker with scripts. On review, upload a still image and describe its important
content, including text. For a published film, choose Edit → Replace poster,
or its Replace poster link in your Videos list. Authorized channel editors can also do this. These forms work
without JavaScript; scripts add an immediate local preview.

Every poster revision has new immutable AVIF/JPEG URLs. The change and signed
federation Update commit together. Failure leaves the previous poster intact;
old files remain charged until acknowledged cleanup. Video URLs, captions,
chapters, social identity and seek previews do not change with the poster.
The original uploader remains the storage debtor and original author remains
the federation signer; collaboration does not transfer either identity.

Native processed films have at most 100 approximate keyframe seek previews in
one bounded JPEG sprite. Open Seek previews and choose a timestamp. Without
scripts, its media-fragment link opens the native video at that time. With
scripts, it seeks the existing inline player and moves focus there. Timestamp
labels remain usable with keyboard and forced colours. Tiles are decorative
navigation aids: they are not descriptions of the video's important visuals.
Use the [visual description, captions and transcript](VIDEO_ACCESSIBILITY.md).

PeerTube-compatible `Video.preview` advertises the storyboard using the
`storyboard` relation and tile width/height/duration vocabulary. PeerTube can
import the sprite and display it in its own seek controls. The client attachment
extension `storyboard` contains `url`, `tile_width`, `tile_height`, `columns`,
`rows`, `tile_count` and `interval_seconds`. This is not a promise of PeerTube's
complete REST API or plugin compatibility.

## Quality and sound

Compatible H.264 video is copied/remuxed for the highest HLS quality rather
than redundantly encoded. Useful lower qualities are selected from 1080p,
720p, 480p, 360p and 240p, never upscaled or nearly identical to the source.
They are encoded sequentially within a bounded worker slot. A separate MP4
remains playable in attachment-only clients and without JavaScript/HLS.
Script-enabled MSE players offer Auto and available quality choices; native
HLS engines retain their own supported controls.

An audio-only Video source becomes a low-frame-rate waveform video with AAC,
while ordinary audio attachments remain audio. Silent films remain silent;
portrait and unusually wide films retain their aspect ratio. No soundtrack
statement generates audio description automatically.

Peer-assisted HLS delivery is on by default when supported, with an accessible
pre-play opt-out. Participating viewers can learn one another's network
addresses. Opting out uses HTTP delivery and does not disable captions,
transcripts, quality choices or native playback. No-JS MP4 playback does not
participate in browser P2P.
