# Accessible Video metadata

The Video publisher at `/videos` accepts a visual description, a written
transcript/media alternative and an initial WebVTT caption track. Set the Video
language for the initial track. A transcript should include important visual
information as well as dialogue and meaningful sounds. Timed captions alone
are not a substitute for describing visuals.

The soundtrack selector distinguishes an integrated audio description from an
ordinary soundtrack that already conveys every important visual. These are
author statements, not automatically validated accessibility claims. No audio
description is generated or mixed into a file merely by selecting a statement.

On private review, use Caption languages to add/replace tracks by language and
remove obsolete tracks; Chapters accepts one `0:00 Title` per line. Both remain
available after publication, linked from the Video library/editor. The native
HTML forms work without scripts. JS adds file validation, preview and chapter
seeking without making these features script-dependent.

Edit a published post to change its visual description, written alternative or
soundtrack statement. Authorized channel editors have the same access. Preview
does not write changes. Save commits alternatives, edit timestamp and signed
federation Update together; an outbox failure rolls them all back.

Viewers can open ALT with keyboard or touch, read the written alternative and
soundtrack statement in native disclosures, choose language-specific native
captions, or follow Read captions as text. The caption transcript page explains
its limitations and also offers the author's separate visual description and
written alternative. Timestamp links point to same-origin media fragments when
scripts are off; JS seeks its inline player without navigating away.

## Client and federation contracts

Client attachments expose `description`, `transcript`, `audio_described`,
`visuals_conveyed_in_audio`, `captions` (including same-origin `transcript_url`)
and `chapters`. A JSON `PUT /api/v1/statuses/{id}` accepts additive
`media_accessibility: [{id, transcript, decorative, audio_described,
visuals_conveyed_in_audio}]`. Each entry fully replaces the structured
alternatives for that kept attachment; omission of the entire array keeps
them. `null`/empty transcript clears it. A maximum of 50,000 Unicode characters
and no NUL applies, and the two soundtrack claims cannot both be true. Ordinary
`media_attributes` continues to edit alt text/focus. IDs outside the validated
kept attachment set are ignored, never used to modify another post.

Canonical native Video Create/dereference/history/Update includes schema.org
`transcript` and explicitly namespaced `visualDescription`, `audioDescribed`
and `visualsConveyedInAudio` extensions. These do not change PeerTube's supported
description, caption or chapter vocabulary. PeerTube 8.3 accepts and ignores
unsupported alternatives; it does **not** preserve these extensions on a
PeerTube re-export, and we do not claim that it does. Compatible Video peers
can retain them across updates. Incoming alternatives apply only to the
canonical primary media, with plain-text rendering and bounded values; no
remote transcript URL is automatically fetched.

Deleted/restricted media does not gain public caption/transcript access.
Captions/chapters continue to use guarded same-origin proxy routes and author
authority; replacing captions versions their public links, and removing a
track revokes its old caption/transcript routes.

Automated browser/keyboard/axe evidence supports the functional milestone.
The independent [human AT and WCAG release gate](ACCESSIBILITY_RELEASE.md)
still requires real tester/environment records; automation is not that claim.
