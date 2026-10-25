# Future work

This is a contributor discussion list, **not shipped functionality or a release
commitment**. Check current routes and user guides before proposing work: native
Audio, discovery, music libraries and video editing already exist. Detailed
historical comparisons are retained in Git; implementation planning belongs in
scoped issues with current evidence and acceptance criteria.

Candidate areas include broader reply/like/boost consent, quiet historical post
archive import, durable writing drafts, saved searches, richer event calendars
and guest registration, restricted native video, assisted media alternatives,
external identity providers and optional object storage.

Client work should close specific gaps in the [Lemmy adapter](LEMMY_API_COMPATIBILITY.md)
and [Mastodon-compatible surface](api/index.md), without equating API compatibility
with ActivityPub interoperability. Preserve source identity, permissions and
moderation across any new workflow.

A proposal should state the user problem, current behavior, proposed task flow,
data/security/migration effects, explicit exclusions and how completion will be
verified. New supported behavior must be documented in the relevant member,
operator or API guide when it ships.
