# Music mobile journey correction

Requested 13 October 2026 after the user's phone screenshots disproved the
previous visual acceptance. This supersedes the mobile usability acceptance
in `MUSIC_UX_REDESIGN_CHECKLIST.md`; protocol and decoding results remain evidence
of those properties, not evidence of a usable interface.

## Findings and intended behavior

- Full-size remote artwork overrode the layered thumbnail rule. The row still
  fit the viewport, but its title and Play button wrapped almost character by
  character. An overflow-only assertion could not catch this failure.
- Audio posts offered a primary Play button opening another window, native
  audio controls, a second music Like, Save and subscription buttons before
  the normal social actions. Listening had competing entry points.
- Primary Play should open the selected audio in this tab. A separate player
  for continued browsing remains an explicit advanced choice. Viewing a track
  must continue to leave the queue unchanged.

The corrected landing page shows six entries from each source catalogue, with
continuation links, instead of putting up to 120 rows before social audio posts.
The player presents transport controls first; queue editing, repeat, shuffle and
destination settings are available on demand. Native post audio uses one player.

## Sequential checklist

- [x] Inspect the screenshots and trace the artwork cascade failure.
- [x] Correct artwork constraints and reserve readable space for row text.
- [x] Give audio posts one primary player and contextual secondary options.
- [x] Make primary Play use this tab; retain the separate advanced player.
- [x] Add real-artwork regression checks at phone widths, beyond overflow.
- [x] Ensure song and episode detail pages also have one primary native player.
- [x] Run fast checks/Clippy first, then affected server and browser tests.
- [x] Inspect real Songhive content and the social audio card manually.
- [x] Signed commit, deploy staging, verify and supply replacement screenshots.

Evidence goes under `.dev/songhive/music-ux-mobile-correction-v1/`. Do not
restart standing local services. Use the disposable accessibility fixture.
This changes presentation and navigation, not federation or hot-path queries;
do not repeat the unrelated federation fleet or timing benchmarks.

## Regression evidence

The corrected real-artwork fixture reproduces the previous build’s failure: a
1024px image renders at 296px, leaves no usable text column and produces a
2100px song row with a 115px-high Play button. The regression checks image size,
remaining text width, button dimensions and row height at 320, 390, 560 and
1440px; it also plays actual uploaded audio and checks the advanced controls.

The first fixture attempt used a non-existent cover-upload field on a song
(the song form calls it Image file). That harness error was corrected before
the geometry reproduction. Fast static checks and Clippy passed, as did all
159 affected server tests. The initial 42-scenario browser run passed completely. The final compact layout,
detail-page controls, phone viewport identity and dark mode passed subsequent
qualification. The complete media repair, final browser results, staging build
240 and replacement screenshots are recorded in `MEDIA_UX_REVIEW.md`. Final
evidence is under `.dev/media-ux-review/`, including real Songhive artwork and
the published social audio card on staging.
