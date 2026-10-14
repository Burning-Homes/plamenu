# Changelog

## Unreleased

- Offer the existing Translate control regardless of a post's declared
  language. Detect the source from the text for web and API translation,
  including the no-JS flow, and refresh translations cached with forced labels.

- Support exact Chat Completions URLs and nested provider request options for
  OpenAI-compatible translation services, including reasoning settings and
  configurable token budgets. Preserve existing endpoint configurations.

- Simplify JavaScript video playback with a compact control bar and labeled
  settings popover. Show adaptive quality before playback, populate resolutions
  after stream discovery, and retain keyboard controls and native fallback.

- Add native Audio publication, structured music metadata and a searchable
  music catalogue, public actor libraries and independent music subscriptions.
  Add a digest-pinned Songhive 0.4.9 interoperability peer.
  Add retained tracks, reusable artists and albums, artwork, named libraries
  and ordered playlists, a persistent music player, listening history and
  statistics, radio records, independent saved references and music favourites.
  Album details and explicitly shared tracks have separate access controls;
  account retirement revokes music grants and scoped subscriptions atomically.

- Add a searchable, paged video workspace with dedicated upload and channel
  creation, video management, and accessible player settings. Simplify video
  and live authoring with optional sections, readable language choices,
  contextual help, retained validation choices, and narrow-screen layouts.
- Add reusable personal and instance Webxdc app libraries, immutable version
  selection, moderator promotion and publication, a public digest-bearing
  catalog, and opt-in guarded imports from xdcget-compatible sources.
- Keep nested participant replies in their branch when displaying threaded
  conversations, while still promoting the root author's self-thread.
- Add searchable pending-account registration context and safe bulk rejection
  across every filtered result page.
- Include the referenced event card in RSVP and other event notifications.
- Present podcast audio discovered through Open Graph (including Castopod
  episode posts) as playable, cacheable media in the web UI and API.
- Improve WCAG 2.1 AA support for status avatars, attachment previews, prose
  links, scrollable code blocks, accent controls, and moderated cards.
- Preserve verified commit signatures by requiring fast-forward-only Codefloe
  pull-request merges.

## [0.6.0] — 2026-09-11

Initial public release.
