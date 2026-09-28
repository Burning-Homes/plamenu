# AP09 M1 / M4 closure run

Goal: fully verify M1 and M4 against their existing acceptance gates. Neither an
implementation commit nor an inventory-row count closes a milestone. No push,
merge or staging deployment is authorized for this run.

## M1 — Channels and native publication

- [x] Channel profile edit and confirmed deletion; owner account deletion.
- [x] Consent-based publisher/editor roles, invitation refusal, revocation and
      immediate permission enforcement, including suspended accounts.
- [x] Accepted ownership transfer preserving channel identity and existing Video
      identities, with no residual implicit privilege for the previous owner.
- [x] Publication/edit/delete authorization and transaction/outbox failure tests.
- [x] Real PeerTube channel profile/owner updates, collaborative publication,
      accepted/playable Video attribution, edits/deletes and Follow/Undo exchange.
- [x] HTML-first management and invitation/transfer journeys; keyboard, narrow
      viewport, Chromium/Firefox/WebKit and scripts-disabled browser evidence.
- [x] Evidence linked in the master progress log and relevant inventory rows.

M1 signed off 2026-09-29. See the master log's M1 sign-off for test commands,
results, retained first-run failures, browser observations and scope boundaries.

## M4 — Playlists and library

- [x] Create/import/edit/confirmed delete; public/unlisted/private visibility.
- [x] Add/remove/reorder with stable entry identity, bounded duplicates and
      missing/deleted video entries that cannot widen the video's audience.
- [x] Validated clip start/end boundaries and accessible playback/navigation.
- [x] Authenticated ownership, guarded remote import, delayed/update/delete
      retention and real PeerTube round-trip federation.
- [x] Keyboard and no-JS management/playback, responsive styling and the browser
      matrix; API permission and transactional failure regressions.
- [x] Evidence linked in the master progress log and relevant inventory rows.

M4 signed off 2026-09-29. The master sign-off records 61 Rust tests, three actual
PeerTube/API/HTML E2E cases, seven browser projects and quality checks. The
post-transfer standalone Video Delete extension also closes the current-owner
signer edge found while reviewing PeerTube cleanup logs. Both selected functional
milestones are verified; this is not an overall parity or manual-AT claim.

## Evidence rules

Keep actual failures and unsupported environments visible. Browser accessibility
tree and Axe checks are supporting evidence, not human screen-reader testing or
a WCAG-conformance claim. The existing M9 release/manual-AT gate is unchanged.
Do not redefine an acceptance gate to meet this goal.
