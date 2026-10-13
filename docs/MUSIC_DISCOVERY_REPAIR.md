# Music discovery and listening repair

The initial Songhive acceptance covered published ActivityPub audio, but did not
cover a listener starting from a public server or album link. That gap left the
main Music page unable to find the public Songhive catalogue. Following a music
resource subscribes to later activities; it does not load its existing songs.

The required listener journey is Music → search or open a server/resource link →
view local details and tracks → listen in Plamenu. Save, subscribe, manage queues
and publish remain separate choices. Advanced operator/client tools belong after
the ordinary listening actions.

Reproduction on 12 October 2026: the reported Songhive album has a publicly
readable track in its anonymous catalogue API, but the track's ActivityPub URL
returns 404 because it is not published as an ActivityPub post. A public catalogue
record must retain its own API authority and must not masquerade as a published
ActivityPub Audio object. Origin ACLs, moderation and live stream availability
still apply; payments and provider integrations remain excluded.

- [x] Reproduce the reported album and identify the discovery and protocol gaps.
- [x] Put remote search/link lookup on the Music landing page.
- [x] Browse the public Songhive catalogue with local detail and play actions.
- [x] Resolve album tracks from the anonymous catalogue without requiring a Follow.
- [x] Keep public catalogue authority separate from published ActivityPub identity.
- [x] Show unavailable/offline/metadata-only media clearly.
- [x] Put ordinary browsing/listening ahead of advanced management navigation.
- [x] Test unpublished public tracks, private/paid denial, current access and playback.
- [x] Verify the reported real album through browser playback on the repaired build.
Deployment acceptance: create a signed repair commit, retain a staging backup,
verify schema 180 and the deployed binary hash, and repeat the real album
playback in staging before closing this task.

Acceptance evidence: the initial focused run passed 64 cases; the final affected
selection passed eight cases, including current public API authority, live-stream
availability and independent album/artist membership. All six actual-peer
Songhive cases passed, including decoded unpublished catalogue audio and private
revocation. Seven browser journeys passed across Chromium, Firefox and WebKit,
including desktop/mobile and no JavaScript. The reported real song decoded
234 seconds of media and advanced past 13 seconds in Plamenu; the public live
radio advanced past 110 seconds without a media error. The 320px listening page
has 305px scroll width. Screenshots and receipts are retained in the local
`discovery-repair-v1` evidence directory. A continuous live stream offers listening
and saving, with finite ZIP download controls omitted.

This repair uses focused checks. The earlier cumulative full-suite and benchmark
evidence remains separate; it is not described as a new full release run.
