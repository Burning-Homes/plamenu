# Video playlists

Open **Video studio → Playlists** (`/videos/playlists?mine=true`), or the public
library at `/videos/playlists`. The **Create playlist** disclosure is above
the list and also contains playlist import; it works without scripts. The public library
lists public playlists; **My playlists** includes private and unlisted channel
collections you can manage. You need a video channel to create a playlist.
The normal native installation already contains everything required.

## Authoring and playback

Choose a channel, title, description and visibility. Add a Video by its copied
first-party permalink or canonical ActivityPub URL. Include native or remote
Videos without copying or re-encoding their media. A channel can have 100 active
playlists, each with up to 1000 entries. Each occurrence has a stable identity:
duplicates are deliberate, and can have different clips.

Start and optional end are whole seconds. End must follow start and both must
fit the known duration. A blank end means the Video's end. Waiting live
broadcasts with unknown duration cannot have bounded clips. Clips create no
additional media file or per-viewer encoder.

Open **Edit entry** to change its position, boundaries or remove it. Numeric
positions work with keyboards and without scripts. Scripts additionally offer
one-action up/down controls and restore focus to the moved entry. Entries are
shown in pages of 25; moving can place one anywhere in the collection.

The normal player retains captions, descriptions, HLS, default-on compatible P2P
and its opt-out. The progressive source carries a media fragment for native
no-JS clip playback. Scripts apply the same initial seek/end pause to HLS and
announce completion. **Previous entry** and **Next entry** are ordinary links;
playback never forces automatic navigation. Open the original Video to watch
outside the clip. Missing/deleted/restricted Videos keep their position but show
an unavailable message, not an unauthorized player.
Deletion of a cached Video also clears its reference from duplicate occurrences
while retaining entry identities, positions and clip boundaries. This covers
hard deletion, thread stubs and channel/person or moderation cleanup.

## Permissions and visibility

Owners and accepted editors manage all playlists in a channel; accepted
publishers manage only those they created. Invitations alone grant nothing.
Revocation and handoff use the same locked permission boundaries as Video
authoring. Handoff preserves Playlist/entry identities, makes the accepted
recipient the effective owner and leaves no implicit authority for the old owner.

- **Public**: discoverable and federated.
- **Unlisted**: reachable by link and federated, but excluded from the public
  library and public actor-history collection.
- **Private**: readable only by authorized channel creators. No public AP root,
  page or element is exposed. Moving a shared playlist to private retracts the
  remote Playlist; sharing again retains its native canonical identity.

Public/unlisted playlists cannot include restricted Videos. Every opening checks
the Video's current audience/moderation, so later privacy changes do not expose
media, titles or status IDs through the collection. A private playlist does not
confer access to somebody else's private Video.

Edits submit the displayed revision. Stale forms/API requests are rejected
instead of overwriting concurrent changes. Reload before retrying. Deletion
requires typing the title, removes entries and federates Delete, but does not
delete Videos. Channel/person deletion also retracts native playlists; historical
creator deletion protects their originals after channel transfer.

## PeerTube import and federation

**Import playlist** accepts public/unlisted ActivityPub Playlist URLs such as
`https://peer.example/video-playlists/{uuid}`. The saved collection remains owned
at its origin; **Refresh from origin** retrieves authoritative metadata. To
curate differently, create a native collection and add its Videos there.

Plamenu interoperates with PeerTube's `Playlist`/`PlaylistElement` vocabulary,
channel audience/ownership, ordered pagination and per-entry timestamps. Native
Create/Update/Delete are signed by the current channel owner, including channel
and account followers; actor `playlists` supplies public history.

Import verifies Group and Person ownership, exact identities and same-origin
root/page/element URLs. Existing transport policy, SSRF protection, same-origin
redirects, a 1000-entry/40-page ceiling, 2 MiB response and 8 MiB aggregate budgets,
eight concurrent auxiliary fetches and a 45-second timeout bound the operation.
The aggregate budget is checked incrementally; at most eight bounded responses
can be in flight, rather than collecting the entire remote response set first.
Invalid/partial/looping collections never replace a saved one. Updates preserve
entry IDs, stale revisions cannot roll metadata back, and signed Delete fences
prevent delayed old Create resurrection. A newer canonical republication can be
imported after a privacy transition.
Known-playlist deletion refreshes channel ownership and fails closed if the
origin is unavailable; cached former-creator authority is never restored by a
fetch failure. A transient verification failure remains retryable.

Imports retrieve metadata, not every Video or media file. Only an entry opened
by a viewer is resolved through the existing guarded Video path. No PeerTube
plugin ABI or full PeerTube REST API is claimed. Browser/Axe evidence supports
accessibility verification but does not replace the manual AT release gate in
`ACCESSIBILITY_RELEASE.md`.
