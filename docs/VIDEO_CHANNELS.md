# Video channels and collaboration

Open Video studio → **Create a video channel** for the dedicated creation form
(`/videos/channels/new`). **Channels** (`/videos/channels`) lists your channels
and invitations in 20-item pages. Each name opens its management page; long
libraries do not bury creation or repeat profile/collaboration controls per row.
These workflows
use ordinary HTML forms and work with JavaScript disabled. A channel has a
stable address and its own subscribers; it is not a discussion community.

The owner can invite an active local person as a **Publisher** or **Editor**.
Publishers upload videos and manage their own publications. Editors can also
change the channel profile and edit/delete any of its videos. Only the owner
controls invitations, ownership and channel deletion. Pending invitations appear
in the recipient's channel list and notifications; they grant nothing until explicitly accepted.
Changing a role requires new acceptance. Owners can revoke access immediately,
including the ability to edit the collaborator's previous channel publications.
Connected encoder and browser broadcasts detect revoked authority within their
two-second credential-check interval. The creator's private upload files do not
become another person's private drafts merely because they were invited.

Published videos are listed under **Videos** with links to Edit, captions,
chapters and metadata. The metadata manager also offers confirmed deletion,
using the same accessible confirmation flow as ordinary posts. Edits stay signed
by the publication's original author and preserve its URL and discussion.
PeerTube presents videos under the channel's current owner; Plamenu's canonical
ActivityPub object additionally preserves the actual uploader.

## Profiles and notifications

The channel profile's **Controllers** disclosure links to its owner and accepted
editors/publishers, with each role labelled. Pending invitations stay private.
A person's **Channels & groups** disclosure links back to the channels they own
and groups they own or moderate. **Manage** exposes the actions their current
role permits. Channel links open its library or preselect it in upload/live
forms. Editors and publishers can leave collaboration from the channel manager.
All of these controls work without JavaScript.

Routine channel activity reaches the owner and every accepted editor, subject
to each recipient's notification preferences, filtering and blocks/mutes.
Publishers retain the personal notifications relevant to their own posts; their
role alone does not subscribe them to every channel event. Channel notifications
identify the channel. When a comment also mentions its owner, the owner sees one
notification with channel context rather than two competing alerts.

A delivered comment in a hosted video's reply thread is a channel event even
when it has no Mention tag. This does not create Mention tags, interpret generic
`to`/`cc` as a request to notify, or notify on ordinary unmentioned Note replies.
History imports and duplicate deliveries do not create new comment alerts.
Private replies reach only controllers who can already read that reply; a
channel role never expands its audience.

Channel follow requests link to **Follow requests**, where owners and accepted
editors accept/reject as the channel. Personal follow requests remain separate.
Collaboration and ownership invitations link to their consent forms. Resolved
or withdrawn requests disappear, and revoking a role immediately removes access
to its pending work. Public activity already received can remain in history.
Groups use their own membership/moderation controls; following does not grant a
management role. Group reports go to the people currently allowed to review
them, without exposing administrative account details to group-only moderators.

Existing clients that support Mastodon's notification fallback can display these
extensions and follow their review links. Plamenu does not require a modified
Phanpy client. See [the API contract](api/index.md#managed-actor-notifications).

## Delivery analytics

Owners and accepted editors can open **Delivery analytics** from the channel
manager. Collection is opt-in per channel, stores no viewer identities, and
has owner-controlled UTC retention and erasure. These are delivery aggregates,
not unique-view estimates. See [Private creator delivery analytics](VIDEO_ANALYTICS.md).

## Handing over a channel

The owner opens **Transfer ownership**, enters the local recipient's username,
and sends an invitation. The recipient finds it in **Manage video channels** and
accepts by typing the immutable channel address. They can instead decline; the
owner can cancel or replace the pending invitation.

Before acceptance, finish or cancel unpublished uploads, scheduled videos and
live broadcasts. A recurring broadcast must be retired/deleted, not merely
waiting for its next episode. Outstanding recording finalization must finish;
an unsafe handoff is refused without changing the owner. This prevents abandoned
private media or silently changing an active encoder's authority.

The accepted transfer keeps the channel address, subscribers, and all existing
video identities and original authors. It retires completed broadcast
credentials and revokes **all** existing collaboration grants/invitations.
The former owner gets no implicit role. The new owner can deliberately reinvite
people afterward. They can edit and delete the channel's existing publications
without pretending to be their author. PeerTube receives a signed channel
profile update identifying the new owning Person.

## Deletion

The owner opens **Delete channel** and types its address. This permanently
retracts its published videos (including collaborators' videos), removes private
drafts and scheduled posts, and tombstones the channel actor. Media/source files
go through the existing durable cleanup queue. Collaborator accounts and their
other channels are unaffected. The address cannot be reused.

Deleting a person's account also deletes channels they still own. A previously
transferred channel remains owned by its recipient, but the deleting person's
own original videos are still removed, including from that transferred channel.
Transferring a channel does not override an original author's account-deletion
privacy guarantee.

## Channel identity on posts

A channel uses the same account profile images as a person or community.
Owners and accepted editors can upload its avatar and header and provide image
descriptions in **Manage video channels**. Publishers and pending invitees
cannot edit the profile. Empty upload fields preserve the existing images.
Profile edits keep the actor address and subscribers and publish an actor Update.

Video cards retain the content author's identity and show **published on** with
a link to the channel on detail, profile, home, public, search and notification
surfaces. This also works when a person's boost carries the video. Imported
videos show this context only after the channel and its ownership were verified;
an arbitrary Group announcement alone is insufficient. Channels retain their
subscription and collaboration model and do not acquire community membership,
voting or moderation powers from this context.
