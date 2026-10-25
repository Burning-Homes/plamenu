# Posting

Ordinary posts allow 5,000 characters by default; your server can set a different
limit. Long-form articles have a separate default limit of 50,000 characters.

Open **New post** to write a post, or use **Reply** or **Quote** on an existing
post. The toolbar has controls for attachments, polls, scheduling, content
warnings, emoji, visibility, quote permissions, text format, and language.
Use **Preview** to check formatting before publishing.

## Text and media

Choose plain text, Markdown, or HTML from the text-format menu. HTML is
sanitized. Attach images, audio, or video and add a short description when the
attachment conveys information. For an image that only adds visual decoration
or repeats the nearby post text, select **This image doesn't add information to
the post** instead. Upload limits are set by your server; video processing may
take time. Use the sensitive-media checkbox and content-warning field when
appropriate.

For audio, add a transcript including dialogue and meaningful sounds. For
video, add a transcript/media alternative that also describes important visual
information, and attach timed captions as a UTF-8 WebVTT (`.vtt`) file when the
video contains speech or other meaningful audio. Captions should identify
speakers when needed and include meaningful non-speech sound.

For video, choose **This video includes audio description** only when the
uploaded video actually contains it. If ordinary dialogue and narration already
convey every important visual, choose the separate soundtrack option. A
transcript does not replace audio description for prerecorded synchronized
video at WCAG Level AA.

## Audio, video and live broadcasts

**New post** offers **Audio**, **Video** and **Go live** alongside ordinary posts.
Choose **Audio** for a titled recording: select one audio file, listen to the
attached preview, and add a description. Artist, album and other catalogue
fields are under **More music details (optional)**. Your audience and the usual
post interactions apply. [Music](music.md) also supports private uploads,
albums, playlists and a listening queue.

Choose **Video** to open the video upload and review flow. Your video stays a
private draft until you publish it. **Videos** in the main navigation opens the
watch catalogue; upload and live creation are available there too. Local and
remote videos use the same playback controls. Thumbnail previews appear when
available, and native controls remain available without scripts.

Choose **Go live**, create a channel if this is your first broadcast, and give
the broadcast a title and audience. **Continue to camera setup** opens camera
and screen controls immediately. Granting capture permission only opens a
private preview. Check it, then press **Go live** to start transmitting.
**Cancel preview** releases the camera and microphone without going live.
Keep the page open while broadcasting and use **Stop broadcasting** to finish.
Streaming apps use **External encoder and stream key**; those credentials are
not needed for broadcasting from the browser. Screen capture and browser
recording availability depend on your browser and device.

For the complete workflows, see [video publishing](video.md) and
[live broadcasting](live.md).

## Visibility

| Visibility | Access |
| --- | --- |
| Public | Publicly readable and eligible for public feeds |
| Unlisted | Publicly readable, omitted from public feeds |
| Followers only | Accepted followers and addressed recipients |
| Private mention | Mentioned accounts |
| Local only | Signed-in members of this server; not federated |

Private mentions are not end-to-end encrypted. Operators of servers storing a
post can access it. Edits and deletion requests may reach remote servers late,
and cannot remove independent copies.

**Who can quote** offers Anyone, Followers or Only me. This controls Plamenu's
consent-aware quote workflow; it cannot stop independent copying or force every
remote server to honor the policy. Visibility and quoting permission are
separate decisions.

## Polls, scheduling, and edits

The poll control sets choices, duration, and whether multiple choices are
allowed. The schedule control accepts a publication time in your configured
time zone. Notes and articles can be scheduled; group posts and events cannot.
Manage queued posts in **Settings → Scheduled posts**. Failed publication stays
queued for retry. If a post remains overdue, contact your server's operator or
cancel it from that page.

Use a post's menu to edit it. You can change its text, content warning, language,
and attachment descriptions, or remove attachments. The edit form keeps the
original visibility and does not add new attachments or polls.

The disclosure beside the post type selector explains how other servers may
display that type. Replies use Notes.

## Articles

Choose **Article** below **New post**, then enter a title and body. The server
has a separate character limit for articles. Use **Schedule for later** to queue
an article. Open the disclosure beside the type selector for details of how
other servers display it.

## Events

Choose **Event** below **New post**, then enter an event name, description,
and start time. The time zone defaults to your account preference; both dates
use the selected zone. Zone labels include the UTC offset for the start date.
Optional fields include an end time, venue, online attendance, capacity, and
tentative status. **Who can attend** selects
open attendance, approval, invitation, or an external attendance link.

Events federate as ActivityPub `Event` objects. Remote software may display
only a link and may not offer RSVP controls. See [organizing events](groups-and-events.md#organize-an-event)
for participation and cancellation.

## Groups

Open a group and use its post action to start a thread. The group composer has
a title and an optional link; a link requires a title. Group posts are public
and subject to the group's posting, membership, and moderation rules. A titled
ordinary group post is sent as an ActivityPub `Page`.

## Personal custom emoji

**Settings → Custom emoji** manages your collection. Upload PNG, GIF, or WebP
images, or choose **Borrow custom emojis** from a post or profile's menu. Your
emoji appear alongside server-wide emoji in composers and reaction pickers.
They work in post bodies, content warnings, polls, profiles, and reactions.

Your role and the server's limits control uploads and borrowing. You can rename
and recategorize original uploads. To replace a borrowed emoji, delete it and
borrow again. Removing upload permission still allows management of existing
emoji.
