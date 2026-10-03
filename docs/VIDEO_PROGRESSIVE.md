# Native Video progressive MP4

A completed native Video may have one optional H.264/AAC MP4 for direct
playback, alongside its original delivery file and existing HLS ladder. The
original URL and download endpoint retain their original bytes. Videos already
processed before this feature keep their existing original fallback; there is
no request-time transcode or historical artifact repair.

## Selection and encoding

The shared policy selects the highest existing rendition at or below 720p
within the pipeline's 4,000 kbps video target. Heights retain the existing
portrait semantics. It adds no ladder rung and never upscales. If no candidate
qualifies, a lowest compatible rendition can qualify only with a measured size
smaller than the original; unknown size is insufficient. If the copied original
qualifies, it remains the progressive source without a duplicate artifact.

A selected lower rendition is encoded once into a faststart MP4. The HLS muxer
then copies its compressed packets into the existing single-file fMP4 layout.
The MP4 retains encoder delay, AAC skip information, edit lists and composition
timestamps. Reconstructing an ordinary MP4 from the completed HLS file is
unsafe: the HLS mux has already discarded AAC priming information. In particular,
removing edit lists can add one audible AAC frame of delay despite making
container startup look correct.

The artifact belongs to its existing HLS rendition and native draft generation.
It uses the same reservation, output journal, cancellation fence, cleanup and
ready transaction. There is one optional metadata field, rather than another
queue or publication state. The worker checks codecs, dimensions, duration,
file size and SHA-256 before completing that transaction. Offload transfer also
checks the artifact hash and probes the received file independently.

## Delivery and interoperability

The local API prefers this file when current, validated ready metadata exists,
and exposes the original URL and actual progressive quality under `progressive`.
The original metadata is retained as `meta.source`; `meta.original` describes the
preferred URL's actual dimensions, duration and bitrate; size is in
`progressive`. Invalid, failed, stale or missing metadata retains the original preference. An unexpectedly
missing stored body returns 404; HTML source candidates and the bounded player
fallback retain an original candidate rather than attempting a repair.

Retired or stale journal entries remove the API preference and ActivityPub Link
and deny fresh public serving, even while a JSON metadata field lingers. Only
the active output journal protects a file from orphan reconciliation.

The immutable artifact URL supports byte ranges and a SHA-256 ETag. Matching
`If-None-Match` returns 304; a matching strong `If-Range` permits 206, while a
mismatching validator returns the full 200 representation. Private drafts,
suspended owners and stale generations are excluded by existing publication
checks. Original serving and download behavior remains independent.

The HTML player keeps native controls, a quality-labelled MP4 link and ordered
progressive/original `<source>` candidates for script-blocked browsers. The
adaptive player falls back to the selected MP4 and, on a file error, retries the
original once. It does not loop between failed representations.

The canonical ActivityPub `Video.url` array adds a named `Playable MP4` Link
with the artifact's real quality. Its original and HLS links remain. The
attachment `Document.url`, object identity and Video UUID retain their original
values: consumers that read only that Document still receive the original.
This change does not migrate peer attachment identity.

## Private processor protocol

The consumer performs one authenticated, bounded `/v1/health` request before
job admission. Version 1 keeps its exact artifact count and omits the optional
field. Version 2 can append one progressive artifact with matching metadata.
Unknown versions fail before upload; a generic 400 or 422 does not trigger a
second legacy job. Count, size, path, geometry and hash validation remain
strict for both versions. Release and deployment remain explicit operations.

## Qualification

Targeted tests cover rendition selection boundaries, original identity,
publication authority, cache/range semantics, stale metadata, journal and quota
ownership, strict offload versions and rejection behavior. Production packaging
tests compare compressed video packet hashes, AAC priming and decoded sample
alignment, including silent and nonzero timestamp inputs. Browser qualification
must use the actual production MP4, ordinary playback speed, range/resume,
seeking and EOF in Chromium, Firefox and a supported WebKit environment. A
single successful download is insufficient evidence of compatibility.

The separately observed HTTP/3 transport slowdown remains an open investigation;
a smaller MP4 changes transfer cost, not the underlying transport behavior.

Local qualification on 2026-10-03 passed 67 serial targeted regressions,
workspace/all-target Clippy with warnings denied, and SQLx metadata generation.
The 185.1-second sample produced a 68,271,209-byte 720p MP4 from a
794,304,971-byte delivery file, with all 11,106 frames decoding and identical
compressed video packets in its HLS rung. Chromium, Firefox and supported
Linux WebKit passed metadata, tail seeking and EOF. Short production fixtures
also passed normal-speed full playback, ranges/resume, adaptive playback and
missing-file fallback across those engines.

With a 256,000-byte/second throttle per response, Firefox needed 19.5 seconds
to begin the long sample: its measured video bitrate is about 2.84 Mbps plus
AAC and container overhead. At 512,000 bytes/second it began in 2.0 seconds.
That initial 15-second test-bound failure is retained as a buffering limitation;
it is not a fatal media failure or a guarantee for slow connections. The
sample's nonfatal QuickTime chapter-reference warning exists in the preserved
original too. Native Safari/iOS, a deployment trial and the final fleet/release
gate were not run in this phase.
