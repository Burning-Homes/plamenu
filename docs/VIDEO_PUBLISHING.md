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
