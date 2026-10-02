# Video viewing

The Videos space links to channel libraries, subscriptions, search, playlists,
watch history and viewing settings. Watching never requires peer-assisted
delivery: the player offers a server-only choice before contacting a tracker,
and the ordinary MP4 player works without JavaScript.

## Browsing, searching and subscriptions

Open **Videos** in the primary navigation. **Latest** lists public Videos;
**Followed channels** lists Videos from accepted channel subscriptions, not all
uploads by the channel's owner. Search matches video titles and descriptions.
Open a card's creator/channel link to search just that library or visit its
profile. **Follow channel**, **Unfollow** and pending-request cancellation are
ordinary keyboard-operable forms, including without scripts.

The gallery loads posters and links, not a player for every card. Warnings hide
sensitive titles and posters. Blocked, muted, suspended or deleted content is
not revealed by searching; native channel libraries also respect their current
owner after a handoff. Anonymous browsing and searching depend on separate
instance policies; subscriptions require signing in.

**Show more** advances older results. Very heavily filtered pages can be empty
and still offer this link: continue to reach older visible matches. Without
scripts, each populated page has at most twenty cards. With scripts, the gallery
can load another page ahead as you browse; the ordinary link remains available.

## Peer-assisted delivery and server-only playback

Eligible HLS videos and live streams use PeerTube-compatible peer delivery by
default. Open the post's **… → Video settings** before playback. Its
**Share video with other viewers** choice explains that
the tracker and participating viewers can learn your network address. Turn it
off before playback to avoid tracker/swarm contact. No-JS playback is always
server-only; unavailable peer delivery falls back to ordinary HTTP delivery.

In **Video viewing settings**, save this choice using the ordinary HTML form.
It applies across signed-in browsers, including signed-in public embeds.
Enhanced player changes save the same account preference; failure is announced
and the immediate playback choice still applies. Anonymous players remember
their choice in that browser when storage is available. An older browser-only
opt-out stays off until you deliberately save an account preference; migration
does not silently reset it. History remains a separate, default-off choice.

Public read-only embeds support `?p2p=0` for server-only delivery even when
third-party cookies are unavailable. The disabled choice explains this explicit
embed setting. Embeds never contain account actions, CSRF tokens or private
history, and signing in never makes a private Video embeddable. Their personalized
network preferences use `private, no-store` responses.

Adaptive VOD startup has bounded recovery: a buffered decoder that produces no
frames for five seconds, an unstarted load after fifteen seconds, or a fatal
adaptive-player error switches once to the existing standard-video source.
The old peer engine is destroyed, its quality selector removed, and a visible
status/link explains that peer sharing is off for this playback. The account
preference is not changed. The player preserves its position and caption
selection. Paused, hidden and detached players do not restart automatically;
live playback retains its separate interruption/DVR recovery. If standard
playback also fails, the link remains available rather than retrying endlessly.

## Downloading and embedding a native Video

When its creator permits it, **Download playable video (MP4)** saves the current
native film, with range requests supported. The download and frame routes check
current publication/audience and account/channel authority on every request;
their responses are private and not stored by caches. A private or deleted
publication never becomes downloadable or embeddable by signing in.

Open **… → Video settings** on an eligible native Video. Download, peer-sharing
and embedding controls live together here, not below every player. JavaScript
opens a keyboard-accessible modal; Escape or its close button returns focus to
the menu. Without JavaScript the same link opens an ordinary HTML page, where
signed-in viewers can also save their peer-sharing preference without changing
history settings. Playback without scripts remains server-only.

There is one labelled, read-only **Embed this video** field, initially using
`?p2p=1`. The modal's peer-sharing choice updates that same code to `?p2p=0`
when disabled; it never overrides the embedded viewer's explicit opt-out.
The text can be selected and copied without JavaScript. The enhanced
**Copy embed code** button reports success, or focuses/selects the field for
manual copying if clipboard permission is unavailable. The code contains no
credentials. Embedded players retain their own overflow entry for changing
peer sharing before playback.

Public native attachment entities expose a `video_distribution` extension with
`download_enabled`, `embed_enabled`, and eligible `download_url`/`embed_url`.
Active live streams have no MP4 download offer; their completed recordings can.
Flags describe explicit offers, not DRM or a new permission to read content.

## Private watch history and resume

Open **Video viewing settings** from Videos or Settings. History is off by
default. Enable **Save my watch history** explicitly if you want it. History is
local account data, never ActivityPub metadata, public counters or analytics.

With JavaScript, a published Video's player saves a position during playback
(at most once per 15 seconds), on pause/end and when leaving the page. Merely
opening the Video, its metadata or an embed does not record a view. Live
broadcasts, drafts and anonymous players do not save positions. Network failure
does not interrupt playback; a manual save gives visible success/failure feedback.

Without JavaScript, open **Saved playback position** beneath a published Video,
enter whole seconds and submit **Save playback position**. The History page and
the Video offer **Resume video** links. Their native media-fragment fallback
opens the playable MP4 at that time; JavaScript seeks/focuses the in-page player.

Replacing the film keeps its post/media identity but invalidates saved positions
for the different footage. Old players cannot save against the new film: writes
carry the reviewed content revision. Metadata-only edits preserve positions.
Without scripts, a rejected save offers links to reload the current film and
check history settings; it does not silently claim success.
Resume is explicit, never automatic playback on page load.

History has at most 1,000 positions and exposes only positions saved within
90 days. Saves prune older/excess entries; the native hourly maintenance worker
also deletes expired rows in bounded batches, including for idle accounts.
History pages use 20-entry keyset paging, batched media/status loading and
`private, no-store` responses. A Video that is deleted, blocked, suspended or no
longer readable cannot disclose its old title or URL through history. Private
positions are not access tokens and do not widen media authorization.

**Forget this video** removes one entry. **Clear watch history** requires an
explicit confirmation and removes all entries; future playback can save again.
Turning history off deletes all entries atomically and fences queued writers.
Account/media deletion cascades to its records. Normal database backups can
contain historical private data: protect and expire them according to the
operator's backup policy rather than claiming deletion rewrites old backups.

## Client API

These are Plamenu contracts, not claims of PeerTube REST compatibility.
Bearer authentication is required; no endpoint accepts a target account ID.

| Endpoint | Scope | Contract |
| --- | --- | --- |
| `GET /api/v1/video_viewer_preferences` | `read:accounts` | `history_enabled`, `p2p_enabled`, `p2p_configured`, `history_max_entries`, `history_retention_days` |
| `PUT /api/v1/video_viewer_preferences` | `write:accounts` | At least one JSON boolean `history_enabled` or `p2p_enabled`; omitted choices are preserved; history false also deletes history |
| `GET /api/v1/video_history` | `read:statuses` | `{data, next}`; pass both `next.before` and `next.before_media` as query parameters |
| `PUT /api/v1/video_history/{media_id}` | `write:statuses` | Whole `position_seconds`, optional boolean `completed`, native attachment's `content_revision` (omitted means 0); returns `{saved}` (false when disabled or stale) |
| `DELETE /api/v1/video_history/{media_id}` | `write:statuses` | Forget this account's entry only; idempotent |
| `DELETE /api/v1/video_history` | `write:statuses` | Clear this account's history, without changing its preference |

Positions must be within the known duration and at most seven days. A completed
position must be within the final ten seconds when duration is known. History
entries include current Video/media IDs, title, position, completion, timestamp,
permalink and native resume URL. Metadata-only PeerTube updates retain attachment
identity and positions; signed Delete removes the entry from subsequent reads.
Ordinary status/media entities and federation never include private history.

`p2p_configured` distinguishes an explicit account choice from the default. It
becomes true when `p2p_enabled` is deliberately saved, not when history changes.
Clients should preserve a legacy local opt-out until an explicit account choice.
