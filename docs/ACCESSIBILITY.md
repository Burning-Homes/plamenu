# Accessibility requirements for contributors

Plamenu-owned navigation, forms, authoring, moderation and media workflows target
[WCAG 2.2 A/AA](https://www.w3.org/TR/WCAG22/). This is a development requirement, not a declaration of conformance.
See the [member guide](members/accessibility.md) for available controls and
[release verification](ACCESSIBILITY_RELEASE.md) for evidence requirements.

Preserve semantic HTML, meaningful labels, keyboard operation, visible focus,
error association/announcement and focus recovery after failed submissions.
Test reflow, zoom, text spacing, contrast, reduced motion and forced colours.
Dynamic updates must not steal focus or make the current task disappear.
Critical actions need clear consequences and usable confirmation/recovery.

Keep ordinary links and forms where an interaction does not require JavaScript.
Test both enhanced and no-script workflows. Media must retain its alternatives:
image descriptions, transcripts, captions, chapter navigation and explicit
soundtrack declarations. Do not turn an author's declaration into an automatic
accessibility claim or infer alternatives that a peer did not supply.

Automated browser/axe checks are necessary but insufficient. Release evidence
must cover applicable criteria and real assistive technology, including desktop
and mobile screen readers, keyboard flows, native media controls and physical
safe-area/reflow behavior. A score or passing subset cannot substitute for an
unmet criterion. Document non-applicability with a reason.

Webxdc app internals and remote content may lack accessible alternatives, but
Plamenu's launcher, consent, navigation and exit controls remain its responsibility.
Any eventual conformance statement must identify scope and third-party content
precisely. Until complete current evidence exists, do not claim WCAG 2.2 AA.
