# Changelog

## [0.7.0] — 2026-10-27

- Add a music catalogue with native audio publication, artist and album metadata,
  libraries, playlists, subscriptions, listening history and a persistent player.
  Discovered podcast episodes can be played and cached.
- Expand video and live-broadcast workspaces with channel management, simpler
  authoring, adaptive playback controls and more reliable live replica playback.
- Add personal and instance Webxdc app libraries, version selection, public
  catalogues, moderator publication and guarded imports.
- Improve threaded conversations, event notifications and bulk registration
  moderation. Make translation available regardless of declared post language
  and support more OpenAI-compatible translation configurations.
- Improve keyboard navigation, focus and error recovery, screen-reader labels,
  narrow-screen layouts and localization across account, posting and media flows.
- Reduce repeated database work and test/build setup costs; improve discovery,
  federation and fixture reliability. Refresh dependencies and integrations.
- Replace development diaries with a concise handbook for users, administrators
  and hosters, including installation, backups, upgrades and troubleshooting.

### Upgrading and limitations

This is an experimental release. Back up the database, media, configuration and
matching encryption secret, and rehearse the upgrade on a restored copy. Forward
migrations may prevent binary-only rollback; see the [upgrade guide](docs/operators/upgrading.md).

Validation includes a complete audit followed by focused repair checks, not a
clean full pass on the final release revision. Accepted gaps remain in browser
and federation coverage and human accessibility verification; no WCAG conformance
claim is made. The WebAuthn dependency advisory RUSTSEC-2026-0387 remains a temporary
accepted exception pending a published upstream upgrade. See [known limitations](docs/KNOWN_ISSUES.md).

## [0.6.0] — 2026-09-11

Initial public release.
