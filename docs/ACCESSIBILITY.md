# WCAG 2.2 accessibility status

Plamenu is working toward WCAG 2.2 Level AA for its web interface, with
selected AAA improvements where they do not make ordinary use worse. This file
records the current implementation and the work still required; it is not a
conformance statement.

The normative standard is [WCAG 2.2](https://www.w3.org/TR/WCAG22/). Evaluation
follows [WCAG-EM](https://www.w3.org/WAI/test-evaluate/conformance/wcag-em/).
Automated checks are regression guards and cannot establish conformance without
the remaining manual and assistive-technology evidence.

## Implemented

### Perception and presentation

- Text, destructive actions, form boundaries, active states, and focus
  indicators use contrast-safe tokens in light and dark themes. Active states
  do not rely on colour alone.
- Reduced-motion and forced-colours adaptations are present. Fixed mobile
  navigation reserves focus and scroll clearance.
- Wide application tables and author-supplied tables remain within page reflow
  and are keyboard-scrollable.
- Status content, warnings, polls, citations, edit history, and translations
  expose their language changes programmatically.

### Keyboard and interaction

- Every full page has a localized skip link to a focusable main landmark.
- Disclosure controls retain native semantics and restore focus on dismissal.
- Enhanced visibility, quote-policy, and language selectors implement linked
  combobox/listbox semantics, arrow navigation, Home/End, typeahead, selection,
  Escape dismissal, and focus restoration. Native controls remain available
  without JavaScript.
- Primary status and navigation targets meet the WCAG 2.2 Level AA target-size
  minimum in the tested layouts.

### Forms and critical processes

- Recognized personal-data fields expose appropriate autocomplete purposes.
- Important authentication, password-reset, remote-interaction, account-email,
  registration, OTP, and account-deletion errors identify and describe their
  invalid controls.
- Destructive forms have a server-rendered review step without JavaScript;
  scripted use progressively restores the compact confirmation prompt. The
  gateway validates CSRF and local targets and does not perform the destructive
  action itself.

### User-authored media

- Authors can describe informative images or explicitly mark images that add no
  information. Decorative intent is stored separately from an omitted
  description and renders as a null text alternative (`alt=""`). If conflicting
  values are submitted, decorative intent clears and suppresses stale text.
- Audio and video support bounded plain-text transcripts/media alternatives.
  Video supports validated UTF-8 WebVTT captions served through a same-origin
  native captions track.
- Video authoring distinguishes a genuine audio-described version from a video
  whose ordinary soundtrack already conveys the important visuals. Only the
  first case is advertised as audio described.
- Media alternatives are authoring aids, not blanket upload requirements.
  Applicability depends on the content: decorative images do not need invented
  descriptions, while informative and time-based media need the alternatives
  required by WCAG 2.2 criteria 1.1.1 and 1.2.1–1.2.5.

### Verification infrastructure

- `accessibility/criteria.json` accounts for every WCAG 2.2 Level A and AA
  criterion, including the obsolete status of 4.1.1, and maps applicable
  criteria to reproducible checks.
- The isolated Playwright/axe suite contains 25 tests across Chromium, Firefox,
  and WebKit at desktop and 320-CSS-pixel mobile viewports, plus no-JavaScript
  Chromium. It covers signed-in and signed-out pages, English and Russian,
  light and dark themes, reduced motion, forced colours, keyboard bypass,
  structure, duplicate IDs, and reflow.
- Release evidence is bound to the exact Git revision and browser-report
  digest. The release validator rejects missing, skipped, altered, dirty-source,
  wrong-revision, or incomplete manual evidence.
- Detailed release procedures and evidence formats are documented in
  [Accessibility release verification](ACCESSIBILITY_RELEASE.md).

## Pending before a WCAG 2.2 AA claim

- Complete the manual criterion inventory for the release candidate, including
  keyboard-only operation, focus order and visibility, 200% text resize, 400%
  zoom, 320-CSS-pixel reflow, WCAG text-spacing overrides, contrast in every
  state, target sizing, validation recovery, and complete critical processes.
- Run NVDA with Firefox and Chrome, VoiceOver with Safari on macOS and iOS, and
  TalkBack with Chrome on Android. Record versions, tester, timestamp, results,
  and evidence artifacts.
- Verify native caption controls and media playback across supported browsers
  and assistive technologies using representative captioned, described,
  transcript-backed, decorative, missing-description, and long-description
  content.
- Exercise every important server-side validation refusal with assistive
  technology, including error announcement and post-submit focus behavior.
- Verify focus clearance and reflow on physical safe-area devices and at high
  browser zoom, and manually inspect forced-colours behavior where computed RGB
  contrast is not meaningful.
- Resolve any failures found by those runs and regenerate clean, revision-bound
  release evidence. Until then, Plamenu must not claim WCAG 2.2 AA conformance.

## Scope and third-party content

Plamenu-owned pages, chrome, authoring tools, local playback, and complete
processes are in scope. Authors remain responsible for the accuracy and
completeness of alternatives in their publications.

Federated content may provide only an interoperable attachment description;
this implementation's transcript, WebVTT, and audio-description metadata are
not portable across all ActivityPub software. Embedded Webxdc application
content is also separate from Plamenu's launcher, consent, session, and exit
controls. Plamenu renders available alternatives but does not infer or claim
missing ones.

Any future statement of partial conformance must identify affected third-party
content precisely and satisfy WCAG 2.2's defined conditions. It must not present
the third-party content itself as conformant.

## Completion criteria

Level AA is achieved only when every applicable WCAG 2.2 Level A and AA
criterion passes with reproducible evidence, every non-applicable criterion has
a documented rationale, all required manual and assistive-technology runs pass,
and no unresolved accessibility violation remains in the representative
sample. Scores and averages do not substitute for individual success-criterion
passes.

Preferred non-blocking AAA targets are enhanced focus visibility (2.4.12 and
2.4.13), 44-by-44 CSS-pixel primary coarse-pointer targets (2.5.5), enhanced
text contrast (1.4.6), and avoidance of non-essential motion. Plamenu does not
make a site-wide AAA claim.
