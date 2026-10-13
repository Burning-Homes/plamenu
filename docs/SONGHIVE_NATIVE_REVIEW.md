# Pinned Songhive native contract review

Reviewed 2026-10-10 against Songhive v0.4.9, revision
`4243a566f8de767b824a55df69ae87c03d17e35b`. This closes the source-level
implementation inventory. Current-build interoperability and browser acceptance
remain separate, open steps in [the completion checklist](SONGHIVE_COMPLETION_CHECKLIST.md).

The local route inventory covers 43 modules and 365 route declarations after
excluding the payment and scrobbling modules. Mixed provider modules include
excluded declarations; that count is an inventory, not a claim of 365 identical
Plamenu endpoints. Plamenu exposes the native contracts through its existing
API, accounts, permissions and HTML-first design.

| Reference surface | Plamenu equivalent reviewed |
| --- | --- |
| Tracks/files, artists/albums, genres/tags, search | Music assets/entities/artwork, guarded original/stream/rendition routes, term catalogues and native resource forms |
| Libraries/playlists, collection/favourites, remote discovery | Ordered native collections, canonical saved/favourite/queue references, origin-controlled remote resources and current audience checks |
| Playback sessions, radio, streams/outputs | Persistent account/device queues, saved radios and native HTTP output workers; the pinned radio track-generation endpoint returns an empty list |
| History/stats and Subsonic/OpenSubsonic | Explicit listen events, current music statistics, purpose credentials and compatibility adapters with batched current catalogue reads |
| Podcasts, RSS/Atom, ZIP downloads | Feed and episode workers, authorized feed serializers, durable bounded exports and progress/cancellation |
| External libraries | Native operator-approved local folders only; held file descriptors, reconciliation, explicit writeback and durable mutation recovery |
| Activities/statuses/timeline, mentions, Webmentions | Native posts/conversations/accepted quotes and interaction controls, exact resource activity feeds, permanent mentions and verified website cards |
| Notifications | Explicit read overrides/inbox management, enabled-reason masks, channel preferences, SMTP receipts/digests and seen-only retention |
| Accounts/auth/tokens/sessions/MFA, privacy/reports/shares | Existing account/moderation/session services, scoped music credentials, resource grants and revocable share capabilities |
| Admin users/settings/invites/audit/tasks | Existing administration plus native music-job counts, active/pending inspection, audited single/bulk cancellation and protected file recovery |
| Profile pages, themes/locales, PWA | Existing native layouts and preferences, one global Music entry, resource/workspace navigation and static-shell-only offline caching |

Payments and named third-party provider, storage, scrobbling, casting and
metadata-enrichment integrations remain excluded. They introduce no required
service credentials or gates.

Concrete omissions found in this final review were closed:

- Native status attachment rendering now uses the same guarded music URL as
  ActivityPub. The previous client URL pointed into the permanently private
  music storage namespace and returned 404 for a live publication.
- Operators can inspect music worker states/counts and stop one or multiple
  active runs. Cancellation uses database leases/generations and records an
  audit entry in the same transaction. Completed caches, saved configuration
  and durable filesystem recovery are protected.
- Disconnecting a folder with pending filesystem intent removes the index;
  recovery cancels the uncommitted operation using its exact original proof.
  It does not silently delete the operator's audio.

Mounted original disappearance has an explicit boundary: native original
and stream/rendition serving require the current physical source. A retained
publication cache remains private internally; neither the original-download
route nor the generic media-key route serves it as a fallback. An already-open
response rechecks authority under the existing one-second refresh contract.
Privacy withdrawal continues to revoke publication access.

Source checks verified matching English/Russian music, mentions and notification
keys; native labels/forms, paging and permission-gated controls; and the service
worker's restriction to versioned static shell assets. Browser layout, keyboard,
PWA behavior and screenshots must still be accepted on the immutable candidate.

Focused evidence is recorded beside completed checklist items. No source-review
result substitutes for the final actual-peer, browser or full-gate results.

The final schema guard exposed fixed-shape worker metadata stored as JSON.
Migration 179 converts remote details/proofs, folder scan options, export recipes
and item errors, and filesystem mutation intents to typed PostgreSQL composite
records and arrays. Ordered export snapshots retain their immutable recipe;
filesystem identities preserve the complete unsigned range. Public JSON
serialization stays unchanged. External parsed Microformats remain an opaque
wire cache; ownership, authority and lifecycle stay in normalized columns.
The schema guard checks that composite fields contain no hidden JSON storage.
