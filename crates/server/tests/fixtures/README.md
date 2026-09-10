# Federation wire-fixture corpus (X4 + F3)

Checked-in JSON shapes of what other fediverse software actually puts on the
wire, driven through the signed inbox path by `tests/wire_shapes.rs` (X4
object-shape hardening) and `tests/content_universe.rs` (F3 non-microblog
objects). The point is that shape regressions are caught here, without a
live peer.

Fixtures address the test actors: sender `https://remote.example/users/bob`
(`RemoteUser::new("remote.example", "bob")`), recipient
`https://plamenu.test/users/alice`. JSON has no comments, so provenance lives
here — when adding a fixture, note which software and version produces the
shape.

Several of the F3 directories below (`lemmy/`, `peertube/`, `nodebb/`, …) are
lifted from **Lemmy's cross-software capture corpus** (real wire JSON captured
from 14 implementations, shipped as test assets in the Lemmy repository), with
URIs rewritten onto the test actors and, where noted, payloads trimmed. The
`Create` envelope around a bare captured *object* is synthetic (the corpus
stores objects; our inbox ingests activities); the object shape inside is
verbatim from the capture.

The pinned source is Lemmy commit
[`6c9bc1ce80d12cf1cdb1924e91d5740e91ff7601`](https://github.com/LemmyNet/lemmy/commit/6c9bc1ce80d12cf1cdb1924e91d5740e91ff7601),
primarily under `crates/apub/assets/{lemmy,discourse,lotide,mobilizon,nodebb,peertube,wordpress}/`.
The source corpus declares `AGPL-3.0`; its complete terms are the GNU AGPL v3
text already included at the repository root in `LICENSE`. Plamenu's
fixture transformations are limited to stable test URIs, trimming fields not
needed for the named behavior, and synthesizing the enclosing `Create` where
the upstream asset is a bare object. The per-file notes below identify live,
synthesized, or separately audited exceptions.

## `lemmy/` — lifted from Lemmy's corpus

- `create_page_image_and_link.json` — a link post carrying both an `Image`
  attachment and the target URL as a `Link` attachment with **`href`, not
  `url`**, plus the Lemmy/PeerTube `language: {identifier, name}` shape. The
  `Link` must land in `external_url` (never as media); the `Image` stays
  media.
- `create_page.json` — the base Page (title in `name`, HTML `content` +
  markdown `source`, community Mention tag, `Link` attachment pointing at a
  pictrs image — old-style Lemmy link post). Native ingestion: real title +
  full body, no `<h2>` compaction.
- `update_page.json` — an `Update(Page)` editing title and body; the title
  must re-hoist.

## `discourse/` — lifted from Lemmy's corpus

- `create_note_titled.json` — a socialhub capture, trimmed: Discourse posts
  are **`Note` with `name`** — a title on a plain Note, hoisted like any
  other title; the stored type stays a Note.

## `writefreely/` — synthesized to match WriteFreely's wire output

No wire fixtures exist upstream; shape matches what WriteFreely federates
(audited 2026-07-11).

- `create_note_titled.json` — a titled single-paragraph post: WriteFreely
  federates it as a `Note` whose `content` *starts with* `<h1>{title}</h1>`
  (their Mastodon workaround) while `name` carries the same title, and the
  `@context` contains a bare `{}` entry. The duplicate heading must strip.

## `lotide/` — lifted from Lemmy's corpus

- `create_page_title_only.json` — a `Page` with **no `content` at all**
  (title-only link post), `summary` duplicating the title, an off-site
  `url`, and singleton `to`/`cc` strings with Public in `cc` only
  (unlisted).

## `wordpress/` — WordPress ActivityPub plugin v9.0.2 shapes

The corpus' WordPress capture (a real dbzer0.com Article) is the shape
source, trimmed.

- `create_article.json` — titled `Article` with full never-truncated HTML
  body, four `Image` attachments, excerpt in `summary` (NOT a CW), a
  `preview` Note, `?p=` id distinct from the pretty `url`.
- `create_article_cw.json` — `sensitive: true` + `dcterms:subject`:
  WordPress replaces the excerpt with the CW text — then (and only then)
  `summary` is a content warning on a converted type.

## `mobilizon/` — one from Lemmy's corpus, one captured live

- `create_event_group.json` — **captured 2026-07-26 from the live Mobilizon
  5.2.4 peer** (`../mobilizon-test`, group outbox), URIs rewritten onto the
  test actors and the group onto `https://remote.example/@testgroup`;
  otherwise verbatim, `@context` included. The group-attributed shape, which
  is the only one that reaches a follower at all (Mobilizon refuses to be
  followed as a Person). Differs from the older corpus capture below in ways
  that matter: the activity's **`to` is a bare string** while the object's is
  an array; the activity carries **`attributedTo` (the group) alongside
  `actor` (the organizing Person)**; the object adds `participantCount`,
  `maximumAttendeeCapacity`, `remainingAttendeeCapacity`, `joinMode`,
  `commentsEnabled`, `anonymousParticipationEnabled`,
  `externalParticipationUrl` and `draft`; and the `Place` location has its own
  `id`. The machine-generated `summary` must be neither CW nor body, as below.
- `create_event.json` — a rendezvous.nomagic.uk capture, trimmed: `Event`
  with `startTime`/`endTime`, IANA `timezone`, `Place` location with nested
  `PostalAddress`, duplicated `status`/`ical:status`, machine-generated
  date/place `summary` (must be neither CW nor body), and a `Document`
  banner attachment (stays media).

## `peertube/` — lifted from Lemmy's corpus

- `create_video.json` — a 2025 tilvids.com capture, trimmed to one HLS
  playlist with two mp4 file Links: `attributedTo` is an **array of
  objects** `[{type: Person}, {type: Group}]` (author = Person, Group =
  channel), `content` is **markdown** (`mediaType: text/markdown`), media
  lives in the `url` Link tree (no `attachment` at all), `icon` carries
  thumbnail + preview, `duration` is ISO-8601, explicit `canReply`/
  `liveSaveReplay`/`permanentLive`/`latencyMode` nulls.

## `nodebb/` — lifted from Lemmy's corpus

- `create_article.json` — trimmed: `Article` with title in `name`, excerpt
  HTML duplicated in `summary`, a `preview` Note, and **explicit
  `"inReplyTo": null` / `"updated": null`** — null-tolerance coverage for
  scalar readers.

## `gotosocial/` — GoToSocial wire output (audited 2026-07)

GoToSocial serializes through the go-fed `activity` library, which emits
single-element properties as the bare value, not a one-element array. GtS
coerces *some* outgoing properties back to arrays for compatibility
(`attachment`, `content`, `oneOf`/`anyOf`, interaction-policy members,
`instrument`, `alsoKnownAs`) — but **not `tag`**, and not `to`/`cc`. An
activity's `object` is explicitly unnested when single, so Deletes carry a
bare IRI string.

- `create_note_singleton_tag.json` — a Note mentioning one person carries
  `"tag": {…}` (bare Mention object) and singleton `to`/`cc` strings, per
  GtS's own serialized output. Dropping this tag loses the mention — no
  notification, no mention row.
- `create_note_singleton_attachment.json` — `"attachment": {…}` (bare
  object). Current GtS coerces attachments to arrays on the way out, but
  pre-coercion GtS releases and other go-fed software do not, and inbound
  leniency must not depend on the sender's courtesy.
- `update_note_singleton_tag.json` — the edit of the singleton-tag Note:
  same bare-`tag` shape on the Update's embedded object, plus `updated`.
  The edit must apply in place (content, `edited_at`) without losing the
  mention, and a replayed Update must not double-apply.
- `delete_note.json` — Delete with the object as a bare IRI string
  (single `object` unnested, see above) and singleton `to`/`cc`.
  Idempotent: a replayed Delete stays a 202 no-op.

## `sharkey/` — Sharkey 2025.5.2-dev wire output

Shapes match Sharkey's ActivityPub renderer — the exact form of everything
Sharkey puts on the wire.

- `create_note.json` — the base Note: `_misskey_content` + `source`
  (misskeymarkdown), explicit `"inReplyTo": null`, note URIs under
  `/notes/{id}` rather than the actor path.
- `create_note_quote.json` — a quote of the base note. Sharkey sends
  **no FEP-044f `quote` property** (deliberately disabled because Mastodon
  then hides the fallback link) — only `_misskey_quote`/`quoteUrl`/
  `quoteUri` and the FEP-e232 `tag` Link with `rel: misskey_quote`. Must
  link as a *legacy* quote.
- `delete_note_tombstone.json` — Sharkey's Delete carries **no activity
  `id`** and wraps the target in a `Tombstone` object. Both must be
  tolerated; replay is idempotent.
- `announce_note.json` / `undo_announce.json` — boost and retraction; the
  Undo embeds the *whole original Announce* as its object. Replay is
  idempotent.
- `create_question.json` — a poll: the `type` becomes `Question`, options
  are `Note`s with tallies in `replies.totalItems`, open polls use
  `endTime` (closed ones swap it for `closed`), and there is no
  `votersCount`.
- `like_reaction_custom_emoji.json` / `undo_like_reaction.json` —
  Sharkey sends every reaction toward non-Mastodon-family peers as
  `Like` + `content` + `_misskey_reaction`, custom emoji carrying a `tag`
  Emoji with the icon image. The reaction row must keep the shortcode and
  icon URL; the embedded-Like Undo clears it.

## `as2/` — generic ActivityStreams 2 shapes

Legal AS2 variants no single peer is cited for; acceptance parity with
Sharkey and GoToSocial, which both tolerate them.

- `create_document.json` — a top-level `Document` post. Accepted as a post
  by Sharkey and GoToSocial; Mastodon drops it. Converted to a compact
  status (title + link) like the other non-Note types, attachments stored
  normally.
- `create_question_singleton_oneof.json` — a Question whose `type` is a
  one-element array and whose `oneOf` is a bare option object. The ingest
  gate has always accepted array `type`; the poll parser must agree or the
  status is silently stored poll-less.
- `create_note_e232_quote.json` — a quote carried **only** as a FEP-e232
  `tag` Link (AP `mediaType` + Misskey quote rel; Mitra's emission shape,
  what the streams/Hubzilla family sends — no flat `quoteUri` aliases),
  plus two decoys: a `text/html` Link with a quote rel (an ordinary
  hyperlink, not an object link) and a rel-less object link (a bare
  reference without quote semantics). Must link as a legacy quote of the
  Sharkey base note.
- `create_note_044f_quote.json` — FEP-044f consent handshake on the wire:
  `quote` property + `quoteAuthorization` stamp hosted by the quoted
  author's origin (test serves the stamp + quoted post via the stub).
  Must link accepted and non-legacy.
- `create_note_mentions_no_acct.json` — FEP-03c1: mentions of two actors
  *without any `acct:` identity* (no `preferredUsername`; same trailing id
  segment on one domain). Both must import distinctly under id-derived
  handles — no acct-uniqueness dependence.
