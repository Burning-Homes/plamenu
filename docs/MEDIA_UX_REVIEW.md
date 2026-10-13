# Connected client usability review

The 13 October 2026 review supersedes the earlier Audio/Video repair and its
staging.240 acceptance. That earlier pass verified individual controls but
missed the connected experience. In particular, adding creation shortcuts
beside the existing type selector made the composer less coherent, and the
Music landing page still exposed its underlying catalogue organisation.
Passing those earlier tests did not establish usable journeys.

## Decisions

The review follows a listener, author, viewer and message recipient through
whole tasks. PeerTube and Songhive compatibility stays in the server protocols;
it does not justify separate local/remote transport controls, duplicate type
pickers, or exposing catalogue administration before someone can listen.

- **Music:** Discover, Your library, Live radio and Podcasts are the primary
  destinations. Tracks have compact artwork/title/artist rows and a direct Play
  action; other resource types have separate shelves. The empty library explains
  how to fill it. Secondary tools share one menu. Uploading is a focused form
  with optional metadata and explicit public/private actions. Track and album
  administration follows listening content, behind a management disclosure.
- **Listening:** Play opens a persistent dock in the current tab. Normal
  browsing, searches and supported Music actions preserve the audio element.
  The track title opens its detail/sharing page. Queue opens the queue itself;
  transport controls are not repeated inside it. Native playback and ordinary
  server-rendered navigation remain available without scripts.
- **Authoring:** One post-type picker offers Note, Article, Event, Audio, Video
  and Go live. Polls remain an addition to a Note; community-specific titled
  posts remain in the group composer. An empty composer opens Video/Live
  creation directly. A populated draft stays in place when changing workflows.
  Note attachments are independent of post type, with new files or copies of
  the author's existing uploads/tracks. Accessibility metadata is retained in
  an attachment disclosure. Copies have independent storage keys; ownership
  checks prevent attaching another person's private upload by ID.
- **Watching and broadcasting:** Local, remote and running live video share
  enhanced controls. Live seeking uses the actual DVR window and preserves
  Go live. Fullscreen gives the video the complete surface, overlays controls,
  and hides them during uninterrupted playback. Pointer, touch and keyboard
  interactions reveal controls. First-time publishing creates a default channel
  without requiring the user to invent a channel address before adding content.
- **Activity and messages:** One activity inbox has compact filters, deliberate
  read actions and secondary management options. Actor/action sentences wrap
  naturally. Direct mentions appear in Messages; their compatibility API
  notifications share read state with conversations. Opening a direct thread
  updates the returned navigation immediately and refreshes other open tabs.
  Message rows show participants and a preview rather than nested social cards.
  New message has an explicit recipient field with suggestions; unknown
  recipients cannot silently publish a message only the sender can see.

## Review method

The source checks cover templates/localisation, ownership, independent attachment
copies, first-channel creation and synchronized notification/conversation state.
Browser journeys exercise real uploads, previews, publications, reused audio and
video playback, same-tab listening, notifications and messages between accounts,
keyboard operation, live ingest/DVR, fullscreen and no-script fallbacks.

Screenshots are reviewed at 320, 390, 768 and 1440px, plus landscape video, with
light and dark themes. The adjacent-screen catalogue includes Music, library,
albums, artists, radio, podcasts, audio upload, new messages, composer, Videos,
video upload and live setup. Populated track/album/library/player states are
captured in the listening journeys. Overflow and automated accessibility checks
supplement visual evaluation; they do not replace it.

Intermediate candidates and their failed results remain under the ignored
`.dev/client-ux-review/` directory. This includes candidate hashes, separate
source-check logs, browser reports, traces and screenshots. The review caught
an inherited rule hiding Note attachments, a nested Queue disclosure, duplicate
transport controls and unknown-recipient handling. Tests were also corrected
where they assumed old labels, old disclosure structure or API authentication
that a browser session does not provide.

## Verification and staging

Local qualification completed on 13 October 2026:

- Formatting, tracked-file static checks and workspace/all-target Clippy passed.
  The live-control stylesheet refinement passed formatting/static checks and a
  fresh build, then the affected live playback and fullscreen journeys. The
  final phone catalogue rule removal passed static checks and a fresh staging
  build, followed by real-content layout checks at all four viewport widths.
- Fresh SQLx metadata comparison validated 1,739 queries; two obsolete cache
  files were removed after replacing their old conversation read queries.
- All 210 affected server/database scenarios passed. One local process ended
  after 175 passing cases; its 35 remaining cases passed separately. Subsequent
  template/interaction refinements passed 131 and then 101 relevant Rust tests.
- The latest applicable result for each of 139 browser scenarios is **135 passed,
  four skipped, zero unresolved failures**. These are combined results after
  corrections, not a claim that every test ran against every intermediate build.
  Three skips avoid duplicating the visual catalogue in other projects; the
  fourth is live capture in the bundled WebKit runtime without MediaRecorder.
- Actual live capture, encoding, HLS playback, DVR rewind/live-edge return and
  unobstructed live fullscreen passed in Chromium desktop/mobile and Firefox.
  VOD fullscreen/auto-hide, keyboard reveal and decoded playback passed in
  Chromium, Firefox and WebKit. Touch reveal was checked in Chromium emulation.
- The disposable RSS/enclosure producer was enabled for the podcast journeys;
  subscription, browsing and playback passed with scripts and without scripts.
  Stored audio, ordered playlists, revocable radio access, external Opus embeds,
  native video publication and the legacy advanced player also passed.

Visual acceptance included populated activity/messages, Note audio/video
attachments, Music track/album pages and expanded/compact player states. At
390px the tested actor/action sentence stays on one line; at narrower widths
it wraps naturally. Read message rows lose both their dot and unread border
in another open tab. Music rows keep 48px artwork and a 44px minimum Play target;
queue titles retain useful width beside compact controls. The running-live
Go live button sits inside the control row, has a 44px minimum target, and hides
at the live edge. Fullscreen captures preserve aspect ratio and contain no
permanent control bar.

The final candidate hash, per-scenario source reports and selected reviewed
screenshots are recorded in `.dev/client-ux-review/verification-summary.json`,
`candidate-8.json` and `screenshots/`. Staging acceptance is recorded separately
below after deployment.

The real-content staging review of build 242 exposed a layout case that the
sparse fixture catalogue did not: one album, one artist and one live station
created disproportionately tall cover sections. These now use compact labelled
rows, and mixed library entries identify their resource type and put playable
songs first. The affected Music journeys passed at desktop/mobile sizes and
without JavaScript after this refinement. The core staging smoke checks also
confirmed local/remote decoded video, full-surface portrait/landscape fullscreen,
Note uploads and existing-media previews, recipient suggestions, no-script
Audio authoring and Music playback continuing in the same tab.

The final phone screenshot caught a leftover two-column breakpoint on those
new catalogue rows. Removing it lets album, artist and station rows use the
available width at every size. Measurements against the real staging catalogue
at 320, 390, 768 and 1440px confirm full-width rows and readable titles; screenshots
wait for artwork decoding before visual acceptance.

Physical phone cameras and the iOS system fullscreen surface require device
qualification. Browser viewport/touch emulation is not evidence for those
hardware paths. The persistent player enhances supported same-origin browsing;
external sites and full-page transitions still use ordinary browser navigation.

### Deployed acceptance, 13 October 2026

[Staging](https://staging3.plamenu.com) reports
`0.7.0-staging.244+sha.ac2c5c967fd5`. The deployed source is
`ac2c5c967fd5cb76ea3d3e556a42c703be51c98c`, including the main client repair
`56086c3ff33b31d60163ad024181294a9e784241` and catalogue refinement
`b7bc365e7530a29b4de92178be67d522185425f0`. All three commits have locally and
Codefloe-verified signatures and DCO sign-offs.

Deployment completed successfully. The service is active/running, with zero
restarts and exit status zero. The installed executable and local stripped
artifact have the same SHA-256:
`bfa42e2fe14961288a17134e3eeb9a26cb23a74834dd7cc1a39d46be65c94379`.

The authenticated staging smoke review covered local and remote decoded video,
portrait/landscape fullscreen, Note uploads and existing-media previews,
recipient suggestions, uninterrupted same-tab Music playback, and no-script
Audio composition. Staging content was used for previews and playback; this
review did not send messages, publish posts or start a staging broadcast.
Live ingest and populated message interactions were exercised in the isolated
local fixtures described above.

Final real-content Music captures were visually reviewed at 320, 390, 768 and
1440px. All catalogue rows occupy their shelf width; no horizontal overflow was
found. The library labels each resource type and presents playable songs first.
Evidence lives under `.dev/client-ux-review/staging/`: `smoke-results.json`,
`music-244-*.png`, `music-phone-244.png`, `library-final-*.png`, the composer,
inbox and video captures, and the `*-244.txt` service/hash checks. These local
evidence files are intentionally ignored rather than published with account
content. This documentation-only acceptance commit requires no runtime redeploy.
