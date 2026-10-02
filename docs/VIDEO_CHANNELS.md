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
in the recipient's channel list; they grant nothing until explicitly accepted.
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
