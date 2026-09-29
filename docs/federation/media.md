# Video media alternatives

Plamenu adds optional, plain-text alternatives to canonical ActivityPub `Video`
objects. The object retains PeerTube's standard captions (`subtitleLanguage`),
chapters (`hasParts`), post description (`content`) and ordinary file attachment.
There is no requirement that a peer support the extensions to accept the Video.

`transcript` maps to `http://schema.org/transcript` in the scoped Video context.
It is text, not HTML or a fetched URL, and may describe important visuals as well
as dialogue and sounds. Native input accepts up to 50,000 Unicode characters;
untrusted over-limit/NUL-containing values are discarded. Null/absence on a
canonical update removes the previously stored transcript.

The following terms use this document's canonical namespace,
`https://docs.plamenu.codefloe.page/federation/media#`:

<a id="visualDescription"></a>
## visualDescription

A short plain-text description of the visual media, distinct from the post
description or captions. Maximum 1,500 Unicode characters. An explicit null
clears it. Omission does not replace an ordinary attachment's own alternative.

<a id="audioDescribed"></a>
## audioDescribed

A boolean author statement that the actual soundtrack includes integrated audio
description. It is not proof of conformance and does not cause an audio track
to be synthesized. Omission/null is false.

<a id="visualsConveyedInAudio"></a>
## visualsConveyedInAudio

A separate boolean author statement that the ordinary soundtrack already
conveys every important visual. Local authors cannot select both claims.
For a conflicting remote object, `audioDescribed: true` takes precedence.
Omission/null is false. Text-only captions never imply either claim.

## Authority, identity and display

Only an already authorized canonical Video update can modify these fields.
Root alternatives apply to its canonical primary media, not unrelated files
in a mixed-attachment post. Readable text is escaped when rendered; no network
fetch is performed for any alternative. Single-primary deferred Video updates
retain the attachment identity and existing same-origin player/transcript links.

Create, dereference, channel history and signed Update share the same serializer.
Local edited alternatives, edit timestamp and outbox jobs commit atomically,
including authorized collaborative edits under original uploader attribution.
Client attachments expose equivalent snake-case keys. The first-party player
shows written alternatives and explicit author soundtrack statements in native
HTML disclosures, independently of timed captions.

PeerTube 8.3 ignores these extensions. Its supported description/caption/chapter
updates are tested separately in both directions; lossless alternative
re-export through PeerTube is **not** part of the compatibility claim.
