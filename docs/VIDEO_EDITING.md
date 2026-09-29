# Editing a published native film

From **Videos → Your published videos**, choose **Edit the published film**.
The same link is in the film's metadata manager. This workflow is for recorded
films published through the durable native source pipeline, not an active live
session or an older short-attachment upload without a retained native source.

## Reprocess or trim the retained source

Choose **Prepare reprocessed video** to encode the retained source with current
server settings, without changing the footage. This forces an encode rather
than silently remuxing an already compatible file. A previously trimmed film
keeps its existing source window; reprocessing cannot resurrect removed footage.

Open **Trim the published film** and enter start/end times in seconds, to three
decimal places. The interval is relative to the *currently published* film,
including any previous trim. The encoder decodes/re-encodes the selected frames;
it does not expose an earlier keyframe through stream copying. Captions are
intersected/rebased, including supported positioning and inline timestamps.
Malformed timing that cannot safely be transformed is rejected. Chapters retain
the starting section at zero and rebase later starts without rounding into
preceding footage. Check clipped cue text and every language before applying.

Untimed visual descriptions, transcripts and audio-description declarations
are cleared in a trim candidate, because they cannot be automatically cut to
match selected footage. Reprocessing preserves them. Review/edit the candidate's
alternatives using **Save alternatives and review edit**, as well as its
captions, chapters and regenerated poster. This private review does not offer
Publish/Schedule buttons: the next action applies the candidate to the original.

Preparation only allocates a private, independently charged source copy and a
durable queue job. Existing quota, disk admission, lease recovery, cancellation
and bounded native processing apply. The current publication remains playable
until you explicitly confirm application. Source copying rechecks current
authority and the source revision before opening, during renewal and before
making the candidate processable. An edit prepared from an older revision (or
another film) cannot be applied using a freshly fetched revision token.

Reprocessing carries current viewing positions into the new content revision.
Trims/replacements hide old positions. In all cases an old player's queued save
fails its revision fence. Post/social identities and metadata remain unchanged.

## Replace with a different source

1. Upload or import the replacement into the **same channel**. Keep it private:
   do not choose Publish video or schedule it as a separate post.
2. Wait for processing, then review the private draft. Check the film, visual
   description, transcript, audio-description intent, every caption language,
   chapters and poster. They describe the replacement, not the old footage.
3. Return to the published film's replacement page, choose the ready draft and
   submit **Review your video**. Its preview, captions and text alternatives
   remain private to the uploader.
4. Review the stated effects, confirm the checkbox and choose **Apply the
   reviewed replacement**. The draft is consumed by this operation.

All steps use ordinary HTML forms and native controls without JavaScript.
Scripts improve upload progress, bounded resumable chunks, processing status
and poster selection. No browser-side encoder or companion service is required.

The existing film remains playable throughout upload, processing and review.
Application atomically adopts the completed film, poster, descriptions,
transcript, captions and chapters into the **original media ID**. The status
ID/address, Video UUID, immutable author, channel, replies, reactions, shares,
bookmarks and playlist references are not replaced. The published title,
description, language, audience, category, licence and content warning retain
their current values; edit them using their existing managers if necessary.
Caption languages belong to the replacement even if the post's language differs.

A failed encoder, cancelled/expired/scheduled draft, revoked editor,
suspension, quota refusal, stale review or failed Update transaction leaves the
existing film unchanged. Concurrent replacements are revision-fenced: only one
can apply against the reviewed version. A conflicting HTML submission offers a
fresh review rather than automatically applying over somebody else's film.

## Identity, storage and playback

Owners/editors may manage channel publications; publishers may manage only
their own. The candidate must belong to the requester and the same channel.
Application rechecks current authority under the channel/media fences, not just
when the form opens. Collaborators do not become the original author,
federation signer or storage debtor. Transferred candidate charges are checked
against the original uploader's quota, including superseded bytes awaiting GC.

Processing happens privately in the existing fair native worker queue. The
application transaction performs no encoding, file copying or origin fetches.
New keys are immutable; old output keys are retired and the superseded raw
source is queued for cleanup. Neither deletion of a draft nor a pointer swap
refunds storage: the cleanup backend must acknowledge each deleted file.
Raw sources and private previews do not become public capabilities.
The already-reserved compatibility JPEG is prepared before adoption, while
first-party clients retain the efficient, natural-aspect-ratio AVIF preview.
This keeps the signed Update acceptable to peers that reject AVIF-only icons.

An original-author `Update(Video)` is queued in the same transaction. PeerTube
uses the stable UUID for its peer swarm; a replacement does not invent a new
social or peer identity. Revised master/checksum URLs and immutable segment
keys distinguish the new content, and segment integrity validation remains
mandatory before accepting peer bytes. A request pinned to a superseded content
revision does not receive the new playlist/checksum document. Existing viewers
may still have the old film buffered; replacing a post cannot erase bytes
already delivered to somebody else's device.

Private watch positions for a different film are no longer offered. An old
player's queued save is rejected by the content-revision fence instead of being
applied to the replacement. Metadata-only edits do not reset viewing positions.

## Client API

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
Send that value when [saving watch history](VIDEO_VIEWING.md); omitted revision
means the initial revision zero and cannot write positions for an edited film.
