# Accessibility journey audit

This ongoing audit records completed, independently reviewable iterations
and the current work. Changes add no mandatory fields or steps to existing
workflows. This log and its single before/after gallery remain the overview;
evidence files are supporting artifacts rather than separate audit reports.

## Review handoff — 19 October 2026

Work stopped at your request after completing iteration 49 (`83f12b3`). The 49
iteration checkpoints include fixes and coverage-only work; each is a separate
local signed/signoff commit, with local signature verification recorded. Nothing
was pushed or deployed at that handoff; the subsequent staging deployment is
recorded below. Review the table and the single
[before/after gallery](media/accessibility-2026-10-19/review.html); explanations
for changes rejected under the no-additional-burden rule remain below.

The final product change focuses the existing video-import refusal explanation:
20 final language/import browser checks and five server tests passed. The same
run also contains six failures from the separate, unfixed timer investigation;
there is no claim of a passing full suite or whole-site WCAG conformance.
Server-only Clippy remains blocked by seven existing discovery/search errors.

Priority follow-ups are **A54: Chromium security-key registration policy** and
**A59: narrow video timer clipping**. The former was left unchanged because
requiring PIN/biometric verification adds a user requirement, while weakening
credential protection requires a separate security review. The latter was
reproduced just before wrap-up; its explicit `fixme` preserves the investigation
without treating the known defect as a passing regression test. Real screen-reader
speech, physical devices/security keys, full remote federation and the remaining
manual criterion matrix are still unqualified. WCAG 3 is draft guidance only.

Your unrelated changes in `docs/api/index.md` and untracked
`docs/ECOSYSTEM_FEATURE_PROPOSALS.md` were preserved outside these commits.
Raw traces, authentication secrets and downloaded account archives were not
added to the review artifacts. No further iteration is running.

## Staging deployment — 19 October 2026

At the user's request, deployed application revision `6921a5779678` to
[staging3.plamenu.com](https://staging3.plamenu.com) using `./dev deploy-staging`.
All 50 accessibility commits through that revision have locally verified
signatures and DCO signoffs. No Git push or merge was performed.

The locked musl `iteration` build completed successfully. Installed binary
SHA-256 matches the local stripped artifact:
`fb096c1841191c8b822e3042d6680d338940e00d3a5651dd8aff6f1344875943`.
The previous binary is retained at `/opt/plamenu/plamenu.previous` for rollback.
The service reports active/running with zero restarts after deployment; public
TLS requests to `/api/v1/instance`, `/`, `/login` and `/signup` returned HTTP 200.
The API reports `0.7.0-staging.317+sha.6921a5779678.dirty`; the dirty marker comes
from the preserved, unrelated documentation edit, while application sources
are committed. These deployment smoke checks do not repeat the full journey
matrix. The A54/A59 findings and other qualification limits remain open.

This documentation record is a later commit than the deployed binary.

## Iterations

| Iteration | Scope | Checkpoint |
| --- | --- | --- |
| 1 | Signup/login recovery, post actions, accessible names, common social journey | `8a0abd7` — 50 browser tests, eight targeted Rust tests and 15 harness tests passed. Signature verified locally; not pushed. |
| 2 | Email confirmation, password reset and localized authentication errors | `e88a573` — 13 browser checks, five password-reset server tests and 16 harness tests passed. Signature verified locally; not pushed. |
| 3 | Account email recovery and profile/reading-preference editing | `9ac5f9c` — 13 browser checks and 12 targeted server/localization tests passed. Signature verified locally; not pushed. |
| 4 | Profile upload error recovery | `4202dc8` — 13 browser checks and ten server/localization tests passed. Signature verified locally; not pushed. |
| 5 | Remote-follow errors and combined regression | `ff56c04` — all 96 combined browser checks and the remote-interaction server test passed. Signature verified locally; not pushed. |
| 6 | Filter-creation recovery | `fb1f86a` — seven browser checks and two server tests passed. Signature verified locally; not pushed. |
| 7 | Search heading and result navigation | `af797e3` — seven browser checks and four server tests passed. Signature verified locally; not pushed. |
| 8 | Media viewer language and keyboard continuity | `c5aabe2` — 26 browser checks and eight layout/localization tests passed. Signature verified locally; not pushed. |
| 9 | Reporting language and optional mute/unmute journey | `3187042` — seven browser checks and 11 server/localization tests passed. Signature verified locally; not pushed. |
| 10 | List creation and membership recovery | `97fcec8` — seven browser checks and 30 selected Rust tests passed. Signature verified locally; not pushed. |
| 11 | Password-change recovery and combined regression | `ce01725` — seven password journeys, six corrected logout journeys, 12 Rust tests and 16 harness tests passed. Signature verified locally; not pushed. |
| 12 | Blocking recovery and named relationship selections | `4f1318e` — seven browser journeys and 13 selected Rust tests passed. Signature verified locally; not pushed. |
| 13 | Existing list/filter edit recovery | `96530f2` — 28 browser journeys and 31 selected Rust tests passed. Signature verified locally; not pushed. |
| 14 | Privacy-domain validation recovery | `f0a0c25` — seven browser journeys and eight Rust/localization tests passed. Signature verified locally; not pushed. |
| 15 | Private messages, localized controls and narrow menu access | `eccae3d` — seven browser journeys, one additional narrow no-JavaScript journey and nine Rust tests passed. Signature verified locally; not pushed. |
| 16 | Scheduled-post validation recovery | `6983144` — seven browser journeys, six selected Rust tests and a final pagination/cancellation integration rerun passed. Signature verified locally; not pushed. |
| 17 | Opt-in two-factor enrollment and sign-in recovery | `8931327` — seven browser journeys and all ten two-factor server tests passed. Signature verified locally; not pushed. |
| 18 | Video authoring, publication and native fallback coverage | `655d052` — 15 existing browser journeys passed; coverage-only checkpoint. Signature verified locally; not pushed. |
| 19 | Group creation, posting and membership approval | `df9c0dd` — seven browser journeys and two targeted server tests passed. Signature verified locally; not pushed. |
| 20 | Event correction, contrast and attendance focus | `6b63894` — seven browser journeys and three targeted server tests passed. Signature verified locally; not pushed. |
| 21 | Webxdc creation recovery, approval and isolated app updates | `024dfb5` — seven browser journeys, five server tests and 17 harness tests passed. Signature verified locally; not pushed. |
| 22 | Declared language on private-message previews | `67a8825` — seven browser journeys and three renderer tests passed. Signature verified locally; not pushed. |
| 23 | Poll refusal recovery and results focus | `a3608bf` — seven browser journeys passed. Signature verified locally; not pushed. |
| 24 | Two-factor setup and disabling error recovery | `6927034` — 21 browser journeys and ten server tests passed. Signature verified locally; not pushed. |
| 25 | Combined regression isolation and music journey refresh | `296f77c` — 233/236 combined; 14 focused message checks, 14 unique music journeys and 19 harness tests pass. Long-name target review remains open. Signature verified locally; not pushed. |
| 26 | OAuth second-factor correction and explicit consent | `4dbfb38` — seven browser journeys and ten server tests passed. Signature verified locally; not pushed. |
| 27 | Accessible browser sign-in throttling | `3bec420` — seven browser journeys, 16 Rust checks and 19 harness tests passed. Signature verified locally; not pushed. |
| 28 | Long inbox participant targets and focus outlines | `dc59574` — 28 browser checks and three renderer tests passed. Signature verified locally; not pushed. |
| 29 | Expanded combined checkpoint | `a9a64fa` — 277/278 combined; 20 focused profile/audio checks pass after correcting a display-name assumption. Signature verified locally; not pushed. |
| 30 | Advanced music library and queue coverage | `11b5ef0` — all 14 browser cases pass. Coverage/harness checkpoint; signature verified locally. Not pushed. |
| 31 | Audio upload recovery | `8352861` — seven browser cases and three Rust checks pass. Signature verified locally; not pushed. |
| 32 | Webxdc creation language | `ae0a008` — 14 final browser cases and three Rust checks pass. Signature verified locally; not pushed. |
| 33 | Podcasts and radio access coverage | `5ff2dba` — four initial cases and all 14 full-matrix cases pass. Coverage/harness checkpoint; signature verified locally; not pushed. |
| 34 | Radio creation recovery | `34f4402` — 14 browser cases and three Rust checks pass. Signature verified locally; not pushed. |
| 35 | Live-broadcast setup coverage | `037a3c1` — 11 pass, two explicit WebKit preview skips. Coverage-only checkpoint; signature verified locally; not pushed. |
| 36 | Live publishing and replay | `482cbf3` — nine pass, four explicit WebKit skips after correcting an outdated preview assumption. Signature verified locally; not pushed. |
| 37 | Core text-spacing adaptability | `202f2df` — 14 language/browser cases pass; composer/reply actions wrap within narrow pages. Signature verified locally; not pushed. |
| 38 | Music-dock focus clearance | `abb1a71` — all 19 focus/listener cases pass after a WebKit fallback. Signature verified locally; not pushed. |
| 39 | Video alternatives lifecycle | `ea402c3` — seven cases pass after a forced-colours scan correction. Coverage/harness checkpoint; signature verified locally; not pushed. |
| 40 | Closing the music player | `bdcf52f` — all 25 close/focus/listener cases pass. Signature verified locally; not pushed. |
| 41 | Music queue in short landscape windows | `2c2f990` — 25 related cases and 12 focused landscape checks pass. Signature verified locally; not pushed. |
| 42 | CSV import/export recovery | `e43ca9a` — seven browser journeys and 16 server/catalog checks pass. Signature verified locally; not pushed. |
| 43 | Alias and migration refusal recovery | `75f377d` — seven browser journeys and 16 migration tests pass. Signature verified locally; not pushed. |
| 44 | Full account archive coverage | `32f5695` — seven browser journeys and ten archive integration tests pass. Signature verified locally; not pushed. |
| 45 | Security-key failure recovery | `7ded4ca` — all seven final enrollment/fallback cases pass; 12 WebAuthn server tests pass. Upstream enrollment policy remains unresolved. Signature verified locally; not pushed. |
| 46 | Additional common client journeys | `d69b453` — 42 browser cases pass; five explicit duplicate-survey skips. Signature verified locally; not pushed. |
| 47 | Video collaboration, playlists and history | `dcc6c0b` — seven playlists, 13 corrected channel/history cases, and one isolated WebKit history recheck pass. Signature verified locally; not pushed. |
| 48 | Existing security-key sign-in recovery | `467f1e6` — two complete Chromium existing-key journeys pass, including native fallback at both widths. Four other-engine skips are explicit. Signature verified locally; not pushed. |
| 49 | Video language, alternatives and retained-source edits | `83f12b3` — 20 final language/import browser checks and five import server tests passed; seven source-edit journeys passed in the earlier run. |

The baseline is [WCAG 2.2 Level AA](https://www.w3.org/TR/WCAG22/), with
practical AAA improvements. [WCAG 3.0](https://www.w3.org/TR/wcag-3.0/)
is a working draft (10 September 2026); its guidance informs suggestions,
not a conformance claim. Automated rule checks and browser focus evidence do
not establish screen-reader interoperability or whole-site conformance.

## Order of work

1. Vital journeys: registration, login/error recovery, reading and publishing
   posts, replies, follows, favourites, boosts, and logout.
2. Common journeys: profile/preferences, notifications, search, media,
   content warnings, polls, bookmarks, lists/filters, and account recovery.
3. Less common journeys: scheduling, account migration/security administration,
   groups, events, advanced video/music, and Webxdc.

## Environment and evidence

- Starting source revision: `a9a57424`. The checkout's actual branch is
  `feat/ap09-native-video` (the earlier local `main` note was stale). Unrelated existing
  documentation edits are preserved. No deployment or remote publication.
- Use `./dev a11y` with disposable PostgreSQL, TLS, browser containers, and
  fixture accounts. No standing accounts or data are used for mutations.
- The fixture now opens registration and sets a minimum age of 18 to exercise
  the conditional birth-date field. This is test configuration only; it does
  not introduce an age requirement for Plamenu instances.
- The first journey uses Plamenu's existing optional-email path for immediate
  account creation. Iteration 2 adds actual confirmation/reset email delivery to a bounded local SMTP sink; external delivery providers remain untested.
- Browser tooling: Playwright 1.63.0 and axe 4.13.0; pinned engines Chromium
  153.0.8010.12, Firefox 155.0, WebKit 26.6. Mobile means an emulated/narrow
  viewport, not physical-device testing. The exact browser image and executable
  hashes are preserved in the evidence files below.
- Findings distinguish reproduced failures, source-review candidates,
  improvements, and untested areas. A criterion reference indicates relevance,
  not necessarily a conclusively established failure of that criterion.

## Findings

| ID | Journey and discovery | Standard and effect | State |
| --- | --- | --- | --- |
| A01 | Existing public-page test visits `/signup` while the fixture defaults to closed registration. | Coverage gap: the real form and proof-of-work widget were not exercised by that check. | Fixed in disposable fixture. |
| A02 | A password mismatch clears an already valid birth date and accepted rules. | [3.3.7 Redundant Entry](https://www.w3.org/WAI/WCAG22/Understanding/redundant-entry.html): unrelated errors require entering valid information again. | Reproduced on desktop and 320px; values now retained. |
| A03 | Signup focuses Username even for a password-confirmation error; failed login also skips its error message. | 2.4.3, [3.3.1 Error Identification](https://www.w3.org/WAI/WCAG22/Understanding/error-identification.html): improve error discovery and recovery; existing text and field associations were already present. This is not a claim that WCAG mandates summary focus. | Reproduced; existing error summary receives native focus, including without JavaScript. |
| A04 | Failed login clears the entered identifier. | Reduce repeat typing during authentication recovery, supporting 3.3.7. Password clearing is retained under the security exception. | Reproduced; identifier now retained and escaped by the HTML renderer. |
| A05 | A failed favourite request silently reverts; temporary disabling also drops keyboard focus. | Error-feedback improvement and 2.4.3 focus continuity. The new message exposes `role="alert"` for [4.1.3 Status Messages](https://www.w3.org/WAI/WCAG22/Understanding/status-messages.html). | Reproduced using HTTP 503; localized explanation and focus recovery added to shared favourite/bookmark/boost/vote handler. |
| A06 | The signup rules link is distinguished from surrounding text by colour alone. | 1.4.1 Use of Color; axe `link-in-text-block` failure. | Reproduced; link is persistently underlined. |
| A07 | `role="alert"` on the signup `<ul>` replaces its list role, leaving orphan list items in the accessibility tree. | 1.3.1 Info and Relationships; axe `listitem` failure. | Reproduced; alert wraps a semantic list. |
| A08 | Signup requires an ALTCHA proof produced by JavaScript. | A no-JavaScript signup route is not established by this work. No-JavaScript support is not itself a WCAG criterion; the widget still needs assistive-technology testing. | Source boundary; anti-abuse design retained, not silently bypassed in tests. |
| A09 | Counted reply/quote links and boost/favourite buttons are exposed as just “0” in the browser accessibility tree. | 2.4.4 Link Purpose and 4.1.2 Name, Role, Value; automated scans accept a numeric name despite its missing purpose. | Reproduced in baseline accessibility snapshot; explicit action-and-count names added for signed-in and anonymous controls, with synchronized names on toggle and rollback. No visual change. |
| A10 | The existing lightbox test tries to fill an attachment description inside a collapsed disclosure. | Regression coverage gap after the existing optional-description UI changed. | Test now opens the native disclosure before entering the description. |
| A11 | During the first long matrix, open registration reverted to approval mode because the fixture had no active moderator. | Fixture realism: this is the existing inactive-moderator safeguard, not a mobile signup defect. | Added an active fixture moderator; ordinary journey accounts stay unprivileged and the production safeguard stays enabled. |
| A12 | A password-reset mismatch loses list semantics and focuses the new-password field instead of its error. | 1.3.1, 2.4.3, 3.3.1; same alert/list issue as signup. | Reproduced through a real confirmation/reset email journey on desktop and 320px. Native summary focus and semantic list repair passed all six engine/layout combinations and no-JavaScript Chromium. |
| A13 | A Russian login form displays English credential errors without marking the language change. | 3.1.2 Language of Parts and understandable authentication recovery. | Reproduced in Russian Chromium. Six first-party login failure messages moved into the existing English/Russian catalogs; protocol error codes unchanged. Russian credential errors passed all six engine/layout combinations. |
| A14 | Wrong-password and duplicate-email account edits discard the proposed address and leave focus away from the error. | 3.3.7, 2.4.3 and 3.3.1: preserve valid work and make correction easier. | Reproduced on desktop and 320px; retained address and native error focus passed all six browser/layout combinations and no-JavaScript Chromium. |
| A15 | An unreadable profile image replaces the editor with a raw JSON response. | 3.3.1/3.3.3 and 3.3.7: the error lacks the normal page/form context and recovery loses typed work. | Reproduced on desktop and 320px; editor retention, focused localized summary and explicitly marked English validation details passed in all six engine/layout combinations, plus native no-JavaScript recovery. |
| A16 | Remote-follow handle errors replace list semantics with an alert role and focus the input past the error. | 1.3.1, 2.4.3 and 3.3.1; axe reports orphan list items. | Reproduced on desktop and 320px; fixed; all six scripted engine/layout combinations and native no-JavaScript Chromium passed in the combined matrix. |
| A17 | Creating a filter without a selected context loses its title, action, expiry, keyword and whole-word choice. | 3.3.7, 2.4.3 and 3.3.1: retain work after the existing context requirement rejects submission. | Reproduced on desktop and 320px; form retention and native error focus passed all six scripted engine/layout combinations and native no-JavaScript Chromium. The requirement itself is unchanged. |
| A18 | Search has no level-one page heading in either initial or empty-result states. | Heading-navigation improvement related to 1.3.1/2.4.6; a missing `h1` alone is not automatically a WCAG failure. | Reproduced on desktop and 320px. Result navigation and query retention worked; a visually hidden localized heading now supports heading navigation in all seven browser configurations. |
| A19 | The Russian media viewer uses English dialog/control names and expansion text without marking a language change. | 3.1.2 and understandable control names. | Reproduced through keyboard upload and image viewing on desktop and 320px; localized labels and expansion text passed in all six scripted browser/layout combinations. |
| A20 | A media navigation or description control can become hidden while focused when switching images. | 2.4.3/2.4.7 focus continuity. | Reproduced for Next, Previous and description expansion on desktop and 320px. Focus recovery to a remaining visible control passed in all six scripted combinations. |
| A21 | An expanded long media description scrolls at 320px but cannot receive keyboard focus. | 2.1.1 Keyboard; axe `scrollable-region-focusable`. | Reproduced in all three narrow browser layouts. Conditional keyboard focus, native Arrow-key scrolling and a localized region name passed; short descriptions stay outside the tab order, including after resize. |
| A22 | Reporting forms and confirmation use English first-party text inside the Russian interface. | 3.1.2 Language of Parts and understandable reporting. | Reproduced on desktop and 320px. Submission and optional mute/unmute completed; localized form/confirmation copy passed in all six scripted engine/layout combinations; native reporting/mute recovery also passed. Excerpts expose their existing language metadata when available. |
| A23 | A whitespace-only list title resets the selected replies policy and exclusivity after rejection. | 3.3.7 and 3.3.1: retain valid choices and associate the invalid title with the error. | Reproduced on desktop and 320px; retained settings, title/error association and focused error passed in all seven browser configurations. |
| A24 | Adding an account before following it clears the entered handle and leaves focus away from the error. | 3.3.7 and 2.4.3: preserve the handle and make correction discoverable while retaining the existing follow requirement. | Reproduced on desktop and 320px; retained handle, error association and focused error passed in all seven browser configurations. |
| A25 | Password-change errors leave focus away from the message and do not identify the affected field. | 2.4.3, 3.3.1 and 3.3.3: improve error discovery and correction. | Reproduced at both widths; focused error and current/new/confirmation associations added without changing password policy. All seven browser configurations passed correction, session revocation and sign-in recovery. |
| A26 | Relationship-manager checkboxes have empty labels, so selecting an account for unblocking has no accessible name. The same renderer pattern is used for domains and hashtags. | 1.3.1 and 4.1.2; axe `label` failure. | Reproduced for blocked accounts on desktop and 320px. Account/domain/hashtag selections now have localized target-specific accessible names with no visual change. Seven browser journeys and English/Russian renderer checks passed. |
| A27 | A rejected edit to an existing list restores persisted values and loses the draft policy/exclusivity. | 3.3.7, 3.3.1 and 2.4.3: retain edits and provide a useful recovery position. | Reproduced at both widths; draft retention, focused error, title association, correction and persisted success passed in all seven browser configurations. |
| A28 | A rejected edit to an existing filter loses its draft settings and existing/new keyword changes. | 3.3.7, 3.3.1 and 2.4.3. | Reproduced at both widths; title/action/expiry/context and keyword drafts now remain editable after validation refusal. Correction and persisted success passed in all seven browser configurations. |
| A29 | An invalid attribution domain replaces privacy settings with raw JSON, losing the normal recovery form. | 3.3.1, 3.3.3 and 3.3.7: retain valid choices and explain correction in context. | Reproduced at both widths; the form now retains raw domain text, seven flags and all six notification policies with a focused summary and domain-error association. All seven browser configurations passed. |
| A30 | Private-message read/unread, removal and menu names remain English in the Russian interface; participant summaries and deleted-message fallback are also hard-coded. | 3.1.2 and understandable controls. | Reproduced at both widths; first-party inbox row text now uses the existing catalogs. English wording and action behavior stay the same. |
| A31 | An open inbox menu at 320px covers most of the conversation link, leaving an undersized clickable strip. | 2.5.8 Target Size (Minimum), axe `target-size`; also harms focus visibility. | Reproduced with the populated inbox. At narrow widths the existing menu now expands inside its card, leaving the link visible below it. Geometry, Tab access and axe pass in all three narrow engines and a native no-JavaScript check. |
| A32 | A rejected reschedule replaces the proposed time with the stored time, leaves focus away from the message, and does not associate the field with its error. | 2.4.3, 3.3.1 and 3.3.3 recovery improvement. Retaining the invalid field itself is a usability improvement, not a separate claim of 3.3.7 failure. | Reproduced at both widths; parseable rejected times are retained, the English-marked error receives focus, and the field is associated with it. Rescheduling preserves the queue page; all seven browser journeys passed. |
| A33 | An incorrect or consumed two-factor code focuses the empty code input past the existing error. | Error-discovery improvement relevant to 2.4.3/3.3.1; summary focus is not a WCAG mandate. | Reproduced on desktop and 320px. Existing error focus and fresh-prompt input focus passed in all seven browser configurations; enrollment, one-time recovery and disabling also completed. |
| A34 | A taken group name clears the display name and resets membership/posting policies, leaving focus away from the error. | 3.3.7, 3.3.1 and 2.4.3 recovery. | Reproduced at both widths; all four draft fields are retained, the existing error is focused, and address/display-name refusals identify the relevant field. Seven browser journeys passed through creation, posting and membership approval. |
| A35 | The full composer's validation banner fails light-theme contrast and does not receive focus after an event-date error. | 1.4.3 Contrast (Minimum); 2.4.3/3.3.1 error-discovery improvement. | Reproduced at both widths with axe. Reusing the established error-text token raises token-derived contrast from about 4.27:1 to 5.48:1; the existing banner receives focus. All seven event journeys passed. |
| A36 | Attendance requests lose keyboard focus when the initiating button is disabled or replaced by the refreshed RSVP controls. | 2.4.3/2.4.7 focus continuity. | Reproduced on failure, attendance and withdrawal. Conditional focus recovery passed all six scripted configurations; a delayed refusal does not steal focus from a different field. |
| A37 | Updated attendance explanations have no status-message semantics. | 4.1.3 Status Messages: expose the existing going/pending/refused/invited/closed text programmatically. | Existing text now has `role="status"`; successful-attendance semantics have browser/server assertions. Actual screen-reader speech remains unverified. |
| A38 | A rejected Webxdc package leaves keyboard focus away from the existing upload error. | 2.4.3/3.3.1 error-discovery improvement; summary focus is not a WCAG mandate. | Reproduced at both widths; the existing error now receives native focus. Seven browser journeys passed through correction, creation, approval and closure; scripted cases also exchange an app update. |
| A39 | Private-message previews omit the declared language even when the message body is marked correctly. | 3.1.2 Language of Parts: expose the available language of preview text and content warnings. | Reproduced for French text and content warnings in an English inbox; all seven browser configurations now preserve the declaration while interface labels retain their own language. |
| A40 | The separate poll-voting script loses focus when it disables the Vote button or replaces the form with results. | 2.4.3/2.4.7 focus continuity. | Reproduced on failure and success at both widths; conditional button/results focus passes all six scripted configurations. Delayed failure and success preserve focus moved elsewhere; native no-JavaScript voting also passes. |
| A41 | Two-factor setup/disabling errors leave focus away from the message; a rejected setup code does not identify its invalid field. | 2.4.3/3.3.1/3.3.3 recovery improvement. | Reproduced at both widths. Recognized security-page errors receive focus and rejected setup codes identify the code input. All seven configurations pass setup correction; 14 related password/sign-in regression journeys also pass. |
| A42 | The accumulated journeys overload shared fixture login budgets, and the resulting browser response is raw JSON “Too many requests”. | Separate issues: unrealistic client sharing is a coverage problem; the HTML authentication error response lacks normal recovery context (3.3.1/3.3.3 improvement candidate). | Bounded independent fixture clients and cached participant sessions resolve the accumulated login failures. Production throttling remains enabled. Iteration 27 adds localized, focused HTML recovery for browser POSTs to `/login` and `/login/challenge`, preserving HTTP 429 and JSON clients. Seven browser cases pass; OAuth throttle presentation remains outside this change. |
| A43 | A long participant name in a narrow inbox produces an axe target-size flag on its clipped inline link. | 2.5.8 Target Size (Minimum), with focus-visibility improvements relevant to 2.4.7. | Reproduced with valid handles and long display names. Existing links now have 24px minimum dimensions, independent truncation and room for focus outlines. All 28 single/two-name and related message checks pass; shortening fixture handles was not its fix. |
| A44 | A rejected OAuth second-factor code focuses the empty input past the error. | 2.4.3/3.3.1 error-discovery improvement, consistent with web sign-in recovery. | Reproduced at both widths; existing error focus passes all seven configurations through correction, local PKCE token exchange and explicit allow/deny consent. |
| A45 | A rejected audio upload discards the optional title, artist and album and leaves the user on a separate problem page. | 3.3.7 Redundant Entry; 3.3.1/3.3.3 error recovery, with useful focus placement. | Reproduced at both widths; retained metadata, focused explanation and correction through publication/playback pass all seven configurations. |
| A46 | Webxdc creation presents English labels and dynamically updated choice text under the Russian page language. | 3.1.2 Language of Parts; comprehensibility in the chosen interface language. | Creation controls now use the catalogs; English backend errors are marked explicitly. All 14 localized/existing Webxdc cases pass; other Webxdc screens remain pending. |
| A47 | A space-only radio-station name returns raw JSON and discards the description, visibility and configuration. | 3.3.1/3.3.3 error identification and suggestion; 3.3.7 Redundant Entry. | Reproduced at both widths; retained native form and localized name guidance pass seven recovery cases plus seven existing station journeys. |
| A48 | Russian composer/reply action rows overflow a 320px viewport; wider text spacing increases the overflow. | 1.4.10 Reflow and 1.4.12 Text Spacing adaptability. | Existing actions now wrap when needed, preserving labels and order. All 14 English/Russian cases pass, with default and spaced layouts represented in the gallery. |
| A49 | The music dock partly covers focused fields/account controls; its expanded queue can hide them completely. | 2.4.11/2.4.12 focus-visibility improvement; tests require complete visibility, stronger than the AA minimum. User-opened-content exceptions are not exhaustively adjudicated. | Actual-height clearance, a shorter scrollable sidebar and conditional page focus scrolling pass all 19 selected cases without requiring the player to close. |
| A50 | Closing the focused music player leaves focus on the page body instead of the unfinished draft. | 2.4.3 focus-continuity improvement; no specific restoration target is claimed as a WCAG mandate. | Return to the last available page target passes six close journeys, with 19 related focus/listener cases also passing. |
| A51 | The expanded music queue consumes the usable height in short landscape windows and covers focused page fields. | 2.4.11/2.4.12 focus-visibility improvement; this is not an orientation-lock finding. | The existing queue scrollport now sizes around its controls. Twelve 800×360/800×320 cases pass; 25 related cases also pass on the same executable. |
| A52 | Refused CSV uploads reset the chosen import type and merge/overwrite mode. | 3.3.7 Redundant Entry; error recovery and 2.4.3 focus continuity. | Validated choices survive completed upload refusals, with a localized focused explanation. Seven complete correction/review/confirm/export journeys pass. |
| A53 | Alias refusals and migration password errors discard the handle and leave focus away from the explanation. | 3.3.7 for valid handles after password refusal; 2.4.3 focus continuity and error-recovery usability. Invalid-handle retention is an additional convenience. | Trimmed handles are retained and errors receive native focus. Seven browser journeys and 16 integration checks pass; remote browser interoperability remains unqualified. |
| A54 | Chromium rejects the current security-key registration options before creating a credential. | Functional access blocker; not by itself a proven 3.3.8 failure while other sign-in methods remain. | Unresolved upstream policy mismatch. Requiring PIN/biometric verification would add a requirement; weakening protection is not an accessibility-only fix. |
| A55 | A failed security-key enrollment request drops keyboard focus when its button is re-enabled. | 2.4.3 focus continuity; asynchronous error recovery. | Conditional restoration passes all six scripted profiles, including preserving a reader's changed focus; native fallback also passes. |
| A56 | No-JavaScript settings show an inert security-key enrollment form because grid styling overrides hidden. | Native-fallback operability/usability; WCAG does not require WebAuthn to function without JavaScript. | Fixed for enrollment: honor hidden and retain the existing explanation. Desktop/mobile native pairs reviewed; native recovery-code sign-in passes. The equivalent sign-in-control issue is tracked in iteration 48. |
| A57 | No-JavaScript second-factor pages expose an inert security-key sign-in button despite hidden markup. | Native fallback operability/usability; the available code form remains the supported path. | Fixed: native key control honors hidden. Both Chromium-width journeys complete recovery-code sign-in with an existing-key fixture. |
| A58 | A temporary security-key sign-in failure drops focus when its button is re-enabled. | 2.4.3 focus continuity and asynchronous recovery. | Fixed: both Chromium-width journeys restore focus after refusal, preserve changed focus during delayed failure, then authenticate and remove the virtual key. |
| A59 | Narrow video controls visibly ellipsize the current-time/duration display. | 1.4.10 reflow / 1.4.12 text-spacing concern: full time remains in the seek slider aria-valuetext, but the visible clock clips. | Reproduced in all six scripted profiles at 320px, with and without text spacing. The 22 October repair preserves the full clock and 44px controls; all six focused profiles passed. Historical failed baseline evidence is retained below; full-gate qualification is separate. |
| A60 | Refused video-source imports retain text but do not focus the visible explanation. | 2.4.3 focus continuity and error-recovery usability. | Reproduced in all seven browser profiles after fixing channel prerequisites. Fixed: native error-summary focus passes all seven profiles; source fetching and publication rules unchanged. |

## Visual changes for review

[Open the before/after gallery](media/accessibility-2026-10-19/review.html).
Visual groups include original captures at 1440×900 and 320×800, plus identified
800×360/800×320 landscape comparisons.
Description scrolling shows the narrow-width pair where overflow occurs;
nonvisual changes are identified separately. Two-factor setup comparisons use
explicit error-only crops to exclude QR codes and setup keys. Long-name inbox
comparisons use crops of the affected conversation card.

1. **Signup recovery:** the error summary receives the existing focus outline,
   the valid date and accepted rules remain filled, and the rules link is
   underlined. Fresh forms still start empty and unchecked. The alert/list
   semantics change has no additional visual effect.
2. **Login recovery:** the existing error receives focus and the identifier
   remains filled; the password stays empty. No new controls.
3. **Failed actions:** a localized alert appears below the action row only
   after a failure. Focus returns to the initiating button if disabling it
   lost focus. It is not taken back from another control. Retrying clears
   the message; successful actions add no permanent UI.

4. **Password reset:** the existing error summary receives focus and its outline;
   password fields remain blank. The list retains its semantics.
5. **Russian login:** error wording follows the interface language. No controls
   or steps are added; English wording stays the same.
6. **Account settings:** failed email changes retain the proposed address and
   focus the existing error. The normal editor and successful flow stay familiar.
7. **Profile uploads:** the raw JSON error is replaced with the familiar editor,
   a focused error and retry guidance, with text and choices retained.
8. **Remote follow:** the existing handle error receives a focus outline. Its
   list semantics are repaired without changing the handle-entry workflow.
9. **Filter creation:** a rejected submission keeps the entered values and selected
   choices, with a focus outline on the existing error. No additional controls.
10. **Media viewer:** Russian expansion text and control names follow the interface
    language; focus stays visible when navigation hides a control. A long expanded
    description gains a focus outline when reached for keyboard scrolling.
11. **Reporting:** the Russian report form and confirmation now use Russian
    first-party copy, including category guidance and optional actions. Fields
    and submission steps are unchanged.
12. **Lists:** creation errors keep selected policy/exclusivity, and membership
    errors keep the entered handle. Existing messages receive focus; the invalid
    title and member input gain explicit error associations.

13. **Password changes:** incorrect-current-password and mismatch errors gain the
    existing focus outline and identify the affected field. Passwords stay blank.
    Pending authenticator setup does not compete for focus on these errors.

14. **Existing list/filter edits:** a rejected edit keeps draft settings and
    keyword changes, with the existing error focused. List title errors are
    associated with the field. Successful forms and validation requirements stay
    the same.

15. **Privacy settings:** invalid attribution domains now return the familiar
    form and a focused explanation, preserving text and choices. The raw JSON
    baseline lacks viewport metadata, so the mobile browser uses its 980px
    fallback; that original capture is preserved alongside the revised 320px form.

16. **Private messages:** Russian inbox actions and participant summaries use
    Russian copy. At widths up to 40rem the existing menu expands within the card
    so the preview link remains visible; its trigger stays at the top right.
    Desktop menus keep their floating placement. No new controls or steps.

17. **Scheduled posts:** the rejected time remains in the existing field,
    associated with the focused error. The entry heading continues to show the
    stored publication time until a valid correction is saved. No new controls.

18. **Two-factor sign-in:** the existing error gains the focus outline after
    an incorrect or consumed code. Fresh prompts retain input autofocus. Setup
    keys and recovery codes are absent from the review screenshots.

19. **Group creation:** rejected names keep the address, display name and both
    policy choices. The existing error gains a focus outline and field association;
    no new requirement or control is added.

20. **Events and attendance:** the shared composer error text is darker in the
    light theme and its full-composer banner receives focus. Attendance failure
    restores the existing button's focus outline when needed; a refreshed RSVP
    keeps focus on the available action. Status-message roles add no visible UI.

21. **App-session creation:** the existing package-upload error gains a focus
    outline. Session details and advanced choices were already retained and
    remain editable. No new package requirement or control is added.

22. **Poll voting:** a refused vote restores the existing Vote button’s focus
    outline when needed. Successful results gain a focus outline when replacing
    the active form; they remain outside ordinary Tab navigation. Focus elsewhere
    is preserved. Both states have desktop and narrow screenshot pairs.

23. **Two-factor setup:** the existing error gains a focus outline after a
    refused password or setup code. A rejected code is associated with that
    error. Cropped desktop/narrow pairs exclude the QR and setup key; fresh
    setup keeps its existing code-entry focus.

24. **OAuth challenge:** the existing code error receives the focus outline;
    fresh prompts retain input focus. Desktop/narrow pairs show that change
    without exposing setup keys, recovery codes or client credentials.

25. **Sign-in throttling:** the raw JSON response becomes a localized page
    with a heading, focused explanation and return link for browser sign-in.
    The active limit and HTTP 429 stay in force. Desktop/narrow pairs include
    the old raw response’s wider viewport capture.

26. **Inbox participant names:** the existing links have larger hit areas and
    visible focus outlines. Two names share the available width and truncate
    independently. Card crops show single-name and two-name comparisons at
    desktop/narrow widths; no new control or step is introduced.

Action-and-count accessible names have **no visual effect**. The baseline
browser tree exposed counted links/buttons as “0”; revised names express
purpose and count, such as “Reply (0)” and “Remove favourite (1)”, including
anonymous remote-interaction links and the no-JavaScript path.

Relationship-selection labels have **no visual effect**: the existing checkboxes
are named, for example, “Select @developer”, “Select example.test” and
“Select #accessibility”. No selection step or confirmation is added.

## Iteration 1 verified coverage

| Journey/check | Result and limits |
| --- | --- |
| Signup → password mismatch → correction → home → logout → incorrect login → correction → first post | Passed in all six scripted browser/layout combinations. Valid date, agreement, username and time zone survive signup errors; passwords remain blank. Existing ALTCHA proof runs normally. |
| Favourite/bookmark/boost | Success, HTTP 503, dropped connection, retry, persistent state and useful accessible names checked. A delayed failure does not steal focus after navigation to another control. |
| Anonymous post reading | Reply/quote/boost/favourite names include their purpose and count. |
| Follow → home reading → content warning → poll vote → reply → notification → unfollow | Passed in all six scripted combinations, including real local multi-account interaction. Replies inherit and retain the parent warning. This is local interaction, not a remote federation interoperability test. |
| Public and authenticated page samples | Axe and document-structure checks on nine public routes and eight authenticated routes. Includes settings/profile/account/security pages and the Webxdc launcher; visiting these pages does not establish complete coverage of their workflows. |
| Adaptation | Existing checks cover 320px reflow, skip-link focus, Russian sign-in, dark theme, reduced motion and forced colours. Forced-colours RGB contrast is deliberately not scored by axe. Lightbox targets and icon alignment tested after entering optional descriptions. |
| No JavaScript | Chromium login-error recovery, posting, favourite/unfavourite and public document/form contracts passed. No claim of no-JavaScript signup. |

Final matrix: **50 passed, 0 failed, 0 skipped**: eight tests each for Chromium,
Firefox and WebKit at desktop and mobile widths, plus two no-JavaScript
Chromium tests. This is the focused core/representative suite, not the complete
advanced-feature suite or a WCAG conformance certification.

Additional checks: locked application build, eight targeted registration and
localization Rust tests, 15 accessibility harness Python tests, Rust formatting,
JavaScript/Python syntax, Git whitespace checks, and the 56-entry WCAG A/AA
criterion inventory passed. Python 3.14 emitted existing HTTPError-cleanup
ResourceWarnings in two harness scenarios; the tests passed.

**Clippy remains blocked by existing discovery-code diagnostics.** The normal
strict run found eight errors in unchanged database/federation discovery files;
the server-only run found seven in unchanged discovery/search files. Neither
run reported a diagnostic in this patch's changed files. These lint failures
are not waived or fixed by this accessibility batch. Logs:
[dependencies](media/accessibility-2026-10-19/clippy-dependencies.txt),
[server](media/accessibility-2026-10-19/clippy-server.txt).

Evidence: [baseline failure report](media/accessibility-2026-10-19/baseline.json),
[passing targeted report](media/accessibility-2026-10-19/verified.json), and
[executable/source hashes](media/accessibility-2026-10-19/verification-manifest.json).
Reports intentionally retain `source_dirty: true`: these are uncommitted local
changes on top of the recorded base, not release qualification. Raw browser
traces containing disposable session/form data remain outside the review files.

Reproduce from the repository root:

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/core-review.json \
  ./dev a11y core-journey.spec.mjs representative.spec.mjs \
  --output=../target/accessibility/core-review-results
./dev test -p plamenu --lib --no-fail-fast \
  -E 'test(web::register::tests::) or test(web::i18n::tests::)'
python3 -m unittest discover -s scripts/tests -p 'test_accessibility_*.py'
./dev a11y-matrix
```

The dedicated output directory avoids deleting older, differently owned test
results. The offline-network fixture is omitted because these tests inject
bounded request failures rather than qualifying service-worker offline behavior.

## Iteration 2 verified coverage

The recovery suite passed **13 tests**: confirmation email → confirmed account →
login → reset email → mismatch → correction → old-session revocation → consumed
link rejection → login with the new password, plus Russian credential-error
rendering in all six engine/layout combinations. Chromium also completed reset
error recovery with JavaScript disabled, including Tab from the summary to the
password field. Axe found no violations in the scanned reset-error, consumed-link
and Russian-login states. This is local SMTP delivery, not external provider or
remote-mailbox interoperability.

A bounded, nonce-protected read-only inbox in the disposable harness receives the
application's actual SMTP messages. Tokens and session traces are kept out of the
review artifacts. Its unit test checks real SMTP receipt, recipient isolation and
inbox access rejection; all 16 accessibility-harness tests passed. Five existing password-reset server
tests also passed, covering revocation, token validation, account-enumeration
resistance, absent SMTP and Russian pages/mail. Formatting, syntax and Git
whitespace checks passed.

Evidence: [baseline failures](media/accessibility-2026-10-19/recovery-baseline.json),
[passing matrix](media/accessibility-2026-10-19/recovery-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/recovery-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/recovery-review.json \
  ./dev a11y recovery-journey.spec.mjs \
  --output=../target/accessibility/recovery-review-results
./dev test -p plamenu --test password_reset --no-fail-fast
python3 -m unittest discover -s scripts/tests -p 'test_accessibility_runtime.py'
```

## Iteration 3 verified coverage

Account edits now render validation failures in the existing form with HTTP 422,
retaining the proposed address in escaped HTML. Neither the address nor passwords
are added to a redirect URL; passwords are never reflected. Successful changes
still redirect. The focused account error does not add a tab stop.

The browser journey checks wrong-password rejection, correction, duplicate-email
rejection, restoration of the original address, summary focus and Tab to the
email input. The final matrix passed **13 tests**, including native no-JavaScript
recovery; axe runs only in the six scripted browser/layout combinations.
Separate journeys save and restore profile text and reading
preferences/time zone. The server selection covers profile forms, stored Russian
locale, profile saving/CSRF, preferences, email conflict/removal, account deletion
and the five existing locale tests: **12 passed**.

Evidence: [baseline](media/accessibility-2026-10-19/settings-baseline.json),
[verified matrix](media/accessibility-2026-10-19/settings-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/settings-verification-manifest.json).
An initial preference test incorrectly selected a hidden checkbox fallback too;
the selector now targets the visible checkbox. An intermediate baseline attempt
was rejected by the harness because the concurrent Rust build replaced its
binary. The retained baseline was rerun against a stable executable; subsequent
builds and browser matrices are serialized or use a frozen baseline copy.
The first no-JavaScript check stalled inside axe; the harness now runs axe only
with JavaScript enabled and retains explicit native form/focus assertions in
the no-JavaScript journey.

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/settings-review.json \
  ./dev a11y settings-journey.spec.mjs \
  --output=../target/accessibility/settings-review-results
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(settings_account_) or test(settings_profile_) or test(settings_preferences_) or test(web::i18n::tests::)'
```

## Iteration 4 verified coverage

Unreadable avatar/header uploads now return the existing editor with a focused
error, retained display name, biography, image descriptions, metadata and account
choices. The rendering-only draft is not written to the account. The server keeps
its original error status and logging; normal successful submissions still redirect.

The new summary and correction hint use the interface language. Detailed messages
from the shared validation layer are still English, explicitly marked `lang="en"`;
translating that layer remains an improvement opportunity. Uploads are optional:
a user can save retained text without reselecting files. Native file controls
cannot be repopulated in a returned page; the hint explains that images need to
be selected again if the user wants to retry them. No confirmation step or
mandatory image description was added.

Ten targeted server/localization tests passed. The new server regression covers
both avatar and header rejection, retained text and checkbox values, escaping of
HTML-like biography text, and unchanged stored profile data. Existing profile
save, form, Russian-locale, CSRF and catalog tests also pass.

The final browser matrix passed **13 tests**, with no axe violations in the
scripted error states. The browser journey tests English and Russian error recovery, changes the saved
interface language through the existing keyboard picker, restores English, and
saves retained text without an image. No-JavaScript Chromium retains direct
native focus/form assertions. Its fixture name stays within the existing
30-character limit; that limit and saved-language precedence were fixture
corrections during test development, not application defects.

Evidence: [baseline](media/accessibility-2026-10-19/profile-upload-baseline.json),
[passing matrix](media/accessibility-2026-10-19/profile-upload-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/profile-upload-verification-manifest.json).
A fresh server Clippy run still reports the same seven diagnostics in unchanged
discovery/search code, none in these changes: [current log](media/accessibility-2026-10-19/clippy-server-current.txt).

Malformed multipart requests and uploads rejected by request/body-size limits
still use their existing early rejection paths. Retaining partial forms across
those paths, and staging optional image bytes for retry, are future work beyond
this bounded iteration; they are not claimed fixed or rejected by the user's
no-additional-burden rule.

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/profile-review.json \
  ./dev a11y profile-upload-journey.spec.mjs \
  --output=../target/accessibility/profile-review-results
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(settings_profile_) or test(web::i18n::tests::)'
```

## Iteration 5 verified coverage

Remote-follow errors now wrap the semantic list in a focused alert. The handle
and target URI were already retained and remain intact; Tab moves from the
summary into the existing handle field. Fresh forms still focus that field.
The same markup covers malformed handles, unavailable home servers and absent
remote-interaction endpoints without new controls.

The browser journey starts from the anonymous profile Follow link, submits an
invalid handle, checks list semantics/focus and retained subject, then corrects
to a local handle and reaches sign-in. The existing server integration test
passed its malformed-handle, local-home, remote subscribe-template substitution,
unsupported-endpoint and signed-in resolution branches using a federation stub.
This does not claim live interoperability with external servers.

The combined matrix passed **96 tests, 0 failed, 0 skipped** against the
iteration 5 executable: 15 tests in each of six engine/layout combinations and
six native no-JavaScript tests. This rechecks the completed signup, social,
representative, recovery, settings, profile-upload and remote-follow journeys
together. Later filter/search candidate work is excluded from that binary and
from this checkpoint's changes.

Evidence: [combined passing matrix](media/accessibility-2026-10-19/combined-verified.json),
[remote-follow baseline](media/accessibility-2026-10-19/remote-interaction-baseline.json),
and [source/executable hashes](media/accessibility-2026-10-19/remote-interaction-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/combined-review.json \
  ./dev a11y core-journey.spec.mjs representative.spec.mjs \
  recovery-journey.spec.mjs settings-journey.spec.mjs \
  profile-upload-journey.spec.mjs remote-interaction-journey.spec.mjs \
  --output=../target/accessibility/combined-review-results
./dev test -p plamenu --test web --no-fail-fast \
  -E 'test(remote_interaction_interstitial_flow)'
```

## Iteration 6 verified coverage

Rejected filter creation now returns the existing form with HTTP 422, its error
focused, and the title, action, expiry, context choices, optional keyword and
whole-word choice retained. Values stay in escaped HTML, not a redirect URL.
The existing requirement to select at least one context and the account limit
remain enforced; successful creation still redirects to the editor.

**Seven browser checks passed** across all six scripted engine/layout combinations
and no-JavaScript Chromium. Each corrects the missing context, creates the filter,
checks its saved settings and keyword, edits its title and finds it in the list.
Axe found no violations in the scripted error states. **Two server tests passed**:
CRUD plus the account-cap rejection, extended to check retained/escaped values
and no extra persisted filter at the cap. Formatting, syntax and whitespace checks
passed. This iteration covers creation errors; existing-filter edit-error
retention remains a source-review candidate, not a claimed fix.

Evidence: [baseline](media/accessibility-2026-10-19/filter-baseline.json),
[passing matrix](media/accessibility-2026-10-19/filter-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/filter-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/filter-review.json \
  ./dev a11y filter-journey.spec.mjs \
  --output=../target/accessibility/filter-review-results
./dev test -p plamenu --test web --no-fail-fast -E 'test(settings_filters_)'
```

## Iteration 7 verified coverage

Search now has a localized level-one heading using the existing visually-hidden
style. The normal layout and focus behavior are unchanged. This is a structural
navigation improvement, not a claim that WCAG requires an `h1` on every page.

**Seven browser checks passed**: initial heading and search focus, a people query,
opening the result profile, an empty query-result state and retention of the
query text, including no-JavaScript Chromium. Axe found no violations in the
scripted empty-result state. **Four server tests passed** for people/hashtag
search, URL resolution, anonymous access and Russian negotiation. Syntax,
formatting and whitespace checks passed.

Evidence: [baseline missing-heading assertions](media/accessibility-2026-10-19/search-baseline.json),
[passing matrix](media/accessibility-2026-10-19/search-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/search-verification-manifest.json).
No screenshot pair is needed because this change has no visual effect.

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/search-review.json \
  ./dev a11y search-journey.spec.mjs \
  --output=../target/accessibility/search-review-results
./dev test -p plamenu --test web --no-fail-fast \
  -E 'test(anonymous_search_) or test(search_finds_) or test(search_resolves_) or test(search_page_)'
```

## Iteration 8 verified coverage

The media viewer now uses the document's English/Russian labels for its dialog,
controls, expansion text and post link. Both full pages and focused/embed shells
supply these inert labels; JavaScript assigns text safely rather than interpolating
translations into HTML.

When navigation hides the focused Next/Previous button, focus moves to the
remaining navigation button. If a shorter description removes its expansion
control or scrollable region, focus moves to Close. Focus elsewhere is left
alone. Escape continues to return focus to the original image link.

The first matrix also exposed a real narrow-screen defect: expanded descriptions
could scroll but could not receive keyboard focus. An overflowing expanded
description now gets one named focus target in the existing dialog's tab cycle;
Arrow keys can scroll it. Collapsed/short descriptions remain outside the tab
order. The condition updates on viewport resize as well. This adds access to
existing text, without adding controls or asking authors for more information.

The new journey uses keyboard activation of the actual file chooser, uploads two
images with optional descriptions, cycles dialog focus, navigates both directions,
expands the long description, scrolls it at narrow widths and closes back to the
image link. A separate native no-JavaScript journey uploads/describes an image and
opens its raw-file fallback. Existing lightbox target/gesture and profile-recovery
journeys are rerun because the language-picker helper is now shared.

**26 browser checks passed**: the new media journey, existing viewer target/gesture
checks and profile recovery across all six scripted engine/layout combinations,
plus native no-JavaScript media and profile journeys. Desktop checks also resize
the expanded viewer to 320px and back and verify conditional focusability.
Axe reports no violations in the scanned viewer/error states. **Eight
layout/localization tests passed**, including both shells and catalog parity.
A fresh Clippy check still reports only the same seven unchanged discovery/search
diagnostics: [latest log](media/accessibility-2026-10-19/clippy-server-current.txt).
Formatting, JavaScript syntax and whitespace checks passed.

Evidence: [viewer baseline](media/accessibility-2026-10-19/media-keyboard-baseline.json),
[scrolling baseline](media/accessibility-2026-10-19/media-scroll-baseline.json),
[passing matrix](media/accessibility-2026-10-19/media-keyboard-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/media-keyboard-verification-manifest.json).
Raw traces and disposable session data stay outside the committed evidence.

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/media-review.json \
  ./dev a11y media-keyboard-journey.spec.mjs profile-upload-journey.spec.mjs \
  representative.spec.mjs --grep 'Russian media|@no-js native media|profile|media lightbox' \
  --output=../target/accessibility/media-review-results
./dev test -p plamenu --lib --test web --no-fail-fast \
  -E 'test(web::layout::tests::) or test(web::i18n::tests::)'
```

## Iteration 9 verified coverage

Reporting headings, categories, guidance, optional forwarding, confirmation and
mute/block labels now use the selected interface language. English wording,
fields, category values, limits and submission behavior stay the same. Evidence
excerpts expose their existing post-language metadata. User-authored rules and
comments are not automatically translated or assigned a guessed language.

**Seven browser checks passed**: open the report form from a profile's keyboard
menu, choose a category, submit a local fixture report, use the optional mute
shortcut and unmute from the profile. Russian runs cover all six engine/layout
combinations; no-JavaScript Chromium completes the native form flow. Axe found
no violations in the scanned report form. **11 server/localization tests passed**,
covering account and post reports, confirmation, rule-forced category, remote
forwarding UI, own-post/CSRF gates and catalog parity. The browser tests use only
disposable local accounts; they do not qualify delivery to external moderators.
Syntax, formatting, catalog-reference and whitespace checks passed.

Evidence: [baseline](media/accessibility-2026-10-19/report-baseline.json),
[passing matrix](media/accessibility-2026-10-19/report-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/report-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/report-review.json \
  ./dev a11y report-journey.spec.mjs \
  --output=../target/accessibility/report-review-results
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(profile_report_) or test(report_page_) or test(report_form_) or test(remote_report_) or test(report_gates_) or test(citing_a_rule_) or test(web::i18n::tests::)'
```

## Iteration 10 verified coverage

List creation retains the submitted title, replies policy and exclusivity after
validation errors, marks an invalid title and focuses the existing error.
Rejected member additions retain the handle, associate it with the message and
focus that message. These refusals return HTML with HTTP 422; successful writes
keep their redirects. Ownership, CSRF, cardinality and follow requirements remain
unchanged. Existing allowlisted error-code URLs still render localized messages.

**Seven browser checks passed**: whitespace-title rejection → correction →
unfollowed-member rejection → follow → add → read list timeline → remove → rename
→ delete → unfollow. No-JavaScript Chromium uses the existing native deletion
review page; scripted cases use the existing browser confirmation. Axe found no
violations in the scanned error states. **30 selected Rust tests passed**, including
list CRUD/membership, ownership, Russian errors, profile membership and related
list queries. The extended tests check retained values and no persisted list after
rejection. Formatting, syntax and whitespace checks passed. Retaining edits to an
already-existing list was outside this iteration and is covered in iteration 13.

Evidence: [baseline](media/accessibility-2026-10-19/list-baseline.json),
[passing matrix](media/accessibility-2026-10-19/list-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/list-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_REPORT_PATH=target/accessibility/list-review.json \
  ./dev a11y list-journey.spec.mjs \
  --output=../target/accessibility/list-review-results
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(lists_) or test(list_timeline_) or test(other_accounts_list_) or test(profile_lists_) or test(web::lists::tests::)'
```

## Iteration 11 verified coverage

Password-change errors focus the existing message and associate it with the
current, new or confirmation field as appropriate. A password-specific error
code distinguishes this form from the independent two-factor forms. Pending
OTP setup yields autofocus only for these password errors; ordinary OTP errors
retain their existing code-entry focus. Password clearing, policy, CSRF,
reauthentication and atomic session revocation are unchanged.

**Seven password browser journeys passed**: incorrect current password → mismatch
→ correction → a second browser session loses access → restore the fixture
password → sign in again through the remembered-account chooser. Dedicated users
keep password mutations separate from other journey sessions. Axe found no
violations in the two scanned error states. **12 Rust tests passed**, including
password change, two-factor enrollment/replay/recovery and OAuth/PKCE; pending-OTP
checks ensure there is only one autofocus target. **16 harness tests passed**.

The broader run completed **138 checks: 130 passed and eight failed**. Seven
failures came from the new test matching two unrelated status messages; a
follow-up also exposed the remembered-account chooser at final sign-in. The test
now targets the password-success message and activates the existing remembered
account before entering credentials. The eighth failure was a WebKit logout
race: the test navigated away before the logout redirect completed. Waiting for
the actual redirect fixed it; **all six scripted core signup/logout journeys
passed** on the same frozen executable. The seven corrected password journeys
also passed. This is combined-run plus focused-rerun evidence, not a claim that
all 138 passed in one invocation. The reviewed baseline fails only the intended
focus/field-association assertions and completes the rest of the lifecycle.

Evidence: [baseline](media/accessibility-2026-10-19/security-password-baseline.json),
[password matrix](media/accessibility-2026-10-19/security-password-verified.json),
[original combined run](media/accessibility-2026-10-19/combined-iteration11.json),
[logout rerun](media/accessibility-2026-10-19/core-logout-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/security-password-verification-manifest.json).
Formatting, syntax and whitespace checks passed. This iteration does not claim
browser coverage of complete authenticator/security-key enrollment.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y security-password-journey.spec.mjs
A11Y_OFFLINE_FIXTURE=0 ./dev a11y core-journey.spec.mjs --grep 'signup error recovery'
./dev test -p plamenu --test web --test two_factor --lib --no-fail-fast \
  -E 'binary(two_factor) or test(settings_security_password_change_reauthenticates) or test(web::two_factor::tests::)'
```

## Iteration 12 verified coverage

Relationship selections now expose the account handle, domain or hashtag in a
localized accessible name. Empty visual labels and layout remain unchanged;
server-rendered names work without JavaScript. Dynamic values are escaped by the
HTML renderer. Bulk actions and their authorization requirements are unchanged.

**Seven browser journeys passed**: follow → cancel block → confirm block → inspect
blocked relationships → select by keyboard → unblock → verify follow is not
silently restored. Scripted cases use the existing native browser confirmation;
no-JavaScript Chromium uses the existing confirmation page. Both baseline widths
completed the workflow but failed accessible-name and axe `label` checks. The
revised blocked-account page passes both. **13 selected Rust tests passed**,
including relationship actions, follow requests, profile moderation, hashtags and
catalog parity. Renderer checks cover all three selection types in English and
Russian, plus attribute escaping. Browser evidence is bounded to account
blocking/unblocking; domain and hashtag selection names have renderer coverage.

Evidence: [baseline](media/accessibility-2026-10-19/blocking-baseline.json),
[passing matrix](media/accessibility-2026-10-19/blocking-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/blocking-verification-manifest.json).
No visible UI change requires a new screenshot pair. Formatting, syntax and
whitespace checks passed. A fresh server Clippy check still reports the same
seven diagnostics in unchanged discovery/search code and none in the modified
files: [latest log](media/accessibility-2026-10-19/clippy-server-current.txt).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y blocking-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(relationships_) or test(profile_menu_mute_and_block_round_trip) or test(profile_shows_viewer_moderation_badges) or test(web::relationships::tests::) or test(web::i18n::tests::)'
```

## Iteration 13 verified coverage

Existing-list edits retain the submitted title, replies policy and exclusivity
after validation refusal; title errors are associated with the field. Existing
filter edits retain title, action, expiry, contexts, existing keyword text and
whole-word choices, plus the new keyword draft. Only the owned filter's stored
keyword rows are rendered; submitted IDs do not add arbitrary rows. Both forms
focus their existing error and return HTML with HTTP 422. Successful submissions
keep their redirects. The list renderer also preserves an existing ownership
lookup error response instead of converting every lookup failure into a 404.

**28 browser journeys passed**: both new edit-recovery journeys, plus the earlier
list creation/membership and filter creation journeys, in all seven browser
configurations. Corrections save and survive reload. Axe found no violations in
the scanned edit-error states. **31 selected Rust tests passed**, including both
extended CRUD paths, list ownership, filter account limits and associated list
queries. The extended server checks verify escaped drafts, checked and unchecked
choices, and unchanged stored list/filter/keyword data after rejection.
Formatting, JavaScript syntax and whitespace checks passed.

Evidence: [baseline](media/accessibility-2026-10-19/edit-recovery-baseline.json),
[passing matrix](media/accessibility-2026-10-19/edit-recovery-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/edit-recovery-verification-manifest.json).
The same gallery includes rejected-edit comparisons at both widths. Database
outages and concurrent edits remain outside these journeys; validation recovery
does not imply recovery from every server failure.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y edit-recovery-journey.spec.mjs \
  filter-journey.spec.mjs list-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(settings_filters_) or test(lists_) or test(other_accounts_list_) or test(web::filters::tests::) or test(web::lists::tests::)'
```

## Iteration 14 verified coverage

Privacy-domain validation failures now return the existing settings form with a
localized summary and an explicitly English shared validation detail. The domain
field is associated with that detail and marked invalid; the summary receives
focus. Raw textarea text, all seven checkboxes and all six notification policies
are retained. Existing normalization, optional domains, accepted values and
successful saves remain unchanged. This fix handles the existing 422 validation
refusal; unrelated service/database errors are not claimed as recovered.

**Seven browser journeys passed**: change every checkbox and notification policy
→ submit an invalid domain → correct it → verify persisted normalization and
choices → restore original settings. Native no-JavaScript recovery is included.
Axe found no violations in the scanned form. **Eight Rust/localization tests
passed**, including Russian summary/English detail language, escaped domain text,
field association and unchanged persisted profile flags, domains, search-index
preference and notification policy after refusal. Formatting, syntax and
whitespace checks passed. A fresh server Clippy run, including iterations 13–14,
reports only the same seven unchanged discovery/search diagnostics:
[latest log](media/accessibility-2026-10-19/clippy-server-current.txt).

Evidence: [baseline](media/accessibility-2026-10-19/privacy-baseline.json),
[passing matrix](media/accessibility-2026-10-19/privacy-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/privacy-verification-manifest.json).
The gallery retains the original JSON and editor captures, including the mobile
fallback-width difference. Translating the shared API validation details remains
a broader localization improvement; their language is explicitly marked here.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y privacy-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(settings_privacy_) or test(privacy_saves_all_notification_policy_categories) or test(web::i18n::tests::)'
```

## Iteration 15 verified coverage

Inbox menus, unread labels, participant joins/counts, removal confirmation and
missing-message fallback now use the interface language. Narrow cards expand the
existing menu within the row instead of covering the conversation preview with
its actions. The trigger stays in place, and the conversation link remains
reachable after the menu actions. CSS handles this with or without JavaScript.

**Seven browser journeys passed**: profile → private composer → local recipient
reads and replies → sender marks read/unread → removes the row → a later reply
restores it → opens the thread. Six scripted engine/layout combinations use the
Russian interface; native Chromium uses English. An additional **320px
no-JavaScript journey passed**, including menu geometry and Tab to the visible
preview link. Axe found no violations in the revised scanned menus. **Nine Rust
tests passed**, including localized participant/deleted-message rendering, direct
reply visibility locking, unread tracking, CSRF and ownership gates. These are
local disposable conversations, not external delivery/interoperability tests.

Evidence: [baseline](media/accessibility-2026-10-19/conversation-baseline.json),
[passing matrix](media/accessibility-2026-10-19/conversation-verified.json),
[narrow native journey](media/accessibility-2026-10-19/conversation-native-narrow.json),
and [source/executable hashes](media/accessibility-2026-10-19/conversation-verification-manifest.json).
The single gallery shows both widths. Formatting, syntax and whitespace checks
passed. A fresh server Clippy check reports the same seven unchanged diagnostics
and none in the modified files. User-authored message text and account names are
not translated.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y conversation-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(conversation_row_) or test(conversations_page_tracks_direct_threads) or test(direct_reply_composer_is_locked) or test(web::i18n::tests::)'
```

## Iteration 16 verified coverage

Rejected reschedules return HTML with HTTP 422, retain the proposed time and
associate that field with a focused error. The shared English validation detail
is explicitly marked as English. Only the submitted entry is marked invalid.
Reschedule forms carry the current queue cursor through validation and success;
cancellation retains its original return to the first page, avoiding a misleading
empty older page. Future-time requirements, time-zone conversion and publication
behavior are unchanged.

**Seven browser journeys passed**: compose a future post → reject a past time →
correct the time → verify it survives reload → cancel. Axe found no violations in
the scanned error state. **Six selected Rust tests passed**, including native
schedule controls, date parsing and rescheduling. A **final scoped integration
rerun passed** after restoring cancellation's first-page redirect; it verifies the
retained cursor/draft, unchanged stored time on refusal and successful cancellation.
This final adjustment removes only the cancellation cursor/redirect change; the
browser screenshots and focus/retention results precede it, as recorded in the
manifest. Formatting, syntax and whitespace checks passed. A fresh server Clippy
check still reports only the seven unchanged discovery/search diagnostics. Actual timed delivery
and daylight-saving transitions are outside this browser journey.

Evidence: [baseline](media/accessibility-2026-10-19/scheduling-baseline.json),
[passing matrix](media/accessibility-2026-10-19/scheduling-verified.json),
[final integration check](media/accessibility-2026-10-19/scheduling-final-server.txt),
and [source/executable hashes](media/accessibility-2026-10-19/scheduling-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y scheduling-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(scheduled_) or test(schedule_) or test(web::scheduled::tests::)'
```

## Iteration 17 verified coverage

The web sign-in challenge focuses its existing error after a rejected code,
while fresh challenges still focus code entry. Existing invalid-field and error
description attributes remain; the code field is cleared. Enrollment, supported
factors, attempt limits, replay protection and backup-code consumption do not
change. Two-factor authentication remains opt-in.

**Seven browser journeys passed**: enable with the displayed setup key and an
independent authenticator calculation → save the ten fixture recovery codes →
sign out → reject an invalid code → recover with a backup code → sign out →
reject reuse of that code → recover with another → disable through the existing
password-gated form. The native no-JavaScript journey completes the same flow.
Axe found no violations in the scanned challenge error. **All ten two-factor
server tests passed**, including TOTP sign-in, replay, recovery, enrollment,
disabling, unknown challenges and OAuth/PKCE binding. Additional assertions keep
exactly one autofocus target on fresh and failed web challenges.

Evidence: [baseline](media/accessibility-2026-10-19/two-factor-baseline.json),
[passing matrix](media/accessibility-2026-10-19/two-factor-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/two-factor-verification-manifest.json).
Only the safe sign-in error screenshots are included; setup keys, backup codes
and raw traces are not committed. Formatting, syntax and whitespace checks
passed; the current server Clippy check still has only the seven known unchanged
diagnostics. The browser evidence uses a software authenticator calculation,
not a physical authenticator app or security key. OAuth challenge error focus
and setup-form error focus remain separate, unmodified review candidates.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y two-factor-journey.spec.mjs
./dev test -p plamenu --test two_factor --no-fail-fast
```

## Iteration 18 verified coverage

**15 existing video browser journeys passed** on the frozen reviewed application:
upload/review/publication and authoring/help/error recovery across all six
scripted engine/layout combinations, plus native upload/captions/publication,
metadata authoring/editing and authoring-error recovery without JavaScript.

The selected publication journey creates/selects a channel, uploads a generated
video with optional visual description, transcript and WebVTT captions, reviews
and publishes it, exercises playback and video discovery/follow controls, and
checks the embed/native fallback. The authoring journey checks collapsed optional
sections, retained values, keyboard help dismissal and focus return, the clickable
help link, touch/hover dismissal where supported, and rejected live scheduling
without creating a broadcast. The runs include axe and reflow assertions in their
selected states. They use automated file selection and include some direct
player/API inspection, so they are not wholly keyboard-only or human usability
tests. Camera/microphone permission and physical assistive technology were not
exercised; conditional inspection of an existing broadcast is not a full live
broadcast journey.

No new failure was reproduced in these selected journeys. This checkpoint adds
coverage evidence to the same overview without changing production UI or tests.
Optional alternatives remain optional; the fixture's supplied alternatives do
not establish the quality or presence of captions/descriptions on user content.
No new visual change needs a before/after pair.

Evidence: [passing matrix](media/accessibility-2026-10-19/video-authoring-verified.json)
and [executable/test hashes](media/accessibility-2026-10-19/video-authoring-verification-manifest.json).
The aggregate report lists the exact 15 selected checks, including native
metadata authoring/editing.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y video-authoring-ux.spec.mjs native-video.spec.mjs \
  --grep 'Authoring|native video upload'
```

## Iteration 19 verified coverage

Group-creation validation failures now return the existing form with HTTP 422,
retaining address, display name, membership policy and posting policy. The
localized existing error receives focus; address and display-name refusals are
associated with their respective inputs. Creation permission, admission limits,
quotas and successful redirects are unchanged. The existing group is not modified
when its name is submitted again.

**Seven browser journeys passed**: taken name → correction → create with approval
membership and moderator-only posting → owner publishes a group post → another
local account requests membership → owner approves → member reads the post,
has no unauthorized posting control and leaves. No-JavaScript Chromium completes
the same forms. Axe found no violations in the scanned creation-error state.
**Two server tests passed**, covering escaped retained drafts, unchanged existing
group policies/name and the instance's creation-permission restriction.
Formatting, syntax and whitespace checks passed.

Evidence: [baseline](media/accessibility-2026-10-19/group-baseline.json),
[passing matrix](media/accessibility-2026-10-19/group-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/group-verification-manifest.json).
These are local group journeys, not qualification of remote community approval,
moderation federation or every group-management form.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y group-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(groups_page_creates_and_lists) or test(groups_creation_respects_the_instance_policy) or test(web::groups::)'
```

## Iteration 20 verified coverage

The shared composer error style now uses `--danger-text`, already defined for
body-sized text on the tinted panel. Its light-theme token-derived contrast is
about 5.48:1 instead of 4.27:1 (normal-text minimum 4.5:1); the dark token is
unchanged. The full-composer error receives native focus. Existing draft
retention and event/date requirements remain unchanged. The separate edit-post
form does not gain error autofocus in this iteration, although it shares the
corrected colour token.

Attendance failure restores the initiating button only when disabling it lost
focus. Refreshing RSVP fragments preserves focus in a replaced region, or in the
originating region if focus dropped to the document; focus elsewhere stays put.
The available action receives focus, with a programmatic region fallback when
no action remains. Existing attendance explanations expose status semantics.
The browser tests cover the action-bearing states; the no-action fallback is a
reviewed guard, not a separately reproduced browser case.

**Seven browser journeys passed**: rejected event end time → retained draft →
correction/publish → another local account's refused attendance → retry → delayed
withdrawal refusal while focus moves elsewhere → successful withdrawal → attend
again → organizer cancellation. Native no-JavaScript forms cover event correction,
attendance, withdrawal and cancellation. **Three Rust tests passed**, covering
selected time zones, retained event metadata, invalid/DST times and RSVP rendering.
Axe passes the final scanned states, including the settled dark composer.

The first revised run passed native Chromium but failed six dark-theme scans
because control colours were still transitioning after the theme switch. Waiting
for finite animations to finish made all seven cases pass on the same executable;
no unrelated theme colours were changed. This timing correction is recorded in
both reports. Formatting, JavaScript syntax and whitespace checks passed. A fresh
server Clippy check still reports the same seven unchanged discovery/search
diagnostics and none in these modified files.

Evidence: [baseline](media/accessibility-2026-10-19/event-baseline.json),
[first transition-time scan](media/accessibility-2026-10-19/event-transition-scan.json),
[passing matrix](media/accessibility-2026-10-19/event-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/event-verification-manifest.json).
The gallery includes composer-error, failed-attendance and saved-attendance pairs.
Remote attendance negotiation and physical assistive technology remain untested.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y event-journey.spec.mjs
./dev test -p plamenu --test web --lib --no-fail-fast \
  -E 'test(composer_event_) or test(event_posts_render_an_rsvp_cluster_per_join_mode)'
```

## Iteration 21 verified coverage

A rejected Webxdc package now focuses the existing creation-page error. The
browser journey checks that session name, description, admission policy, update
interval and size limit remain available; their retention predates this change.
The user reselects a valid package, as browsers cannot refill a file input.

**Seven browser journeys passed**: invalid package → retained form → valid upload
→ approval-only session → second local account requests admission → owner
approves → owner closes the session. In all six scripted engine/layout cases,
both participants also open titled app frames on isolated origins, send one
update through the fixture app's keyboard-operated button, and receive the
shared update in both frames. Native no-JavaScript Chromium covers session
management; executing an app requires JavaScript. Axe passes the scanned
creation-error and scripted player/frame states. The two baseline Chromium
cases completed the journey but failed the expected error-focus assertion.

**Five selected server tests and 17 harness tests passed**, including retained
multipart failure recovery, creation permissions/CSRF, isolated runtime surfaces
and durable update delivery. The harness run retains two existing resource
warnings; it is not reported as warning-free. Formatting, JavaScript syntax and
whitespace checks passed.

The disposable TLS proxy now serves `*.webxdc.localhost` as well as `localhost`
using its existing ephemeral trusted CA. Certificate verification stays enabled;
production TLS configuration is unchanged. The small, deterministic fixture
package and its source/builder are committed together under
`accessibility/fixtures/webxdc/`. Rebuild it with
`python3 accessibility/fixtures/webxdc/build.py`.

Evidence: [baseline](media/accessibility-2026-10-19/webxdc-baseline.json),
[passing matrix](media/accessibility-2026-10-19/webxdc-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/webxdc-verification-manifest.json).
The gallery contains the creation-error pair at both widths. This qualifies the
specific local fixture journey, not arbitrary uploaded apps, remote Webxdc
federation or screen-reader speech. First-party Webxdc text still needs a
Russian-interface localization/language-parts review.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y webxdc-journey.spec.mjs
./dev test -p plamenu --test webxdc --no-fail-fast \
  -E 'test(upload_limits_are_live_and_multipart_failures_preserve_the_form) or test(session_creation_unifies_app_choice_metadata_and_standard_defaults) or test(actor_bundle_landing_and_isolated_runtime_are_distinct_surfaces) or test(creation_permission_is_not_staff_and_admin_actions_require_permission_and_csrf) or test(admission_boundary_replay_and_live_fanout_share_the_durable_log)'
```

## Iteration 22 verified coverage

Conversation previews now carry the existing declared language of the original
message text or content warning. Their text comes from the original source, so
the optional translation target is not used for this attribute. Media-only
fallbacks and the “Open conversation” label continue to inherit the interface
language. Missing language metadata remains unspecified; users are not required
to supply it. There is no new control, visible wording or layout change.

**Seven browser journeys passed**: compose a French private message → recipient
reads the preview → opens the correctly marked body → repeat with a French
content warning → open the warning. All six scripted engine/layout pairs and
native no-JavaScript Chromium preserve the preview declaration while interface
metadata inherits `en-US`. **Three renderer tests passed**, covering plain text,
warnings, escaping, absent metadata, translation metadata and the Russian media
fallback. Formatting, JavaScript syntax and whitespace checks passed.

Initial unit expectations were corrected for the existing whitespace-preserving
HTML-to-text conversion and actual Russian wording. The first browser assertion
assumed `en` instead of `en-US`. A library-only build also left the old web
executable unchanged; that obsolete run was interrupted and excluded. The final
matrix uses the explicitly rebuilt and separately frozen web executable.

Evidence: [corrected baseline](media/accessibility-2026-10-19/conversation-language-baseline.json),
[passing matrix](media/accessibility-2026-10-19/conversation-language-verified.json),
and [source/executable hashes](media/accessibility-2026-10-19/conversation-language-verification-manifest.json).
The gallery identifies this as a semantic change without a screenshot pair.
Actual screen-reader pronunciation and messages lacking language metadata remain
outside this verification.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y conversation-language.spec.mjs
./dev test -p plamenu --lib --no-fail-fast \
  -E 'test(conversation_preview_) or test(conversation_media_fallback_) or test(conversation_row_localizes_)'
cargo build --locked -p plamenu --bin plamenu
```

The accumulated 215-case run for iterations 1–21 completed with **198 passes
and 17 failures**. Later journeys hit login failures and changed video-authoring
expectations; investigation and correction are pending. Preserve this result
as evidence, not a passing combined claim: [report](media/accessibility-2026-10-19/combined-through-21.json)
and [frozen-source manifest](media/accessibility-2026-10-19/combined-through-21-manifest.json).
Later production edits were absent from that frozen iteration 21 executable.

## Iteration 23 verified coverage

The independent poll handler now preserves keyboard context, like the earlier
post-action and attendance fixes. Failed requests re-enable the existing Vote
button and restore focus only if disabling it lost focus to the document.
Results replacing the active voting form receive programmatic focus with
`tabindex="-1"`; this does not create an extra ordinary Tab stop. Focus already
moved outside the affected region remains where the user put it.

**Seven browser journeys passed**: publish a poll → another local account selects
a choice → HTTP 503 refusal → choice retained and button focused → delayed
connection failure while focus moves to the reply field → retry → authoritative
results with the chosen option highlighted → focused results → Tab onward →
reload confirms one vote. The six scripted cases also create a second poll and
confirm that delayed successful voting does not steal focus from the reply field.
Native no-JavaScript Chromium completes creation and voting through ordinary
forms. Axe passes the scanned failure and result states. JavaScript syntax and
whitespace checks passed; production changes are confined to the browser script.

The initial test assumed the literal text `1 vote`; Fluent correctly inserts
bidirectional isolation characters around the number. The assertion was corrected
before the comparison baseline and final matrix. The corrected baseline's only
failures concern missing button/results focus and the programmatic focus target.

Evidence: [baseline](media/accessibility-2026-10-19/poll-focus-baseline.json),
[passing matrix](media/accessibility-2026-10-19/poll-focus-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/poll-focus-verification-manifest.json).
The gallery contains refusal and results pairs at both widths. This is local
single-choice voting; remote vote reconciliation, multiple-choice interaction
and actual screen-reader speech remain separate coverage boundaries.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y poll-focus.spec.mjs
```

## Iteration 24 verified coverage

Recognized security-page errors now receive focus, including two-factor setup
and disabling refusals. A rejected setup code has `aria-invalid="true"` and an
association with the existing error. Fresh setup retains code-entry autofocus;
only one target owns autofocus. The separate password-change form is not marked
invalid by an OTP error. Code and password rules, optional enrollment and key
retention remain unchanged.

**21 browser journeys passed**: seven new setup journeys plus all 14 related
password-change and two-factor sign-in journeys. The new path exercises a wrong
setup password → correction → fresh code focus → rejected code → retained setup
key and focused/associated error → valid independent authenticator code → ten
recovery codes → wrong disabling password → correction → disabled state. The
native no-JavaScript configuration completes the same forms. Axe passes the
scanned pending-setup error state. **All ten two-factor server tests passed**,
including the existing authentication and OAuth security boundaries. Formatting,
JavaScript syntax and whitespace checks passed.

Evidence: [baseline](media/accessibility-2026-10-19/two-factor-setup-baseline.json),
[passing matrix](media/accessibility-2026-10-19/two-factor-setup-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/two-factor-setup-verification-manifest.json).
The gallery shows cropped password/code error pairs at both widths. Setup keys,
QR codes and recovery codes are excluded. OAuth browser challenge focus and
physical security-key ceremonies remain separate unfinished journeys.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y two-factor-setup.spec.mjs two-factor-journey.spec.mjs security-password-journey.spec.mjs
./dev test -p plamenu --test two_factor --no-fail-fast
cargo build --locked -p plamenu --bin plamenu
```

## Iteration 25 verified coverage and remaining finding

The accumulated run exposed HTTP 429 authentication responses. Multiple
independent test actors had shared one engine/IP budget and repeatedly signed
in the same participant account. The fixture now assigns bounded participant,
security and recovery client identities per engine, seeds ordinary participant
accounts per engine, and reuses their real browser sessions. Production limits
stay unchanged; failed logins within each client still count. The proxy only
accepts its fixed identity list and replaces arbitrary forwarded headers.

The video-authoring journey now creates its own channel when needed instead of
relying on a prior video-publication test. The report journey waits for the mute
POST to finish before navigating to check the result. Nineteen harness tests
pass. The corrected 236-case matrix includes all earlier selected journeys plus
iterations 22–24 against the frozen iteration 24 executable: **233 passed and
three failed**. The earlier authentication, video and mute failures are cleared.

Two remaining no-JavaScript cases used a 34-character participant handle, beyond
the 30-character local mention limit. The fixture now uses `peer_` handles, with
a regression check that all fit the limit. **All 14 conversation and declared-
language journeys pass** on that corrected fixture. The other combined failure
was a target-size flag on a long participant name in narrow Chromium. That
separate product boundary remains open as A43; changing fixture names is not
claimed as its fix. There is no all-passing 236-case run to report.

Combined evidence: [233/236 report](media/accessibility-2026-10-19/combined-through24-corrected.json),
[14 passing message checks](media/accessibility-2026-10-19/conversation-fixture-verified.json),
and [source/executable provenance](media/accessibility-2026-10-19/combined-through24-verification-manifest.json).

The first audio/music run completed with four passes and ten failures:
[initial report](media/accessibility-2026-10-19/music-entry-initial.json).
Desktop listener tests selected the global Discover link rather than Music's
Discover link. Audio-authoring tests expected a removed disclosure/link. Tests
now use the scoped Music navigation and the current composer type selector;
the 14-case rerun passed all seven listener cases and four publication cases.
The remaining three narrow publication failures used a desktop-only CSS
selector. With the shared “New post” accessible name, all seven publication
cases now pass. These are test-navigation corrections,
not a reason to reintroduce old controls into the product.

Music evidence: [navigation-corrected run](media/accessibility-2026-10-19/music-entry-navigation-corrected.json),
[seven passing publication cases](media/accessibility-2026-10-19/music-publication-verified.json),
and [source/executable hashes and scope](media/accessibility-2026-10-19/music-entry-verification-manifest.json).
There are 14 unique passing journeys across these reports, not one entirely
passing 14-case matrix. Automated file selection and WebKit pointer playback
are used; local synthetic audio does not qualify arbitrary media alternatives
or remote-server interoperability.

OAuth challenge recovery is the next independently scoped product change.
Genuine browser rate-limit responses also need accessible recovery context;
fixture isolation does not resolve that product presentation gap. The long-name
participant target needs its own baseline, correction and visual review.

No production interface is changed by iteration 25, so it adds no screenshot
pair. The test fixes preserve the existing navigation and anti-abuse rules.

## Iteration 26 verified coverage

A rejected OAuth second-factor code now focuses the existing error. Fresh
challenges retain code-entry autofocus; existing code/error associations and
server-held authorization binding stay unchanged.

**Seven browser journeys passed**: opt-in enrollment → register a disposable
profile-only client → a fresh browser enters credentials → fresh challenge focus
→ invalid code → focused error → recovery-code correction → local callback →
S256 PKCE code exchange → verify the account through the local API. Repeating
the authorization request reuses the session and offers explicit deny/allow
choices; both redirects preserve the client's state. Native no-JavaScript
Chromium completes the browser forms. Axe passes the invalid-challenge state.
**All ten two-factor server tests passed**, including the extended OAuth refusal
and correction path. Formatting, JavaScript syntax and whitespace checks pass.
A fresh server-library Clippy check still reports the same seven unchanged
discovery/search diagnostics and none in the modified production files.

The callback is a path on the disposable localhost origin with no client UI;
the test verifies its redirect parameters and performs the client's local API
exchange. No external application is contacted. This covers the local OAuth
flow, not third-party client interoperability or a physical security key.

Evidence: [baseline](media/accessibility-2026-10-19/oauth-baseline.json),
[passing matrix](media/accessibility-2026-10-19/oauth-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/oauth-verification-manifest.json).
The gallery has desktop and narrow challenge-error pairs, with no setup keys,
recovery codes or client credentials. Actual screen-reader speech remains
unverified.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y oauth-journey.spec.mjs
./dev test -p plamenu --test two_factor --no-fail-fast
cargo build --locked -p plamenu --bin plamenu
```

## Iteration 27 verified coverage

Explicit HTML browser POSTs to `/login` and `/login/challenge` now receive a
localized throttle page instead of raw JSON. It has a level-one heading,
focused explanation and native return link. The response remains HTTP 429;
rate-limit headers, private/no-store handling and normal HTML security headers
remain in force. No password or form secret is echoed. Requests without an
accepted HTML representation and API endpoints retain their JSON response.

**Seven browser cases passed** using the ordinary production threshold: repeated
wrong credentials → actual HTTP 429 → readable, focused explanation → return
to the form → retry with correct credentials remains refused while the limit
is active. The six scripted engine/layout pairs pass axe; native no-JavaScript
Chromium completes the same error/return path at 320px. The test intentionally
exhausts a separate bounded fixture client and valid `limit_` account for each
engine, so it cannot consume another journey's allowance.

**16 Rust checks passed**: all 11 rate-limit integration tests and five
localization checks. Coverage includes both IP and identifier-only refusal,
English/Russian HTML, `text/html;q=0` exclusion, JSON API behavior, no password
echo, retained rate-limit headers/CSP, durable budgets and trusted-proxy rules.
**19 harness tests passed**. Formatting, JavaScript syntax and whitespace checks
pass. Fresh server-library Clippy still reports the same seven unchanged
discovery/search diagnostics, with none in this change.

Evidence: [baseline](media/accessibility-2026-10-19/login-throttle-baseline.json),
[passing matrix](media/accessibility-2026-10-19/login-throttle-verified.json), and
[source/executable hashes](media/accessibility-2026-10-19/login-throttle-verification-manifest.json).
The gallery shows desktop/narrow pairs. The raw JSON baseline lacks the normal
viewport metadata, so its narrow screenshot is 980px wide; it is not rescaled
or edited in the source PNG.

The browser run does not wait for the actual limit window to expire, and it does
not establish screen-reader speech behavior. There is no countdown because an
accurate common retry time is not available from both limiter paths here. OAuth
rate-limit responses and non-throttle outage responses remain separate review
boundaries. This does not change admission policy or add a recovery requirement.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y login-throttle-journey.spec.mjs
./dev test -p plamenu --test rate_limit --lib --no-fail-fast \
  -E 'binary(rate_limit) or test(web::i18n::) or test(security_headers)'
```

## Iteration 28 verified coverage

A dedicated journey uses valid short handles and long display names. The
baseline reproduces the narrow target-size flag, 20px link height and clipped
focus outline. A 20px height alone is not a claim that every link fails WCAG,
because target-spacing exceptions also matter.

Existing participant links now have 24px minimum dimensions. Each name truncates
independently within a flexible name container, keeping the next participant
link visible. Small vertical padding leaves room for the existing outline.
Localized separators, full accessible names and profile destinations remain
intact; there is no new control, popup or required step.

**28 browser checks passed**: 14 target journeys (one and two displayed names in
all seven configurations), plus all 14 existing conversation/language journeys.
Tests exchange private messages, open the menu, check the visible intersection
and a real point hit test, verify outline clearance, and activate the profile
link with the keyboard. The two-name cases also check the second link's size
and pointer reachability. Scanned scripted states pass axe; the two native
no-JavaScript target cases run at 320px. **Three renderer tests passed**, retaining
localized participant summaries and declared preview language. Formatting,
JavaScript syntax and whitespace checks pass.

The first CSS candidate passed 19/21 checks but hid an overlong atomic link in
WebKit; the final layout bounds each name separately. A native test expectation
also exceeded the existing 30-character display-name field limit and was
corrected. The final matrix adds explicit two-name coverage. These unsuccessful
checks are retained rather than represented as a passing first attempt.

Evidence: [complete baseline](media/accessibility-2026-10-19/conversation-target-baseline.json),
[first candidate](media/accessibility-2026-10-19/conversation-target-first-candidate.json),
[passing matrix](media/accessibility-2026-10-19/conversation-target-verified.json),
and [source/executable hashes](media/accessibility-2026-10-19/conversation-target-verification-manifest.json).
The gallery compares the affected card at both widths, excluding unrelated
inbox threads from the crop. Larger participant sets, physical devices and
screen-reader speech remain outside exhaustive qualification.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y conversation-targets.spec.mjs conversation-journey.spec.mjs conversation-language.spec.mjs
./dev test -p plamenu --lib --no-fail-fast \
  -E 'test(conversation_preview_) or test(conversation_media_fallback_) or test(conversation_row_localizes_)'
```

## Iteration 29 verified coverage

The expanded combined run finished with **277 passes and one failure across
278 cases**. It includes the prior core and advanced journeys, OAuth recovery,
genuine login throttling, long participant names and both music entry journeys.
It uses the frozen iteration 28 executable. Application commit and browser-source
hashes were captured before execution, so later checkout revisions in the
aggregate report do not identify the compiled application.

The one failure was a test assumption: the no-JavaScript music subscription
expected the publisher's handle, while an earlier profile journey had changed
that account's display name. The subscription already identifies its account
by the exact library URL. Its text assertion now uses the account's current
display name, falling back to username. All **20 profile/audio follow-up cases
pass**, including the changed-name interaction. This is a focused verification,
not a rerun or passing claim for the complete 278-case matrix.

The intentional throttle test also allows a fixed-window boundary inside a
bounded attempt sequence (at most 61 requests). Production limits remain
unchanged. Iteration numbers follow the start of each work stream; their commits
can complete in a different order.

Evidence: [277/278 combined report](media/accessibility-2026-10-19/combined-iteration29.json),
[start-of-run source/executable hashes](media/accessibility-2026-10-19/combined-iteration29-manifest.json),
[20 passing follow-up cases](media/accessibility-2026-10-19/combined29-music-corrected.json),
[follow-up hashes and scope](media/accessibility-2026-10-19/combined29-followup-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y profile-upload-journey.spec.mjs music.spec.mjs
```

## Iteration 30 verified coverage

The advanced library and player journeys cover stored recordings, metadata and
artwork, ordered playlists, membership grants and revocation, saved resources,
queue controls, playback continuity, restoration, history and statistics.
Secondary browser contexts now inherit the selected project's viewport and
fixture headers. Initial Chromium desktop/narrow checks passed (four cases), followed by all
14 cases in Chromium, Firefox and WebKit at both widths plus native forms with
JavaScript disabled, against the frozen iteration 28 executable. No production
interface changes are included in this checkpoint, so no screenshot pair is
needed. Scripted playback continuity/restoration and native queue controls are
different paths; automated file selection and API access assertions supplement
UI interaction. Native WebKit playback uses pointer activation. These local
results do not establish remote interoperability or screen-reader speech.

Evidence: [initial four cases](media/accessibility-2026-10-19/music-advanced-baseline.json),
[passing full matrix](media/accessibility-2026-10-19/music-advanced-verified.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/music-advanced-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y music-player.spec.mjs music-library.spec.mjs
```

## Iteration 31 verified coverage

A completed but rejected audio upload returns a separate problem page and loses
the optional title, artist and album. Both Chromium widths reproduce this.
The change keeps the existing upload form and escaped metadata, opens the
optional details only when they contain work, focuses the error and explains
why the file must be selected again. The existing publish/private-save choices
and optional metadata remain unchanged. WCAG 2.2 3.3.7 (Redundant Entry) and
3.3.1/3.3.3 error recovery inform this change; it is not a conformance claim.

Scope is a completed import rejected as a bad/unprocessable request. Malformed
multipart input, tampered metadata lengths and publication outages after an
asset was saved remain separate boundaries. Browser file selection cannot be
repopulated by returned HTML. No draft server storage or extra confirmation step
is introduced. No metadata fields, extra approval or draft-management controls
are required. The gallery has reviewed desktop/narrow before/after pairs.

All seven browser configurations pass rejection, error focus, empty native file
selection, retained escaped metadata, correction, publication and native audio
playback. Scripted error pages pass the selected axe rules; 320px pages do not
scroll horizontally. Automated file selection supplements keyboard actions.
The server integration check covers both publish/private-save rejection, escaping,
no persisted asset, no-store caching and CSRF rejection. Two catalog checks also
pass; the service executable was explicitly rebuilt afterward.

Evidence: [reproduced baseline](media/accessibility-2026-10-19/music-upload-baseline.json),
[passing browser matrix](media/accessibility-2026-10-19/music-upload-verified.json),
[source/executable hashes and server scope](media/accessibility-2026-10-19/music-upload-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y music-upload-recovery.spec.mjs
./dev test -p plamenu --no-fail-fast -E 'test(music_upload_rejection_keeps_metadata_without_creating_asset) or test(russian_catalog_has_every_source_message) or test(every_offered_interface_language_has_a_catalog)'
```

## Iteration 32 verified coverage

Webxdc session creation still shows English controls under a Russian page
language, including labels updated by JavaScript after an app choice. The
change localizes creation labels, hints, dynamic choice summaries and count
plurals. Existing backend error details are marked `lang="en"`; supplied app
names/descriptions are preserved. Library management and session/player screens
remain separate localization boundaries. This addresses 3.1.2 Language of Parts
and comprehensibility without introducing controls, permissions or requirements.

The initial survey needed two test corrections: the locale picker stores `en`
(the page declares `en-US`), and styled native radio inputs should be exercised
with Space rather than clicking their covered input boxes. No production change
was made for either test mismatch. Full-page captures normalize scroll to avoid
Chromium painting offscreen fixed elements into the screenshot.

Both baseline widths complete selection, rejection and correction while the
language assertions fail. All **14 candidate cases pass**, and the same 14 pass again after replacing two
count/limit conversions with checked conversions during lint review: seven Russian
creation journeys and seven existing English approval/shared-update journeys.
The Russian path exercises library upload, search with no results, choosing a
saved app, switching back to a device upload, rejected package recovery, retained
session metadata and successful creation from the library. Native forms work
without scripts. Selected axe rules pass on scripted error pages and no
horizontal page overflow occurs at 320px. Actual assistive-technology speech and
arbitrary app content remain unverified.

Three Rust checks pass again after the lint correction: the session-creation integration now checks both locales
and preserved author metadata, plus both catalog checks. The gallery includes
reviewed desktop/narrow pairs for creation and rejected upload. Browser-owned
file-picker text follows the browser's language, independently of Plamenu. The dependency-inclusive lint command stops at eight errors in unchanged database/federation discovery code; the final server-only `--no-deps` report retains the same seven unchanged discovery/search errors. No new lint errors remain in the changed creation code. These are not passing lint claims. See [server report](media/accessibility-2026-10-19/clippy-server-current.txt) and [dependency report](media/accessibility-2026-10-19/clippy-dependencies-current.txt).

Evidence: [baseline](media/accessibility-2026-10-19/webxdc-language-baseline-final.json),
[first passing candidate](media/accessibility-2026-10-19/webxdc-language-verified.json),
[14 passing final cases](media/accessibility-2026-10-19/webxdc-language-final.json),
[source/executable hashes](media/accessibility-2026-10-19/webxdc-language-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y webxdc-create-language.spec.mjs webxdc-journey.spec.mjs
./dev test -p plamenu --lib --test webxdc --no-fail-fast -E 'test(session_creation_unifies_app_choice_metadata_and_standard_defaults) or test(russian_catalog_has_every_source_message) or test(every_offered_interface_language_has_a_catalog)'
```

## Iteration 33 verified coverage

All 14 podcast/radio cases pass across Chromium, Firefox and WebKit at both
widths plus native forms without scripts, following four passing Chromium
survey cases. Podcast checks use a disposable local RSS, artwork and audio
producer: subscribe via a website, inspect episodes, mark played/unplayed,
add episodes to a playlist and queue, decode audio, import OPML with duplicate
recognition, and unfollow. The scripted journey also checks playback reporting
when the enhancement script arrives after playback begins.

Radio checks cover creation, private/public/local visibility, owner-only
configuration, member grants/revocation, saved references and removal after
access is revoked, and deletion. Secondary contexts inherit each project's
viewport and fixture headers. UI actions are supplemented by API permission
assertions and automated file selection; native WebKit playback uses pointer
activation. No public RSS producer or station was contacted, and this does not
qualify remote-provider compatibility or screen-reader speech.

This checkpoint changes test context setup only; no production visual change
or screenshot pair is needed. Radio creation refusal is a separate reproduced
issue tracked for iteration 34.

Evidence: [initial cases](media/accessibility-2026-10-19/music-podcast-radio-baseline.json),
[14 passing cases](media/accessibility-2026-10-19/music-podcast-radio-verified.json),
[source/executable hashes and fixture scope](media/accessibility-2026-10-19/music-podcast-radio-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 A11Y_PODCAST_FIXTURE=1 ./dev a11y music-podcasts.spec.mjs music-radios.spec.mjs
```

## Iteration 34 verified coverage

A name containing only spaces passes native required-field validation but is
rejected by the existing server rule. The response is raw JSON, with no native
correction form or retained station details. Both baseline widths reproduce
this and complete creation after manually re-entering the draft.

The change reuses the existing creation fields, retains escaped description,
visibility and opaque station configuration, focuses the error, and associates
localized existing name requirements with the name input. No JSON requirement
is imposed on configuration: it remains opaque text, as before. Other creation
validation/quota errors retain the form with English backend details explicitly
marked. Edit/concurrent-revision and membership-refusal recovery remain separate
boundaries; service rules and API responses are unchanged.

All **14 browser cases pass**: seven refusal/correction journeys and seven
existing station-management journeys. Recovery checks retained details, field
association, native error focus, narrow reflow and publication of the corrected
configuration. Selected axe rules pass in scripted error pages. Three Rust
checks pass, including English/Russian help, escaped retention, no row written
on refusal and successful opaque-configuration correction; both catalog checks
also pass. The latest server lint report has the same seven unchanged
discovery/search errors, with no new radio error.

Reviewed screenshot pairs are in the gallery. The mobile baseline raw JSON
uses a 980px layout viewport because it has no application viewport metadata;
the preserved original capture is not a 320px application layout. The corrected
HTML page reflows at 320px.

Evidence: [reproduced baseline](media/accessibility-2026-10-19/radio-create-baseline.json),
[14 passing cases](media/accessibility-2026-10-19/radio-create-verified.json),
[source/executable hashes](media/accessibility-2026-10-19/radio-create-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y radio-create-recovery.spec.mjs music-radios.spec.mjs
./dev test -p plamenu --lib --test web --no-fail-fast -E 'test(radio_create_rejection_retains_draft_and_localizes_name_help) or test(russian_catalog_has_every_source_message) or test(every_offered_interface_language_has_a_catalog)'
```

## Iteration 35 verified coverage

The live-setup survey selected 13 cases: **11 pass and two are explicitly
skipped**. Capture choices or encoder fallback pass in all six scripted browser
configurations; camera preview/cancellation pass in Chromium and Firefox at
both widths. This WebKit runtime lacks MediaRecorder, so its two preview cases
are skipped, not counted as passes. The no-JavaScript 320px journey covers
external-encoder management, playback settings, planned starts, calendar feeds
and native viewing routes.

Preview uses synthetic media acquisition and confirms that no publishing
connection starts before Go live, then verifies that cancelling stops acquired
tracks. Device choices and the publisher region have automated accessibility
checks in light/dark appearance. This is not physical-device qualification,
complete broadcasting coverage or a screen-reader speech result. Existing
screenshots/traces may contain disposable stream credentials; only aggregate
results and hashes are published here. No production interface changed, so no
before/after pair is needed.

Evidence: [11 passes and two skips](media/accessibility-2026-10-19/live-setup-verified.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/live-setup-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y live-broadcast.spec.mjs --grep 'browser capture choices|camera preview stays private|live authoring retains'
```

## Iteration 36 verified coverage

After correcting an overly anchored filter that selected no cases, the initial
three-case live-publishing survey passed one-shot replay and native external
encoder replay. The camera/screen test expected selecting capture to publish
immediately, but the current product correctly stays in private preview. The
test now verifies the waiting state and activates the existing Go live control.
No product behavior changed to satisfy that outdated assumption.

The corrected full selection has **nine passes and four explicit skips**.
Chromium/Firefox at both widths pass camera and screen publication, stopped
capture, successive sessions with distinct playable replays, and one-shot
publication/replay conversion. HLS segments and replay decoding use the real
local pipeline. The no-JavaScript case passes native external-encoder replay.
Four WebKit publisher cases skip because this runtime lacks MediaRecorder;
its setup/fallback results are recorded separately in iteration 35.

Capture sources and encoder input are synthetic; UI actions include pointer
operations and programmatic media playback as well as keyboard interaction.
This does not qualify physical devices, public federation, every encoder or
all broadcast failures. Raw traces/screenshots can contain disposable stream
keys and are not published. This is a test/coverage checkpoint without a
production visual change, so no screenshot pair is needed.

Evidence: [two passes and one initial failure](media/accessibility-2026-10-19/live-publish-initial.json),
[nine passes and four skips](media/accessibility-2026-10-19/live-publish-verified.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/live-publish-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y live-broadcast.spec.mjs --grep 'browser camera and screen publish through one stable broadcast|one-shot browser broadcast becomes a playable replay|@no-js external encoder replay stays accessible in native video controls'
```

## Iteration 37 verified coverage

The core survey applies the four user overrides in
[WCAG 2.2 1.4.12](https://www.w3.org/WAI/WCAG22/Understanding/text-spacing.html):
1.5 line height, 2em paragraph spacing, .12em letter spacing and .16em word
spacing, without changing other style properties. These are test overrides,
not new defaults or a required accessibility setting. The existing CSP blocks
inline style injection; the test uses the loaded same-origin stylesheet's
CSSOM and asserts computed values instead. Post pages need not have an `h1`,
so the metric assertion samples `body`.

The corrected baseline has ten passes and four failing Russian narrow cases.
Its original `innerWidth` guard missed Chromium mobile overflow because that
value expanded with the overflowing page; the test now measures document
`clientWidth`. Russian composer actions also overflow at normal spacing. The
four-property CSS change lets the existing action row wrap and stay within its
parent. Preview and Publish/Reply retain their labels and DOM/keyboard order.
No controls, field requirements or extra action steps were added.

All **14 English/Russian browser cases pass**, including 320px native forms
without scripts. They sample login/signup presentation, composition, published
reading and profile fields; composition/publication completes with overrides
applied. The 70 spaced-page observations and 28 default composer/reply
observations have no page overflow; the sampled headings, labels, summaries
and buttons report no clipping candidates. This is not an exhaustive content,
text-resizing, browser-zoom or assistive-technology qualification. Authentication
and signup completion under these overrides are not asserted by this survey.

The gallery includes reviewed normal and widened-spacing composer/reply pairs
at both widths. Baseline narrow screenshots retain their overflowing document
width; corrected captures fit the 320px viewport. Do not shorten translations,
hide button labels or reduce a user's chosen spacing to make the row fit;
wrapping preserves the existing controls and customization.

Evidence: [corrected baseline](media/accessibility-2026-10-19/text-spacing-corrected-baseline.json),
[14 passing cases](media/accessibility-2026-10-19/text-spacing-candidate.json),
[98 geometry observations](media/accessibility-2026-10-19/text-spacing-geometry.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/text-spacing-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y text-spacing-journey.spec.mjs
```

## Iteration 38 verified coverage

All four baseline dock cases reproduce covered focus: partial overlap with the
collapsed player and fully hidden controls with the expanded queue. The desktop
account row is also covered. This work follows
[focus-not-obscured guidance](https://www.w3.org/WAI/WCAG22/Understanding/focus-not-obscured-minimum.html).
Tests ask for the whole sampled control to remain visible, beyond the AA
minimum. The standard's exception for user-opened content that can be revealed
without changing focus is not exhaustively assessed here, so these results are
not a blanket AA-failure classification or conformance claim.

The dock's measured height now reserves focus-scrolling space and sufficient
page padding. The desktop sidebar occupies the remaining height, retaining its
native scroll areas and existing account controls. A ResizeObserver tracks
content-size changes, with explicit updates on open/close and window resize.
The first candidate passed 16 of 19 cases; WebKit needed a conditional fallback
when native focus scrolling still left a page control covered. That fallback
keeps focus in place and moves only enough to reveal it, bounded to avoid
scrolling the top of a large control offscreen. It excludes the main landmark,
fixed overlays and fullscreen content.

All **19 final cases pass**: collapsed/expanded focus in six scripted browser
configurations, desktop sidebar Tab traversal, closing and clearing the reserved
space, and seven existing listener journeys including native forms without
scripts. The frame-aligned baseline still fails all four cases. The gallery
contains reviewed field and desktop-account pairs; it also now exposes the
previously captured desktop pairs for recent iterations alongside their narrow
comparisons. Geometry evidence records focused controls, not screen-reader
speech. Very short windows, every queue size and fixed-dialog interactions are
not exhaustively qualified.

Do not make the player/queue modal, require closing it before editing, or stop
playback to reach a field. Those options would add steps or interrupt listening.
The existing persistent player and native controls remain usable together.

Evidence: [four reproduced baseline cases](media/accessibility-2026-10-19/music-dock-focus-frame-baseline.json),
[first 16/19 candidate](media/accessibility-2026-10-19/music-dock-focus-verified.json),
[19 passing final cases](media/accessibility-2026-10-19/music-dock-focus-final.json),
[focus geometry](media/accessibility-2026-10-19/music-dock-focus-geometry.json),
[source/executable hashes](media/accessibility-2026-10-19/music-dock-focus-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y music-dock-focus.spec.mjs music-listener-journey.spec.mjs
```

## Iteration 39 verified coverage

All seven video-alternatives cases pass. The journey authors a local video with
optional description/transcript metadata and WebVTT, adds chapters, publishes,
adds/replaces/removes a French caption track, confirms obsolete resources are
unavailable, decodes video with active caption cues, reads timed captions as
text, seeks from the transcript, edits alternatives and removes chapters.
Native forms include an oversized-caption refusal that retains the selected
language. The transcript stage deliberately resizes to 320px, including tests
that started at desktop width.

The initial three-case survey passed the native journey and reported a timer
contrast flag in both scripted forced-colours cases. The sampled painted timer
is black on white; the test now follows the existing representative-page policy:
exclude RGB contrast scanning only while forced colours are active, retain all
other selected rules, and keep normal-theme contrast checks. The reflow assertion
also uses document `clientWidth` so mobile overflow cannot enlarge its reference.
See the [original forced-colours appearance](media/accessibility-2026-10-19/video-alternatives-forced-colours.png).
This is a scan limitation/review boundary, not a claimed product contrast fix or
exhaustive operating-system high-contrast qualification.

Automated file selection, programmatic native playback/track selection and API
checks supplement UI interaction. This validates metadata handling and caption
transport/rendering, not linguistic caption accuracy or spoken audio description.
The synthetic fixture's author declarations are not a media-conformance claim.
Optional alternatives remain optional; no author certification or new publishing
requirement is introduced. No production UI changed, so no screenshot pair is
required for this checkpoint.

Evidence: [one pass and two initial flags](media/accessibility-2026-10-19/video-alternatives-baseline.json),
[seven passing cases](media/accessibility-2026-10-19/video-alternatives-verified.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/video-alternatives-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y video-accessible-metadata.spec.mjs
```

## Iteration 40 verified coverage

Closing the music dock hides its focused close button and leaves focus on the
page body, even while an upload draft is being edited. Both baseline widths
reproduce this and still complete the subsequent private upload. The change
returns keyboard focus to the last available page control, or main content if
that control was removed, hidden or disabled. It only restores focus when the
close action hides the currently focused player control; no new interaction or
mandatory dismissal is added. Focus continuity is relevant to 2.4.3, not a claim
that WCAG mandates one particular restoration target.

All **25 cases pass**: six close/continue journeys, twelve dock/queue focus
journeys and seven existing listener journeys. The new path preserves the draft,
returns focus to its title input, and completes a subsequent private upload.
The gallery shows the restored focus outline at both widths. Removed/hidden
previous-target fallbacks are source-reviewed rather than exhaustively simulated;
actual screen-reader announcements remain a manual-review boundary.

Evidence: [both reproduced baselines](media/accessibility-2026-10-19/music-dock-close-baseline.json),
[25 passing cases](media/accessibility-2026-10-19/music-dock-close-verified.json),
[source/executable hashes](media/accessibility-2026-10-19/music-dock-close-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y music-dock-close.spec.mjs music-dock-focus.spec.mjs music-listener-journey.spec.mjs
```

## Iteration 41 verified coverage

The initial three 800×360 cases reproduce the queue covering page focus; an
expanded six-case baseline at 360px and 320px heights confirms it in all three
engines. The queue now sizes its existing scrollport around the measured player
controls, leaving editing space while keeping a minimum scrollport for queue
controls. Normal-height layouts retain their previous maximum queue size.

The initial combined run passed all 25 existing dock/close/listener cases. Its
12 new landscape cases reached the queue playback control but used the visible
word “Play” instead of its accessible name “Play this track.” After correcting
that selector, all **12 focused landscape cases pass on the same executable**.
The final evidence is 25 related passes plus 12 focused passes, not a claimed
passing rerun of all 37 together. Both the page field and a queue control are
checked within their visible bounds, and the queue control starts playback.
The frame-aligned baseline keeps six reproduced failures after the selector
correction.

Reviewed before/after pairs show page focus and queue-control focus at 800×360
and 800×320. Tests check focused components; they do not guarantee that every
surrounding label/context is simultaneously onscreen in a short window. Smaller
windows, physical device rotation and virtual-keyboard behavior remain outside
this bounded matrix. Do not require portrait orientation or closing the queue
to continue editing; that would add a required user action. The existing queue
scrolling is reused instead of introducing another control or mode.

Evidence: [six baseline cases](media/accessibility-2026-10-19/music-queue-landscape-corrected-baseline.json),
[25/37 initial combined run](media/accessibility-2026-10-19/music-queue-landscape-verified.json),
[12 passing focused cases](media/accessibility-2026-10-19/music-queue-landscape-focused.json),
[focus geometry](media/accessibility-2026-10-19/music-queue-landscape-geometry.json),
[source/executable hashes and follow-up scope](media/accessibility-2026-10-19/music-queue-landscape-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y music-queue-landscape.spec.mjs
```

## Iteration 42 verified coverage

The CSV baseline at both Chromium widths reproduces a refused empty file
resetting Domain blocking / Overwrite to Following / Merge. It also leaves
keyboard focus away from the explanation. The candidate keeps validated type
and mode on completed upload refusals, focuses the localized error and explains
that the file must be selected again. Only enum choices travel in the redirect;
CSV contents and filenames are not placed in the URL. The existing separate
review-and-confirm step remains in place.

All **16 server/catalog checks pass**, including the existing worker, quota,
ownership, suspension and import parsing checks. All **seven browser journeys pass**, including native controls without JavaScript.
Selected axe rules pass on the six scripted error pages; the error summary
receives native focus and narrow layouts do not overflow. The latest server
lint still reports the seven unchanged discovery/search errors.
The browser journey corrects the file, reviews and confirms the import, waits
for the worker and downloads the resulting domain-blocking CSV. Its `.example`
domains are synthetic and do not cause remote account operations.

Early rate/quota gates still reject before reading the body, so cannot retain
choices they have not read. Unknown types and malformed multipart remain
outside this recovery change. Do not buffer blocked uploads merely to recover
form choices. Do not require another confirmation step or file transformation;
those would add user work. File re-selection follows browser security behavior,
not a new application requirement.

The gallery includes reviewed before/after pairs at both widths: the new focused
summary adds a re-selection explanation, and the original choices remain set.
No new required fields or workflow steps were added.

Evidence: [two reproduced baselines](media/accessibility-2026-10-19/import-export-baseline.json),
[seven passing journeys](media/accessibility-2026-10-19/import-export-verified.json),
[source/executable hashes](media/accessibility-2026-10-19/import-export-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y import-export-journey.spec.mjs
./dev test -p plamenu --lib --test import --no-fail-fast -E 'binary(import) or test(russian_catalog_has_every_source_message) or test(every_offered_interface_language_has_a_catalog)'
```

## Iteration 43 verified coverage

Both Chromium baseline widths reproduce lost handles and absent error focus
on alias rejection and migration password rejection. The migration correction
then reaches handle validation, where the baseline also loses the handle.
The candidate retains the trimmed handle and focuses the existing localized
explanation. Passwords remain blank and are never included in the redirect.
Retention is encoded in the existing error redirect and escaped by the HTML
renderer. Removal errors have no editable handle to restore.

The browser journey uses invalid handles and a wrong password, causing no
remote requests or real account move. Integration checks use the repository's
federation stub to complete alias add/remove and a successful move following
password correction. This is not browser-level remote-server interoperability.
Russian recovery and escaped hostile handle text are included in server checks.
All **16 migration integration checks pass**. All **seven browser journeys pass**,
including native forms at 320px without JavaScript. Six scripted error pages
pass selected axe rules. The final browser matrix also passes seven archive
journeys, recorded separately below. Server lint retains the same seven
unchanged discovery/search diagnostics, with no migration diagnostic.

Both forms have reviewed desktop/mobile before/after pairs in the gallery.
The visible change is retained input and a focus outline on the existing error.
Evidence: [two reproduced baselines](media/accessibility-2026-10-19/migration-recovery-baseline.json),
[14 passing migration/archive cases](media/accessibility-2026-10-19/migration-archive-verified.json),
[source/executable hashes](media/accessibility-2026-10-19/migration-recovery-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y migration-recovery-journey.spec.mjs
./dev test -p plamenu --lib --test migration --no-fail-fast -E 'binary(migration)'
```

No new verification channel or confirmation screen is introduced: either would
add a requirement beyond the existing password and alias safeguards. Migration
cooldown, target authorization and server rules are unchanged. Low-level API
failures, external-server outages and technical-detail language remain separate
coverage boundaries; the change handles existing typed form refusals.

## Iteration 44 verified coverage

The initial archive journey passes in both Chromium widths: native request,
ready-state polling, cooldown explanation, keyboard download and non-empty ZIP
signature. All **seven browser configurations pass**, including 320px native controls.
All **ten archive integration tests pass**, covering ZIP contents/media, size caps,
streaming, range download, concurrency, cooldown, deletion cleanup and worker
races. Browser checks do not inspect private archive contents. Raw archives and
traces remain excluded from committed review evidence. No production interface
changed, so no before/after pair is needed for this coverage checkpoint.

Evidence: [seven archive cases within the passing 14-case run](media/accessibility-2026-10-19/migration-archive-verified.json),
[scope, source hashes and server test names](media/accessibility-2026-10-19/archive-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y archive-journey.spec.mjs
./dev test -p plamenu --lib --test archive --no-fail-fast -E 'binary(archive)'
```

## Iteration 45 verified coverage and unresolved blocker

The virtual-authenticator experiment stops before registering a key in both
Chromium widths. The current library sends `userVerification: preferred`,
`residentKey: discouraged` and `credentialProtectionPolicy:
userVerificationRequired`. Chromium rejects that combination. This matches
[upstream issue 490](https://github.com/kanidm/webauthn-rs/issues/490), and is a
functional access blocker, not evidence of a completed security-key ceremony.
The generated options are recorded without challenges, identifiers or secrets.

Do not force PIN/biometric verification to satisfy that policy: it would add a
requirement for existing users and keys. Do not silently weaken the credential
protection policy in an accessibility change either. A compatible upstream or
security-reviewed policy resolution remains necessary. Authenticator-app and
recovery-code sign-in remain available; they do not excuse the broken optional
key path. Physical-device behavior remains unverified.

A separate controlled service failure reproduces keyboard focus falling away
from Add security key after the disabled button is restored. Both baseline
widths continue through recovery-code sign-in and disabling opt-in two-factor
authentication. The focus fix reuses the existing conditional restoration helper, so a reader
who moves to another control keeps that position. Six scripted cases pass,
including delayed failures after moving focus. The initial native case exposes
another defect: grid display overrides the enrollment form's hidden state.
The candidate CSS now honors that state, leaving the existing explanation and
working authenticator/recovery-code paths. All **seven final enrollment/fallback
cases pass**, including both native widths and delayed failures after moving
focus. These are seven passes within the mixed 34-case run: its other results
are 12 video passes, 11 failures (two existing-key baselines, seven stale channel
navigation checks, two forced-colour history scans), and four explicit virtual-
authenticator skips. This is not a claim that the whole mixed run passed.

The initial six-pass/one-failure matrix is retained as evidence; the native
failure is a product defect, not silently reclassified as a test issue. Before/
after captures use the existing fieldset directly to avoid offscreen fixed-link
artifacts in full-page screenshots. Twelve existing WebAuthn server tests pass,
including garbage-attestation rejection, registration-state/CSRF behavior,
removal and Russian interface text. They do not prove a successful production
browser ceremony.

The shared gallery shows original before/after fieldset captures for both focus
restoration and native hiding, plus explicitly unresolved registration-blocker
captures. The change adds no mandatory field, mode or authentication method.
The existing-key sign-in handler and its hidden-state issue remain separate
iteration-48 work.

Evidence: [initial blocked virtual registration](media/accessibility-2026-10-19/security-key-baseline.json),
[non-secret generated options](media/accessibility-2026-10-19/security-key-registration-options.json),
[focused baseline pair run](media/accessibility-2026-10-19/security-key-focus-framed-baseline.json),
[initial six passes and native failure](media/accessibility-2026-10-19/security-key-focus-verified.json),
[native baseline](media/accessibility-2026-10-19/security-key-native-login-baseline.json),
[all seven final enrollment cases within the mixed run](media/accessibility-2026-10-19/security-video-management.json),
[source/executable hashes and exact scope](media/accessibility-2026-10-19/security-key-focus-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y security-key-journey.spec.mjs
./dev test -p plamenu --lib --test webauthn --no-fail-fast -E 'binary(webauthn)'
```

## Iteration 46 verified coverage

The additional client matrix uses the frozen iteration-43 executable. It covers
new/existing audio and video in ordinary Notes, distinct notification/message
inboxes and synchronized read state, private mentions through three compose
surfaces, denial of private embeds, and a 13-page visual/navigation survey.
**42 cases pass**; five duplicate connected-screen surveys are explicitly skipped
because that 26-state catalogue runs once in Chromium desktop. The scripted
journeys run in all three engines at both widths; native private mentions and
inbox state also pass. The pending security-key focus change is absent from this
frozen executable and is verified separately.

Before running, secondary contexts adopt the browser project settings and the
existing isolated account helpers. Notification recipients use bounded valid
handles rather than the 31-character no-JavaScript creator identifier. Reflow
checks use `documentElement.clientWidth`, avoiding the mobile `innerWidth`
expansion that can hide document overflow. These are harness corrections, not
product-interface changes.

The 13-page survey checks 390px dark and 1440px light states; it does not claim
every combination of width and theme. Media playback includes direct browser
API inspection and file selection, so these are not wholly keyboard journeys.
The existing optional accessibility fields remain optional. No production UI
changed in this checkpoint; no before/after comparison is needed. All 19
accessibility-harness unit tests also pass.

Evidence: [42 passes and five explicit skips](media/accessibility-2026-10-19/client-journeys-initial.json),
[source/executable hashes and scope](media/accessibility-2026-10-19/client-journeys-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y client-consistency.spec.mjs private-mentions.spec.mjs status-embed.spec.mjs client-navigation-review.spec.mjs
python3 -m unittest discover -s scripts/tests -p 'test_accessibility*.py'
```

Iteration numbers follow when work started. This coverage checkpoint completes
before iteration 45's focus fix; its frozen executable and scope are explicit.

## Iteration 47 verified coverage

The initial matrix covers channel invitation/acceptance, editor permissions,
revocation, ownership transfer/deletion, duplicate playlist entries and clip
playback, playlist privacy/deletion, and opt-in history/resume controls. It uses
the frozen enrollment candidate. Known fixture conventions are applied before
running: isolated participants, inherited browser settings, 320px native
channels/playlists and actual client-width reflow measurements.

The run identifies stale assumptions before completing all journeys: upload
no longer exposes the workspace Channels link, and the history scan applies
RGB contrast measurement while forced system colours are active. The planned
harness correction will navigate through the current Videos screen and apply
the already documented forced-colour scan policy. The initial run will finish
before tests are edited, and its failures will remain recorded. No video
production change has been made.

The corrected 14-case run passes 13. WebKit desktop history times out during
Upload video and its cleanup navigation then stalls beyond the ordinary test
limit. Only that disposable MiniBrowser process was terminated, letting the
remaining cases finish. The interrupted case is not a pass; its root cause is
not established. The focused repeat **passes on the same executable and unchanged test**.
The final evidence comprises **seven playlist passes** from the initial mixed
run, **13 channel/history passes** from the corrected run and **one isolated
WebKit history pass**. No passing full-21 rerun is claimed.

The [forced-colour image](media/accessibility-2026-10-19/video-history-forced-colors.png)
shows dark time text on the system-painted light control surface; it does not
qualify all operating-system high-contrast themes. Normal-theme contrast checks
remain enabled. The time display is also visually truncated in this narrow
capture, a separate reflow investigation rather than a problem repaired by the
scan-policy correction. The seek slider still exposes the full time through
`aria-valuetext`; visual access needs further checking.

All channel flows now complete invitation/acceptance, editor access, revocation,
explicit ownership transfer and deletion. Playlists preserve duplicate clip
identity/order, decoded clip bounds, private access and underlying video after
playlist deletion. History covers opt-in storage, explicit position correction,
resume, privacy settings and native fallback. Generated clips and local actors
are used; this is not remote federation, physical-device or speech-output
qualification. No production video interface changed, so no before/after pair
is required for this coverage checkpoint.

Evidence: [initial mixed run](media/accessibility-2026-10-19/security-video-management.json),
[13 corrected passes and interrupted case](media/accessibility-2026-10-19/video-management-followup.json),
[focused WebKit pass](media/accessibility-2026-10-19/video-history-webkit-recheck.json),
[source/executable hashes and interruption scope](media/accessibility-2026-10-19/video-management-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y video-channels.spec.mjs video-history.spec.mjs video-playlists.spec.mjs
```

## Iteration 48 verified coverage

To inspect sign-in for an already registered key independently of the enrollment
blocker, this experiment tightens one disposable fixture enrollment response
from preferred to required user verification. A virtual authenticator performs
that verification; the real server still validates and stores its attestation.
This changes no production setting and is explicitly not qualification of the
normal enrollment request. After setup, sign-in uses the production challenge,
assertion and credential-removal flow, with one controlled service refusal.
Both initial Chromium cases complete real key authentication and removal,
while their soft assertions reproduce lost focus after the controlled refusal.
An extended baseline additionally finds the hidden sign-in key control exposed
without JavaScript; native recovery-code sign-in still completes. Both corrected Chromium journeys **pass**, including native recovery-code
sign-in at each width, focus restoration, delayed refusal after moving to the
code field, real key assertion, removal and two-factor cleanup. Four non-Chromium
virtual-authenticator cases are explicitly skipped; CDP coverage is limited to
Chromium. No real hardware or user credential is involved.

The two passes sit within a 33-case mixed run: seven retained-source edit cases
also pass, while 20 language/import cases stop at a missing fixture channel.
Those setup failures are recorded in iteration 49, not hidden as successes.
The CSS applies to the shared key prompt, but physical authenticator behavior,
OAuth key ceremonies and production enrollment compatibility are not qualified.
The existing registration-policy blocker remains unresolved.

The gallery has reviewed desktop/mobile pairs for sign-in focus and native
hiding. No authentication requirement, secret persistence or extra step is
added. The changes reuse the established conditional focus helper and honor
already-present hidden markup.

Evidence: [extended baseline within the mixed run](media/accessibility-2026-10-19/security-video-management.json),
[final two passes and explicit skips within the mixed run](media/accessibility-2026-10-19/key-login-video-editing.json),
[baseline fixture exception and hashes](media/accessibility-2026-10-19/security-key-login-baseline-manifest.json),
[final executable/source hashes and scope](media/accessibility-2026-10-19/security-key-login-verification-manifest.json).

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y security-key-login-journey.spec.mjs --project=chromium-desktop --project=chromium-mobile
```

## Iteration 49 verification

The next bounded matrix covers language selection and help-panel placement,
English/Russian narrow labels, retained optional alternatives after a refused
source import, and private reprocessed/trimmed replacement candidates. It uses
local generated video. A successful remote import and scheduled-source discovery
require a separate recorded PeerTube fixture and remain untested here.

Before running, source editing uses the established channel-creation navigation
helper, source/import reflow checks measure actual client width, and native
forms start at 320px. These setup changes affect the harness. All seven source-edit cases pass in the initial mixed run; 20 language/
import cases fail because their isolated accounts have no video channel yet.
The correction uses the existing channel-creation journey before inspecting
import controls. The import error renderer also lacks error focus; a focused
baseline now confirms that in all seven profiles, while all 13 language/help
cases pass. The candidate adds native focus to the existing error summary,
without adding fields, confirmation steps or import permissions. All seven corrected import journeys and all 13 language/help journeys pass on
the frozen candidate. All five `video_source_import` server tests pass, including
retention, focused errors, no fetching on refusal and the accepted import path.
The service was explicitly rebuilt before the final browser run. Server-only
Clippy still reports the same seven existing discovery/search errors.

The combined final report has **20 passes and six failures**: all six failures
belong to the separate, unfixed timer investigation A59. It is not an all-green
suite. Seven retained-source edit journeys passed in the earlier mixed run;
they were not rerun after the unrelated import-error markup change.

Evidence: [baseline](media/accessibility-2026-10-19/video-import-language-baseline.json),
[final combined report](media/accessibility-2026-10-19/video-import-clock-baseline.json),
[verification manifest](media/accessibility-2026-10-19/video-editing-verification-manifest.json),
and the import-focus section of the [comparison gallery](media/accessibility-2026-10-19/review.html).
Original desktop/mobile pairs show the existing error acquiring a focus outline.
The generated channel name differs between runs because the timer fixture created
another channel; this is fixture data, not a product change. No additional field,
required step or permission was introduced.

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y video-language-ux.spec.mjs video-secondary-ux.spec.mjs video-source-edit.spec.mjs
./dev test -p plamenu --lib --test video_source_import --no-fail-fast -E 'binary(video_source_import)'
```

## Deferred timer investigation — A59

A real generated-video upload, publication, play and pause journey reproduced
visible time truncation in all six scripted browser profiles. At a 320px viewport,
Chromium desktop allocated 35px for `0:00 / 0:03`, requiring 64px normally and
83px with WCAG text spacing. The 1440px states fit. Existing buttons retain their
44px targets; full time remains available in the seek slider's accessible value.
This is a specific visible-information concern, not a whole-page conformance verdict.

No timer product change was made: work stopped at the user's wrap-up request,
not because a repair inherently violates the no-additional-burden rule. A future
candidate could let recorded-video time wrap onto another row while preserving
all existing controls. Check long durations, live-player differences, spacing and
both themes before choosing that layout; do not add another settings step or
shrink controls to make room.

The [six-profile geometry](media/accessibility-2026-10-19/video-clock-baseline-geometry.json),
[manifest](media/accessibility-2026-10-19/video-clock-baseline-manifest.json),
[ordinary baseline image](media/accessibility-2026-10-19/video-clock-unresolved-mobile-default.png)
and [text-spacing baseline image](media/accessibility-2026-10-19/video-clock-unresolved-mobile-spaced.png)
are retained. These are **unfixed baseline images**, not a before/after pair.
The six failures are in the final combined report linked in iteration 49.

At the 19 October handoff,
`accessibility/tests/video-clock-readability.spec.mjs` was marked `test.fixme`
with A59. The 22 October gate work removed that annotation and reproduced all
six failures in the complete browser run. The repair now allows the clock its
intrinsic width and a separate row in narrow players, preserving the existing
controls and target sizes. Forced-colour controls use the system palette.
All six scripted timer profiles passed the focused 22 October run, including
320px text spacing and 44px controls. The same run passed the distribution
journey's forced-colour checks. Its local evidence is in
`target/full-gates/20261022T122841Z-23c3d73c/browser-focused-1/automated.json`;
that partial run is explicitly not a full gate pass.

Run the active regression with:

```sh
A11Y_OFFLINE_FIXTURE=0 ./dev a11y video-clock-readability.spec.mjs
```

The original baseline executed before that annotation; the manifest records both
versions' hashes. The historical images remain failed baseline evidence; they
must not be presented as evidence for the current repair.

## Decisions and outstanding coverage

- Do not add a separate profile button or name popup just to enlarge an inbox
  target. Those add controls or interaction states; preserve the existing links
  and make their hit areas and focus outlines usable.
- Do not add a CAPTCHA, email-confirmation requirement or support-contact step
  to throttled sign-in. Those add recovery burdens. The existing waiting policy
  remains; the response now explains it in the browser interface.
- Do not require an accessibility certification gate for uploaded apps. That
  would block existing packages and add a requirement. Optional author guidance
  and catalog accessibility notes are preferable; arbitrary app content remains
  an explicit coverage gap.
- Do not make inbox menus modal or require closing them before opening a
  conversation. That adds a step or keyboard constraint. Let the existing narrow
  disclosure expand so both its actions and the conversation remain reachable.

- Do not auto-follow an account during list addition: that would create an
  unexpected relationship or require another consent step. Keep the existing
  follow control and improve recovery from its existing prerequisite.
- Preserve the existing optional-email signup path; making email mandatory
  would add a requirement unrelated to accessibility.
- Do not require authors to add descriptions/captions to every upload as a
  prerequisite for publishing. Support optional alternatives and evaluate their
  use; forced completion would violate the requested constraint. Missing
  alternatives remain a content-accessibility gap, not an exemption from WCAG.
- Do not add a separate accessibility mode or onboarding questionnaire.
  Improvements should work through existing controls and platform preferences.
- Do not permanently expand all attachment-description controls to satisfy
  stale tests. That would add clutter for every author. Tests now open the
  existing optional disclosure through its summary.
- Retain password clearing after server errors. Avoid echoing secrets into
  returned HTML; password-manager/autofill and paste remain available.
- Do not replace the anti-spam proof with a visual puzzle, mandatory email,
  or phone verification. Those options would add cognitive or account-creation
  burdens. A suitable no-JavaScript alternative remains a separate design task.
- Screen-reader speech output, physical mobile devices, and the full
  criterion-by-criterion manual matrix remain unverified. See
  [accessibility status](ACCESSIBILITY.md) and
  [release verification](ACCESSIBILITY_RELEASE.md).

## WCAG 3 direction

The draft's **No redundant entry** (2.9.4.2), **Focus order meaningful**
(2.3.3.3), and **Error notifications available** (2.5.1.1) point toward the
same improvements: retain valid work, maintain a useful keyboard position,
and explain failures. These draft requirements remain developing. Future
testing should include task completion with people using assistive technology,
not just rule scans.

## Continuing the audit

The expanded combined checkpoint and focused correction are recorded in iteration 29. Remaining boundaries include remaining first-party Webxdc localization; real security-key/browser ceremonies;
recovery expiry and mail-provider failure; concurrent/outage states; migration,
remaining live failure modes and specialist music integrations. The completed local journeys above do
not qualify remote federation interoperability or arbitrary user-authored media
and app content. Unchecked workflows are coverage gaps, not asserted failures.

For manual review now, repeat the mismatch and bad-login paths, try post actions
with an unavailable connection, and compare the gallery in both widths. Check
error announcement and action names with a real screen reader before making
any assistive-technology or conformance claim.

Preserve baseline screenshots before editing the next interface. Record each
remaining item as untested, a reproduced defect, a deferred fix, or a proposal
rejected by the no-additional-burden constraint.
