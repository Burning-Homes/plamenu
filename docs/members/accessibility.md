# Accessibility and media alternatives

Use the interface with ordinary links, labelled forms and keyboard controls.
Many workflows remain available without JavaScript. Browser capture and
interactive apps require it; the video and music guides explain their playback
fallbacks. These features are not a claim of complete WCAG conformance.

When posting, describe meaningful images with alt text; mark an image decorative
only when it conveys no information. For audio, provide a transcript. For video,
provide both spoken/sound information and important visual information:

- **Visual description** explains meaningful visuals and on-screen text.
- **Transcript / written alternative** describes dialogue, sounds and visuals.
- **Caption languages** accepts WebVTT tracks, one per language.
- **Chapters** accepts a line such as `0:00 Introduction` for each section.
- Describe the poster as well as the film.

A soundtrack declaration says whether the existing audio contains audio
description or already conveys the visuals. It does not generate narration.
Review alternatives after replacing or trimming a film: captions can be retimed,
but a written description may no longer match the selected footage.

Viewers can open ALT, written alternatives and **Read captions as text**, select
caption languages and use chapter links. With scripts disabled, timestamp links
open the native media at that position. Remote software may omit alternatives
or discard extensions when forwarding them.

Report an accessibility problem with the page/action, browser, operating system,
assistive technology, and expected versus actual result. Do not include private
content or credentials. Maintainers track [accessibility validation](../ACCESSIBILITY_RELEASE.md)
separately from these usage instructions.
