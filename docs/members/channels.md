# Video channels and collaboration

A video channel has a stable address, profile and subscribers. Create one in
**Video studio → Create a video channel**. **Channels** lists your channels and
pending invitations. A channel is separate from a discussion group.

| Role | Permissions |
| --- | --- |
| Owner | All channel work, invitations, ownership transfer and deletion |
| Editor | Channel profile and all its published videos |
| Publisher | Upload and manage their own channel publications |

Invitations require acceptance; changing a role requires new acceptance.
Revocation takes effect on current work as well as later edits. Collaborators'
private drafts do not become shared drafts. Profile **Controllers** identifies
accepted roles; pending invitations remain private.

Owners/editors can set avatar/header descriptions, manage publications and
review channel follow requests. Relevant channel notifications link to their
review forms, subject to the recipient's preferences and existing content
access. Following a channel grants no editing rights.

## Transfer or delete

The owner chooses **Transfer ownership**, names a local recipient, and sends an
invitation. The recipient accepts by entering the immutable channel address.
Finish/cancel private drafts, schedules, broadcasts and recording finalization
first; retire recurring broadcasts before handoff. An unsafe transfer is refused.

Transfer keeps the channel address, subscribers and publication identities,
but revokes existing collaborators/invitations and completed broadcast
credentials. The old owner gets no implicit role. The new owner can reinvite
people. Original authorship is preserved: deleting an author's account still
removes that author's videos, including those in a transferred channel.

**Delete channel** requires its address and permanently retracts publications,
removes drafts/schedules and tombstones the channel. This includes collaborators'
channel videos; their accounts and other channels are unaffected. Storage
cleanup runs in the background. The address cannot be reused.

## Delivery analytics

**Delivery analytics** is off until the owner enables it. Owners and editors
can read daily UTC response/byte totals; only the owner changes collection,
retention or erases them. Retention is 1–90 days, default 30. Disabling collection
erases retained totals; **Erase delivery totals** keeps the chosen policy.

These are best-effort counts of body bytes emitted by the server, **not views,
unique people, watch time, billing traffic or P2P statistics**. No viewer
identities are stored in these aggregates. Retries and bots can contribute;
crashes can undercount. Operators can disable collection server-wide.
Private [watch history](video.md#watch-privately) is a separate member setting.
