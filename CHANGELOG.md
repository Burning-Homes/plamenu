# Changelog

## Unreleased

- Keep long inbox participant links visible and independently truncated,
  with usable hit areas and room for keyboard focus outlines.

- Explain browser sign-in limits with a localized, focused page and native
  return link, preserving HTTP 429, admission limits and API JSON responses.

- Focus rejected OAuth second-factor errors while retaining code-entry focus
  for fresh challenges and the existing authorization/consent flow.

- Focus two-factor setup and disabling errors, and associate rejected setup
  codes with their field while retaining normal focus for fresh enrollment.

- Preserve keyboard focus after refused poll votes and when voting results
  replace the active form, without taking focus from later navigation.

- Preserve declared message language in private-message previews and content
  warnings while keeping interface labels in the interface language.

- Focus the existing app-session creation error after a rejected package,
  keeping session details and advanced choices available for correction.

- Improve composer-error contrast and focus, preserve keyboard context through
  attendance requests, and expose updated attendance text as status messages.

- Preserve group-creation drafts and policy choices after validation errors,
  with focused feedback associated with invalid names.

- Focus the existing two-factor sign-in error after a rejected code while
  retaining code-entry focus for fresh challenges.

- Preserve rejected publication times and queue pagination when rescheduling,
  with focused validation feedback associated with the affected time field.

- Localize private-message inbox controls and expand narrow inbox menus within
  their cards so conversation links remain visible and reachable.

- Keep privacy choices and domain text in the settings form after domain
  validation fails, with a focused explanation and field association.

- Keep existing list and filter edits after validation errors, including keyword
  drafts, and focus the existing error so the same form can be corrected.

- Give relationship-selection checkboxes localized account, domain and hashtag
  names for assistive technology, without changing their layout.

- Focus password-change errors and associate them with the affected field,
  preserving independent two-factor setup and existing authentication rules.

- Preserve list-creation choices and rejected member handles, with focused
  validation feedback and an associated invalid-title message.

- Show report forms and confirmations in the selected interface language,
  including category guidance and optional mute/block actions.

- Localize media-viewer labels and keep keyboard focus on a visible control
  when image navigation hides the active button.

- Give search pages a localized heading for assistive-technology navigation
  without changing the visible search layout.

- Retain filter titles, keywords and selected settings after rejected creation,
  and focus the existing error for correction.

- Preserve list semantics and focus the existing error when a remote-follow
  handle needs correction, including without JavaScript.

- Keep profile text, image descriptions and choices in the editor after a
  rejected upload, with a focused error and guidance for retrying.

- Keep the proposed email address after rejected account changes and focus
  the existing error so keyboard users can correct it.

- Focus password-reset errors while preserving their list semantics, and show
  login failures in the selected interface language.

- Preserve birth date, accepted rules, and login identifier after form errors,
  focus the existing error summary, and underline the signup rules link.
  Give post actions descriptive accessible names, explain failed actions,
  and keep keyboard focus available for retry without adding steps to
  successful use.

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
