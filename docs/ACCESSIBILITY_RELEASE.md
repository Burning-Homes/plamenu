# Accessibility release verification

WCAG conformance is established per success criterion, not by an automated
score. Plamenu therefore keeps two complementary records for each release
candidate: a repeatable browser report and reviewed manual evidence. The
normative target is [WCAG 2.2 Level AA](https://www.w3.org/TR/WCAG22/); the
sampling and evidence approach follows
[WCAG-EM](https://www.w3.org/WAI/test-evaluate/conformance/wcag-em/).

## Automated browser gate

Install the pinned packages and matching browser container once, then run the
full matrix against an isolated disposable Plamenu and PostgreSQL fixture:

```sh
./dev a11y-install
./dev a11y
```

The default runner uses the digest-pinned official Playwright image so all
three engines have the same reviewed runtime on different developer hosts.
The test image additionally contains the distro encoder and PulseAudio. Its
private null sink lets headless Firefox run real Web Audio/MediaRecorder without
a physical sound card; it discards output and does not contact a host sound
server. The sink is enabled only in the test container and ends with that run.
It is not a Plamenu installation or production-service dependency.
The browsers share a disposable TLS proxy's network namespace: canonical
`https://localhost` URLs, including absolute media and caption URLs, resolve
to that fixture without request rewriting. The native server binds only to
Docker's bridge gateway, and host readiness ports bind only to loopback.
Set `A11Y_LOCAL_BROWSERS=1` only for diagnostics after installing Playwright's
local engines and host libraries. That mode uses a random loopback TLS port
and cannot verify canonical absolute media URLs; use the container mode for
video playback. A release report still requires every
project. The suite exercises representative signed-out and signed-in pages at desktop
and 320 CSS-pixel mobile viewports. Firefox uses a narrow desktop viewport
with touch enabled, not Android emulation: [Playwright documents `isMobile`
as unsupported in Firefox](https://playwright.dev/docs/api/class-testoptions#test-options-is-mobile).
The lightbox regression uses real mouse pointers
to drag and real touch taps to dismiss in touch-enabled projects; this is not
physical-device swipe/pinch coverage. It runs axe rules tagged for WCAG 2.0,
2.1, and 2.2 Level A/AA, plus document structure, duplicate-ID, skip-link,
keyboard-focus, reflow, Russian, dark-theme, reduced-motion, forced-colours,
and no-JavaScript contracts. The result is
`target/accessibility/automated.json`. A passing axe scan is only evidence for
the rules it can evaluate; it is never treated as a conformance determination.

Use Playwright arguments to narrow a diagnostic run, for example:

```sh
./dev a11y --project chromium-desktop -g 'signed-out'
```

A release report must come from the complete seven-project matrix. Narrowed,
skipped, failed, or dirty-source reports are rejected.

## Criterion and manual evidence gate

`accessibility/criteria.json` inventories every WCAG 2.2 Level A and AA
criterion. It deliberately includes obsolete criterion 4.1.1 with the W3C
status and rationale so an absent row cannot be mistaken for an audit gap.
The same catalog tracks the programme's preferred AAA targets without turning
them into a site-wide AAA claim.
`accessibility/manual-checks.json` groups the required human procedures while
preserving criterion-level traceability. Validate both with:

```sh
./dev a11y-matrix
```

After the automated matrix passes on a clean release-candidate commit, create
the manual record:

```sh
./dev a11y-evidence /secure/release-evidence.json
```

Complete every generated check with:

- `status: "pass"` only after its full procedure passes;
- the tester's name and an ISO 8601 `checked_at` timestamp;
- exact browser, operating-system, device, and assistive-technology versions
  in `environments`;
- durable screenshot, video, accessibility-tree, measurement, or test-note
  paths in `artifacts`; and
- concise observations in `notes`, including sampled content and processes.

The cross-platform procedure requires Chromium, Firefox, and WebKit at desktop
and mobile sizes; NVDA with Firefox and Chrome; VoiceOver with Safari on macOS
and iOS; and TalkBack with Chrome on Android. Other procedures cover keyboard,
zoom/reflow, text spacing, target size, contrast, error recovery, authentication,
media alternatives, authored/federated content, and no-JavaScript operation.
Do not mark a whole grouped check as passed when any listed criterion or
required environment is untested.

Validate a work-in-progress record structurally, or enforce release completion:

```sh
python3 scripts/accessibility-evidence.py validate /secure/release-evidence.json
python3 scripts/accessibility-evidence.py validate --release /secure/release-evidence.json
```

The release validator checks the exact Git revision, automated-report digest,
complete browser projects, zero failures or skips, a clean tested source, and
complete manual evidence. It does not accept averages, waivers, or a generic
"not applicable" result at the grouped-check level. Criterion-specific
conditional applicability must be explained in the check notes and any public
conformance or partial-conformance statement.

## Release command

Supply the completed record explicitly:

```sh
./dev release --accessibility-evidence /secure/release-evidence.json
```

Release preparation stops before building when the evidence is absent,
incomplete, modified, or belongs to another commit. The validated manual and
automated records are copied into the release stage, hashed with its other
outputs, and represented as a required `accessibility` check in `release.json`.
The evidence files may include tester identity or private test notes, so keep
the working copies outside Git and review them before wider distribution.

This gate proves that the recorded representative scope and processes passed;
it does not by itself authorize a broader WCAG claim. Any claim must still state
the conformance date, WCAG version and level, exact scope, relied-upon
technologies, and precisely bounded third-party content.
