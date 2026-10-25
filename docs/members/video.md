# Watch and publish video

**Videos** offers Latest, Followed channels, search, playlists and viewing
settings. Following a channel subscribes to that channel, separately from its
owner's personal posts. Anonymous browsing depends on server policy.

## Watch privately

Start playback explicitly. Use captions, chapters, written alternatives and
quality controls where available. A standard MP4 fallback supports browsers
without JavaScript; older or remote media may offer different formats.

Eligible players use peer-assisted delivery by default. The tracker and other
participants can learn your network address. Before playback, open **… → Video
settings** and turn off **Share video with other viewers** for server-only
delivery. Save the choice in **Video viewing settings** to use it across your
signed-in browsers. No-JavaScript playback is server-only. Public embed URLs can
use `?p2p=0`; a creator's embed does not override your explicit opt-out.

Watch history is **off by default** and separate from delivery analytics.
Enable **Save my watch history** for private resume positions. JavaScript saves
progress while watching; without it, use **Saved playback position**. Resume is
explicit, not automatic playback. History retains up to 1,000 positions for
90 days. Forget one video, clear history, or disable it to erase all positions.
Replacement/trim invalidates positions for changed footage; metadata edits do not.

## Publish a recorded film

1. Create or accept access to a [channel](channels.md).
2. In **Video studio → Upload a video**, select it and choose your source file.
   The default source limit is 2 GiB; your operator may set another limit.
3. Wait for processing. The draft and preview are private. A failed encode
   retains the source for **Retry processing**.
4. Choose **Review your video**. Check playback, title, description, audience,
   language, warning, category, licence, poster, captions and chapters. Add
   [accessible alternatives](accessibility.md).
5. Publish now, or choose a publication time at least five minutes ahead in the
   displayed time zone. Manage reservations in **Settings → Scheduled posts**.

Native Video publication supports public/unlisted audiences; it is not a private
video-sharing service. Raw uploaded sources are never offered as public downloads. Playback and explicit MP4 offers use processed outputs.
Unpublished drafts expire seven days after creation; retry does not extend that.

With JavaScript, uploads can resume by selecting the same file in the same
browser. Without scripts, cancel an interrupted draft and upload again. Use
**Drafts** to find processing failures or cancel unwanted work. Storage may
remain charged until background deletion finishes.

## Import instead of uploading

**Import a video** copies a public HTTPS file or supported recorded
ActivityPub/PeerTube Video into a private draft. Confirm your right to copy it,
then review and publish as above. This creates a new Video under your channel;
it does not inherit the original's comments, subscribers or identity.
Arbitrary website scraping, credentials/private sources and live imports are
unsupported.

**Scheduled sources** periodically checks a public channel or playlist for new
recorded videos. Enable it deliberately, then review each resulting private
draft: it never automatically publishes. **Pause** stops new discovery; cancel
already queued drafts separately. Removing a source keeps copies but forgets
its deduplication history, so re-adding it can copy older entries again.

## Edit a publication

Open its management overview for metadata, captions, chapters and poster edits.
For films with a retained native source, **Edit the published film** supports:

- **Reprocess** with current settings while retaining the selected source window.
- **Trim** using times relative to the currently published film. Review retimed
  captions/chapters and supply fresh written alternatives where cleared.
- **Replace** using your own ready private draft in the same channel.

Prepare and review a private candidate, then explicitly apply it. The old film
stays playable until application succeeds. The post URL, author, channel,
comments and playlist references stay intact. A stale review or failed process
cannot silently replace the film. Already delivered copies cannot be recalled.

## Playlists, downloads and embeds

Create a channel playlist through **Video studio → Playlists**. Add native or
remote Video links; this references their media without copying it. Repeats are
allowed. Set optional whole-second start/end clips, reorder entries or remove
them. Previous/Next links do not force automatic navigation.

Public playlists are discoverable; unlisted ones require a link; private ones
are restricted to authorized channel creators. A playlist never grants access
to an otherwise restricted video. Deleting one keeps the videos. Importing a
PeerTube playlist retains its remote ownership; create your own to curate it.

Creators use **Downloads and embeds** to control explicit offers. Viewers can
copy embed code in **Video settings**, or download an eligible MP4. These are
not DRM controls: disabling an offer cannot prevent saving publicly played
bytes. Active live broadcasts have no completed MP4 download yet.

See [live broadcasting](live.md) for camera/encoder workflows and
[client APIs](../api/video.md) for programmatic editing and history.
