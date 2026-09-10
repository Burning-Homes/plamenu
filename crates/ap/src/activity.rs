//! Incoming activity envelopes and outgoing activity builders.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::AS_CONTEXT;
use crate::urls::{LocalStatusUrls, LocalUserUrls, replies_page_url};

/// The `ActivityStreams` "public" collection: addressing a post here makes it
/// publicly visible.
pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The minimal envelope every inbox delivery must carry. `object` stays raw:
/// its shape depends entirely on the activity type.
#[derive(Debug, Clone, Deserialize)]
pub struct Activity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    /// Actor IRI or embedded actor object. Absent on a FEP-7aa9
    /// `FeatureRequest`, whose actor is derived from the HTTP signature.
    #[serde(default)]
    pub actor: Value,
    #[serde(default)]
    pub object: Value,
    /// RFC 3339 publish time of the activity itself — an `Announce`'s own
    /// timestamp, which Mastodon stores as the reblog row's `created_at`.
    #[serde(default)]
    pub published: Option<String>,
    /// FEP-1b12 `audience`: the Group whose community an announced activity
    /// belongs to (Mitra sets it on the `Announce` wrapper itself).
    #[serde(default)]
    pub audience: Value,
}

impl Activity {
    /// The actor IRI, whether given as a string or as an embedded object.
    #[must_use]
    pub fn actor_id(&self) -> Option<&str> {
        id_of(&self.actor)
    }

    /// The object IRI, whether given as a string or as an embedded object.
    #[must_use]
    pub fn object_id(&self) -> Option<&str> {
        id_of(&self.object)
    }
}

/// Extracts the IRI of a value that is either a string or an object with `id`.
#[must_use]
pub fn id_of(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Object(map) => map.get("id").and_then(Value::as_str),
        _ => None,
    }
}

/// Flattens an `ActivityStreams` one-or-many property into a slice: absent or
/// null is empty, an array is itself, anything else is a singleton. go-fed
/// peers (`GoToSocial` among them) serialize single-element properties as the
/// bare value rather than a one-element array, so every plural property must
/// be read through this.
#[must_use]
pub fn one_or_many(value: Option<&Value>) -> &[Value] {
    match value {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(other) => std::slice::from_ref(other),
    }
}

/// An `Event`'s organizer, which `Mobilizon` puts in the object's own `actor`
/// rather than in `attributedTo`.
///
/// For a group event `Mobilizon` sends **both**: `actor` is the organizing
/// Person and `attributedTo` is the group the event belongs to. Nothing else in
/// the fleet puts an `actor` on an object, so this is read for `Event` only —
/// on any other type an `actor` inside an object is not an authorship claim and
/// must not be treated as one.
fn event_organizer_id(object: &Value) -> Option<&str> {
    if object.get("type").and_then(Value::as_str) != Some("Event") {
        return None;
    }
    id_of(object.get("actor")?)
}

/// The author IRI of an object's `attributedTo`, which may be a bare IRI, an
/// embedded object, or an array mixing several references: `PeerTube` sends
/// `[{type: Person, …}, {type: Group, …}]`, where the Person is the creator
/// and the Group the channel. Like Lemmy's parser, a `Person` entry wins;
/// otherwise the first resolvable IRI does.
///
/// A `Mobilizon` `Event` is the one shape where the author is not in
/// `attributedTo` at all: that names the *group*, and the organizing Person is
/// in the object's `actor`. Attributing such an event to the group loses the
/// human organizer entirely — the card then credits the group both for the post
/// and for the `Announce` that carries it, and no one can see who is running
/// the event. So an organizer wins over a `Group`-shaped `attributedTo`; a
/// Person named in `attributedTo` still wins over both, since that is an
/// explicit authorship claim.
#[must_use]
pub fn attributed_to_id(object: &Value) -> Option<&str> {
    let entries = one_or_many(object.get("attributedTo"));
    let explicit_person = entries
        .iter()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("Person"))
        .and_then(id_of);
    explicit_person
        .or_else(|| event_organizer_id(object))
        .or_else(|| entries.iter().find_map(id_of))
}

/// Every IRI an object is attributed to — for "does this object belong to the
/// verified sender" checks, where any of the listed authors may deliver it
/// (a `PeerTube` Video is attributed to both the uploader and the channel).
///
/// Includes an `Event`'s organizer for the same reason [`attributed_to_id`]
/// prefers it: the organizer is the actor that signs and delivers the `Create`,
/// so a check that only knew the group would reject the organizer's own event.
pub fn attributed_to_ids(object: &Value) -> impl Iterator<Item = &str> {
    one_or_many(object.get("attributedTo"))
        .iter()
        .filter_map(id_of)
        .chain(event_organizer_id(object))
}

/// Builds an `Accept` for a received `Follow`, echoing the original activity
/// back as the object (what Mastodon expects).
#[must_use]
pub fn accept_follow(
    domain: &str,
    local_username: &str,
    follow_row_id: i64,
    follow_activity: &Value,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    json!({
        "@context": AS_CONTEXT,
        // Mirrors Mastodon's synthetic id scheme for accepts.
        "id": format!("{}#accepts/follows/{follow_row_id}", urls.id),
        "type": "Accept",
        "actor": urls.id,
        "object": follow_activity,
    })
}

fn rewrite_actor_scoped_iris(value: &Value, legacy: &str, canonical: &str) -> Value {
    match value {
        Value::String(text)
            if text == legacy
                || text
                    .strip_prefix(legacy)
                    .is_some_and(|tail| tail.starts_with('/') || tail.starts_with('#')) =>
        {
            Value::String(format!("{canonical}{}", &text[legacy.len()..]))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rewrite_actor_scoped_iris(item, legacy, canonical))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        rewrite_actor_scoped_iris(value, legacy, canonical),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Rebase every actor-scoped IRI in a locally-built activity from the legacy
/// username layout to the actor's persisted canonical identity.
///
/// Activity builders intentionally retain their long-standing `(domain,
/// username)` API for compatibility. The server applies this once, before an
/// activity is proof-signed and delivered. Only exact actor IRIs and values
/// below the actor by `/` or `#` are rewritten; human `/@handle` URLs and
/// unrelated strings are untouched.
#[must_use]
pub fn rebase_local_identity(
    activity: &Value,
    domain: &str,
    username: &str,
    canonical_actor_id: Option<&str>,
) -> Value {
    let current_legacy = LocalUserUrls::new(domain, username).id;
    let canonical = canonical_actor_id.unwrap_or(&current_legacy);
    let prefix = format!("https://{domain}/users/");
    // Queued jobs may predate a handle rename. Derive the builder-era actor
    // ID from the document's actor field, rather than assuming the account's
    // current username; this makes already-queued activities rename-safe.
    let legacy = activity
        .get("actor")
        .and_then(crate::activity::id_of)
        .filter(|actor| actor.starts_with(&prefix))
        .map_or(current_legacy.as_str(), |actor| {
            let tail = &actor[prefix.len()..];
            let end = tail.find(['/', '#']).unwrap_or(tail.len());
            &actor[..prefix.len() + end]
        });
    if canonical == legacy {
        return activity.clone();
    }

    rewrite_actor_scoped_iris(activity, legacy, canonical)
}

/// Builds an outgoing `Follow` of a remote actor.
#[must_use]
pub fn follow(
    domain: &str,
    local_username: &str,
    follow_row_id: i64,
    target_actor_uri: &str,
) -> Value {
    follow_as_sent(
        &follow_uri(domain, local_username, follow_row_id),
        domain,
        local_username,
        target_actor_uri,
    )
}

/// Rebuilds a previously-sent `Follow` from its stored activity id, so an
/// `Undo` can reference the exact activity the remote side accepted.
#[must_use]
pub fn follow_as_sent(
    follow_activity_uri: &str,
    domain: &str,
    local_username: &str,
    target_actor_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    json!({
        "@context": AS_CONTEXT,
        "id": follow_activity_uri,
        "type": "Follow",
        "actor": urls.id,
        "object": target_actor_uri,
    })
}

/// The id our outgoing `Follow` activities carry (remotes echo it back in
/// their `Accept`/`Reject`).
#[must_use]
pub fn follow_uri(domain: &str, local_username: &str, follow_row_id: i64) -> String {
    let urls = LocalUserUrls::new(domain, local_username);
    format!("{}#follows/{follow_row_id}", urls.id)
}

#[must_use]
pub fn follow_uri_for_actor(actor_id: &str, follow_row_id: i64) -> String {
    format!("{actor_id}#follows/{follow_row_id}")
}

/// Builds a `Move`: a local account announcing it has migrated to
/// `target_actor_uri`. Mastodon's `MoveSerializer` shape — the `object` is
/// the moving actor itself, and the id is `{actor}#moves/{migration_id}`.
#[must_use]
pub fn move_account(
    domain: &str,
    local_username: &str,
    migration_id: i64,
    target_actor_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#moves/{migration_id}", urls.id),
        "type": "Move",
        "actor": urls.id,
        "object": urls.id,
        "target": target_actor_uri,
    })
}

/// A local status rendered as an `ActivityPub` `Note`.
#[derive(Debug)]
pub struct NoteParams<'a> {
    /// The AS2 object type to serve this post as — stated, never inferred (see
    /// [`PostKind`]). Both Note builders (the `Create` at post time and the
    /// object served at the post's own URL) must pass the same value, or the
    /// activity and the object it points at would disagree.
    pub kind: PostKind,
    pub domain: &'a str,
    pub username: &'a str,
    /// Persisted canonical actor ID; `None` selects the legacy username layout.
    pub actor_id: Option<&'a str>,
    pub status_id: i64,
    /// Already-sanitized HTML.
    pub content_html: &'a str,
    /// The raw text the status was authored in and its format — federated as
    /// the AP `source` property (Pleroma emits the same shape), so rich-text
    /// peers can display or re-edit the original markup (P4).
    pub source: Option<NoteSource<'a>>,
    /// RFC 3339.
    pub published: &'a str,
    /// RFC 3339 edit time; Mastodon reads it as the Note's `edited_at`.
    pub updated: Option<&'a str>,
    /// `public` | `unlisted` | `private` | `direct` | `local`.
    pub visibility: &'a str,
    /// Content warning (plain text), `None` when the post has none —
    /// federated as the Note's `summary`, like Mastodon.
    pub summary: Option<&'a str>,
    /// Marks the post (and its media) as sensitive.
    pub sensitive: bool,
    /// ISO 639 language code; emitted as the `contentMap` key.
    pub language: Option<&'a str>,
    /// `ActivityPub` id of the status this replies to.
    pub in_reply_to_uri: Option<&'a str>,
    /// Pre-built `Document` objects for media attachments.
    pub attachments: &'a [Value],
    /// Pre-built `Mention`/`Hashtag` tag objects.
    pub tag: &'a [Value],
    /// Actor IRIs of mentioned accounts (added to `cc`, Mastodon-style).
    pub mentioned_uris: &'a [String],
    /// FEP-044f quote of another post.
    pub quote: Option<NoteQuote<'a>>,
    /// Quote-approval policy bitmap, federated as the Note's `interactionPolicy`
    /// (`crate::quote_policy`).
    pub quote_approval_policy: i32,
    /// Attached poll; turns the Note into a `Question`.
    pub poll: Option<NotePoll<'a>>,
    /// IDs of up to 5 of the oldest distributable self-replies, advertised
    /// in the inlined first page of the Note's `replies` collection —
    /// remote servers use it to backfill threads, like Mastodon's.
    pub self_reply_ids: &'a [i64],
    /// Sizes of the advertised `likes`/`shares` collections (Mastodon
    /// ingests them as untrusted favourite/boost counts).
    pub favourites_count: i64,
    pub reblogs_count: i64,
    /// Thread title of a group submission: emitted as `name` and the
    /// object becomes a `Page` — Lemmy's native post shape. Mastodon renders
    /// its usual title-plus-link stub for Pages; untitled group posts stay
    /// Notes, which Lemmy auto-titles and Mastodon renders in full.
    pub title: Option<&'a str>,
    /// A titled link post's target URL, emitted as
    /// `attachment: [{type: Link, href}]` ahead of the media documents —
    /// Lemmy's link-post shape.
    pub external_url: Option<&'a str>,
    /// FEP-1b12 group the post is submitted to: stamped as `audience` and
    /// copied into `cc`, so consumers (and the group host's own inbox) can
    /// associate the object with the community.
    pub group_uri: Option<&'a str>,
    /// FEP-f228 `context`: the conversation's collection-of-posts IRI. Our own
    /// `/contexts/{id}` for a locally-owned thread, or the remote owner's IRI
    /// passed through so it threads our reply.
    pub context: Option<&'a str>,
    /// FEP-171b `contextHistory`: the conversation's collection-of-activities
    /// (container) IRI — emitted only for a private conversation whose
    /// container we run.
    pub context_history: Option<&'a str>,
    /// Event fields; turns the object into an `Event`. Set only when the
    /// author picked the event post kind — see [`NoteEvent`].
    pub event: Option<NoteEvent<'a>>,
}

/// Which AS2 object type an outgoing post is served as.
///
/// This used to be inferred from which optional fields happened to be set
/// (`poll.is_some()` → `Question`, `title.is_some()` → `Page`, else `Note`). That
/// worked while every kind had exactly one telltale field, and stopped working
/// the moment two kinds shared one: `Page` and `Article` are **both titled**, so
/// nothing about the fields distinguishes them. The kind is therefore stated by
/// the caller, never guessed here.
///
/// Making it a real dimension also makes the *consequence* explicit: `Page`,
/// `Article` and `Event` are all rendered by Mastodon as a truncated
/// title-plus-link stub, so which type we pick decides how much of the post the
/// Mastodon family can read. That is an authoring decision, never a side effect.
///
/// Only three of these are *author-selectable* (`Note`, `Article`, `Event`).
/// `Question` and `Page` are implied by attaching a poll or submitting to a
/// group, and resolved in one place server-side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PostKind {
    /// An ordinary post. Rendered in full everywhere.
    #[default]
    Note,
    /// A poll.
    Question,
    /// A titled (Lemmy-style) thread or link post.
    Page,
    /// A titled long-form post: the full body federates, and the title is
    /// carried both as `name` and as a leading heading.
    Article,
    /// A calendar event with a start time and participation rules.
    Event,
}

impl PostKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Question => "Question",
            Self::Page => "Page",
            Self::Article => "Article",
            Self::Event => "Event",
        }
    }

    /// The kind a **stored** local status is served as, from its `object_type`
    /// column plus the two implied kinds. The column is authoritative: the served
    /// object, the `Create`/`Update` activity, the client entity and the rendered
    /// card all derive from it, so none of them can disagree about what the
    /// audience received.
    #[must_use]
    pub fn of_stored(object_type: Option<&str>, has_poll: bool, has_title: bool) -> Self {
        match object_type {
            Some("Article") => Self::Article,
            Some("Event") => Self::Event,
            Some("Page") => Self::Page,
            _ if has_poll => Self::Question,
            _ if has_title => Self::Page,
            _ => Self::Note,
        }
    }
}

/// Event fields on an outgoing post, rendering it an `Event`.
///
/// Present only when the author explicitly chose the event post kind. It is never
/// inferred from "there is a date in here": an `Event` is rendered by Mastodon as
/// a truncated title-plus-link stub, so silently upgrading a post that happens to
/// mention a time would quietly cost the author most of their audience's ability
/// to read it.
#[derive(Debug, Default)]
pub struct NoteEvent<'a> {
    /// RFC 3339 start — the one field an event cannot be without.
    pub start_time: &'a str,
    pub end_time: Option<&'a str>,
    /// IANA zone name of the venue, as the organizer set it.
    pub timezone: Option<&'a str>,
    /// `free` | `restricted` | `invite` | `external`.
    pub join_mode: &'a str,
    /// Where to RSVP when `join_mode` is `external`.
    pub external_participation_url: Option<&'a str>,
    pub max_attendees: Option<i32>,
    /// Attendees we have accepted — Mobilizon's `participantCount`, which
    /// excludes the organizer.
    pub participant_count: i64,
    /// `CONFIRMED` | `TENTATIVE` | `CANCELLED` (ical vocabulary).
    pub status: &'a str,
    pub is_online: bool,
    /// The venue: a `Place` name plus whatever of the address was given.
    pub location_name: Option<&'a str>,
    pub location_street: Option<&'a str>,
    pub location_locality: Option<&'a str>,
    pub location_region: Option<&'a str>,
    pub location_country: Option<&'a str>,
    pub location_postal_code: Option<&'a str>,
}

/// Poll fields on an outgoing Note (rendering it a `Question`).
#[derive(Debug)]
pub struct NotePoll<'a> {
    pub options: &'a [String],
    /// Per-option tallies (`replies.totalItems`); zeros while totals are
    /// hidden, like Mastodon serializes `hide_totals` polls.
    pub tallies: &'a [i64],
    /// Multiple choice (`anyOf`) vs single choice (`oneOf`).
    pub multiple: bool,
    /// RFC 3339 end time.
    pub end_time: Option<&'a str>,
    /// Whether the poll has ended (adds `closed`).
    pub expired: bool,
    pub voters_count: Option<i64>,
}

/// The AP `source` property of an outgoing Note: the raw text as authored
/// plus its media type (`text/plain` | `text/markdown` | `text/html`).
#[derive(Debug)]
pub struct NoteSource<'a> {
    pub content: &'a str,
    pub media_type: &'a str,
}

/// Quote fields on an outgoing Note.
#[derive(Debug)]
pub struct NoteQuote<'a> {
    pub quoted_uri: &'a str,
    /// The `QuoteAuthorization` stamp, once granted.
    pub authorization_uri: Option<&'a str>,
}

/// `to`/`cc` for a visibility level, the way Mastodon encodes them: `Public`
/// in `to` is public, `Public` only in `cc` is unlisted, the followers
/// collection alone is followers-only, and nothing at all is direct (the
/// mentioned actors are added by [`note_addressing`]).
fn addressing(visibility: &str, followers_url: &str) -> (Vec<String>, Vec<String>) {
    match visibility {
        "unlisted" => (vec![followers_url.to_owned()], vec![PUBLIC.to_owned()]),
        "private" => (vec![followers_url.to_owned()], vec![]),
        "direct" | "local" => (vec![], vec![]),
        _ => (vec![PUBLIC.to_owned()], vec![followers_url.to_owned()]),
    }
}

/// The inverse of [`addressing`], for inbound objects. `author_followers_url`
/// is the sender's followers collection (`{actor}/followers` for every major
/// implementation): addressing it means followers-only, while addressing
/// only individual actors means direct — misreading a DM as followers-only
/// would leak it, so anything below `unlisted` without the followers
/// collection is `direct`, like Mastodon.
#[must_use]
pub fn visibility_from_addressing(
    to: &Value,
    cc: &Value,
    author_followers_url: &str,
) -> &'static str {
    fn has(value: &Value, predicate: &dyn Fn(&str) -> bool) -> bool {
        match value {
            Value::String(s) => predicate(s),
            Value::Array(items) => items.iter().any(|item| has(item, predicate)),
            _ => false,
        }
    }
    let is_public = |s: &str| s == PUBLIC || s == "as:Public" || s == "Public";
    let is_followers = |s: &str| !author_followers_url.is_empty() && s == author_followers_url;
    if has(to, &is_public) {
        "public"
    } else if has(cc, &is_public) {
        "unlisted"
    } else if has(to, &is_followers) || has(cc, &is_followers) {
        "private"
    } else {
        "direct"
    }
}

/// Visibility levels ranked broad→narrow, for clamping a reply so it can never
/// widen the audience past its conversation root (FEP-171b). `local` ranks with
/// `private` (both closed, non-public); an unknown value ranks as broad so it is
/// clamped, never used to widen.
#[must_use]
pub fn visibility_rank(visibility: &str) -> u8 {
    match visibility {
        "unlisted" => 2,
        "private" | "local" => 1,
        "direct" => 0,
        // "public" and anything unrecognised
        _ => 3,
    }
}

/// The narrower of `requested` and `root` — the reply's effective visibility,
/// never broader than the conversation root's. Ties keep `requested` (so a
/// `local`↔`private` choice, both closed, is honoured).
#[must_use]
pub fn clamp_visibility<'a>(requested: &'a str, root: &str) -> &'a str {
    if visibility_rank(requested) > visibility_rank(root) {
        // Never widen: fall back to the (narrower) root. Normalise to a known
        // literal so an unrecognised root can't leak through.
        match root {
            "unlisted" => "unlisted",
            "private" => "private",
            "local" => "local",
            "direct" => "direct",
            _ => "public",
        }
    } else {
        requested
    }
}

fn note_addressing(params: &NoteParams<'_>, followers_url: &str) -> (Vec<String>, Vec<String>) {
    let (mut to, mut cc) = addressing(params.visibility, followers_url);
    // Mentioned actors are recipients: the only ones (in `to`) for a direct
    // post, copied (in `cc`) otherwise — Mastodon's scheme.
    let bucket = if matches!(params.visibility, "direct" | "local") {
        &mut to
    } else {
        &mut cc
    };
    for uri in params.mentioned_uris {
        if !bucket.iter().any(|t| t == uri) {
            bucket.push(uri.clone());
        }
    }
    (to, cc)
}

/// Whether a body already opens with a heading that *is* the title — an author
/// who wrote their own `# Headline` gets one heading, not two. The mirror image
/// of `ingest::strip_duplicate_title_heading`, which drops such a heading on the
/// way in.
fn leads_with_heading(content: &str, title: &str) -> bool {
    let trimmed = content.trim_start();
    let title = title.trim();
    ["h1", "h2"].iter().any(|tag| {
        trimmed
            .strip_prefix(&format!("<{tag}>"))
            .and_then(|rest| rest.split_once(&format!("</{tag}>")))
            .is_some_and(|(heading, _)| crate::text::sanitize_remote_plain(heading) == title)
    })
}

/// The body as it goes on the wire: the stored HTML plus the two things a
/// receiver can only get from the content itself.
///
/// A quote post carries the quote twice — structurally, and as a visible `RE:`
/// fallback — for servers that don't understand the structural field (Mastodon
/// bakes the same fallback into its own quote posts). Quote-aware receivers
/// strip the `quote-inline` paragraph; the rest get a usable link.
///
/// A long-form post carries its headline twice for the same kind of reason: as
/// `name`, and as a leading `<h1>` — `WriteFreely`'s workaround, and the only
/// way the title survives on Pleroma, `GoToSocial`, Sharkey and Mitra, none of which
/// render `name`. Receivers that *do* hoist `name`
/// strip the duplicate heading, as our own ingest does. Only `Article` bakes:
/// Lemmy shows a `Page`'s `name` natively, so baking would double it there.
fn note_content<'a>(params: &'a NoteParams<'a>) -> Cow<'a, str> {
    let mut body = match &params.quote {
        Some(quote)
            if !params.content_html.contains(quote.quoted_uri)
                && !params.content_html.contains("quote-inline") =>
        {
            Cow::Owned(format!(
                r#"<p class="quote-inline">RE: {}</p>{}"#,
                crate::text::shortened_link_anchor(quote.quoted_uri),
                params.content_html,
            ))
        }
        _ => Cow::Borrowed(params.content_html),
    };
    if params.kind == PostKind::Article
        && let Some(title) = params.title
        && !leads_with_heading(&body, title)
    {
        body = Cow::Owned(format!(
            "<h1>{}</h1>{body}",
            crate::text::escape_html(title.trim())
        ));
    }
    body
}

/// The `Note` object itself (also served at its own URL).
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear mapping from status to wire Note; splitting would scatter the shape"
)]
pub fn note_object(params: &NoteParams<'_>) -> Value {
    let urls = LocalUserUrls::for_account(params.domain, params.username, params.actor_id);
    let status_urls =
        LocalStatusUrls::from_actor_id(params.domain, params.username, &urls.id, params.status_id);
    let note_id = status_urls.id.clone();
    let (to, mut cc) = note_addressing(params, &urls.followers);
    if let Some(group) = params.group_uri
        && !cc.iter().any(|c| c == group)
    {
        cc.push(group.to_owned());
    }
    let kind = params.kind.as_str();
    let body = note_content(params);
    let content_html: &str = &body;
    let mut note = json!({
        "id": note_id,
        "type": kind,
        "attributedTo": urls.id,
        // `summary` is the content warning; Mastodon emits it as null when
        // absent, never omitted.
        "summary": params.summary,
        "sensitive": params.sensitive,
        "content": content_html,
        "published": params.published,
        // The Note's `url` is the human web page (`/@name/{id}`), not the AP id.
        "url": status_urls.web_url,
        "to": to,
        "cc": cc,
        "attachment": params.attachments,
        "tag": params.tag,
    });
    if let Some(title) = params.title {
        note["name"] = json!(title);
    }
    if let Some(group) = params.group_uri {
        note["audience"] = json!(group);
    }
    // The link a titled post is about leads the attachments as a bare `Link`
    // (Lemmy's link-post shape); media documents follow.
    if let Some(href) = params.external_url
        && let Some(items) = note["attachment"].as_array_mut()
    {
        items.insert(0, json!({ "type": "Link", "href": href }));
    }
    if let Some(source) = &params.source {
        note["source"] = json!({
            "content": source.content,
            "mediaType": source.media_type,
        });
    }
    if let Some(updated) = params.updated {
        note["updated"] = json!(updated);
    }
    if let Some(language) = params.language {
        note["contentMap"] = json!({ language: content_html });
    }
    if let Some(parent) = params.in_reply_to_uri {
        note["inReplyTo"] = json!(parent);
    }
    // FEP-f228 / FEP-171b conversation grouping. `context` resolves to the
    // collection of posts, `contextHistory` to the collection of activities.
    // Mastodon's legacy `conversation` alias carries the same value, so a
    // Mastodon peer threads our posts into one conversation (it reads
    // `conversation`, not `context`).
    if let Some(context) = params.context {
        note["context"] = json!(context);
        note["conversation"] = json!(context);
    }
    if let Some(history) = params.context_history {
        note["contextHistory"] = json!(history);
    }
    if let Some(quote) = &params.quote {
        // The FEP-044f property plus the legacy compat duplicates Mastodon
        // also emits.
        note["quote"] = json!(quote.quoted_uri);
        note["quoteUri"] = json!(quote.quoted_uri);
        note["_misskey_quote"] = json!(quote.quoted_uri);
        if let Some(authorization) = quote.authorization_uri {
            note["quoteAuthorization"] = json!(authorization);
        }
        // FEP-e232 object link — Mitra's dual-emission: the same quote as a
        // `tag` Link with an ActivityPub `mediaType` and the Misskey quote
        // rel, for receivers (streams/Hubzilla family) that read only e232.
        if let Some(tags) = note["tag"].as_array_mut() {
            tags.push(json!({
                "type": "Link",
                "href": quote.quoted_uri,
                "mediaType": "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
                "rel": ["https://misskey-hub.net/ns#_misskey_quote"],
            }));
        }
    }
    if let Some(poll) = &params.poll {
        // Options serialize like Mastodon's: bare Notes whose `replies`
        // collection carries the tally.
        let items: Vec<Value> = poll
            .options
            .iter()
            .enumerate()
            .map(|(index, name)| {
                json!({
                    "type": "Note",
                    "name": name,
                    "replies": {
                        "type": "Collection",
                        "totalItems": poll.tallies.get(index).copied().unwrap_or(0),
                    },
                })
            })
            .collect();
        note[if poll.multiple { "anyOf" } else { "oneOf" }] = json!(items);
        if let Some(end_time) = poll.end_time {
            note["endTime"] = json!(end_time);
            if poll.expired {
                note["closed"] = json!(end_time);
            }
        }
        if let Some(voters) = poll.voters_count {
            note["votersCount"] = json!(voters);
        }
    }
    if let Some(event) = &params.event {
        write_event_fields(&mut note, event);
    }
    // The quote-approval policy, federated like Mastodon's `NoteSerializer`:
    // always present, only automatic approval, self-fallback when nobody else
    // is granted.
    note["interactionPolicy"] = crate::quote_policy::interaction_policy_json(
        params.quote_approval_policy,
        &urls.id,
        &urls.followers,
        &urls.following,
    );
    advertise_collections(&mut note, params, &urls.id, &status_urls);
    note
}

/// Writes the `Event` properties onto an outgoing object.
///
/// Field names follow Mobilizon, the only widely-deployed event host: it is both
/// the reference consumer and the one implementation whose parser we have read.
/// `status` is duplicated as `ical:status` because Mobilizon emits both and reads
/// either, and a consumer that only knows the ical term would otherwise miss a
/// cancellation — the single most important thing an event can say.
fn write_event_fields(note: &mut Value, event: &NoteEvent<'_>) {
    note["startTime"] = json!(event.start_time);
    if let Some(end) = event.end_time {
        note["endTime"] = json!(end);
    }
    if let Some(zone) = event.timezone {
        note["timezone"] = json!(zone);
    }
    note["joinMode"] = json!(event.join_mode);
    note["status"] = json!(event.status);
    note["ical:status"] = json!(event.status);
    note["isOnline"] = json!(event.is_online);
    note["participantCount"] = json!(event.participant_count);
    // Never emitted as `true`: an unpublished event is not federated at all, so
    // the only honest value on the wire is `false`. Stated rather than omitted
    // because Mobilizon's own parser reads it and a missing field there is
    // ambiguous.
    note["draft"] = json!(false);
    if let Some(max) = event.max_attendees {
        note["maximumAttendeeCapacity"] = json!(max);
        // The remaining count is derived, never stored: keeping both would let
        // them drift, and the origin is the only party that can compute it.
        let remaining = i64::from(max) - event.participant_count;
        note["remainingAttendeeCapacity"] = json!(remaining.max(0));
    }
    if let Some(url) = event.external_participation_url {
        note["externalParticipationUrl"] = json!(url);
    }
    // The `Place`, with its `PostalAddress` nested — but only when there is
    // something to put in it. An empty Place is worse than none: a consumer
    // renders it as a venue whose name it failed to read.
    let address: Vec<(&str, Option<&str>)> = vec![
        ("streetAddress", event.location_street),
        ("addressLocality", event.location_locality),
        ("addressRegion", event.location_region),
        ("addressCountry", event.location_country),
        ("postalCode", event.location_postal_code),
    ];
    let has_address = address.iter().any(|(_, value)| value.is_some());
    if event.location_name.is_some() || has_address {
        let mut place = json!({"type": "Place"});
        if let Some(name) = event.location_name {
            place["name"] = json!(name);
        }
        if has_address {
            let mut postal = json!({"type": "PostalAddress"});
            for (key, value) in address {
                if let Some(value) = value {
                    postal[key] = json!(value);
                }
            }
            place["address"] = postal;
        }
        note["location"] = place;
    }
}

/// The collections a Note advertises about itself, exactly like Mastodon's
/// `NoteSerializer`: `replies` inlines a first page of self-reply IRIs whose
/// `next` link pages by `min_id` (or jumps straight to other accounts'
/// replies), `likes`/`shares` carry only their size.
fn advertise_collections(
    note: &mut Value,
    params: &NoteParams<'_>,
    actor_id: &str,
    status_urls: &LocalStatusUrls,
) {
    let reply_items: Vec<String> = params
        .self_reply_ids
        .iter()
        .map(|id| format!("{actor_id}/statuses/{id}"))
        .collect();
    let next = replies_page_url(
        &status_urls.replies,
        params.self_reply_ids.last().copied(),
        params.self_reply_ids.is_empty().then_some(true),
    );
    note["replies"] = json!({
        "id": status_urls.replies,
        "type": "Collection",
        "first": {
            "type": "CollectionPage",
            "next": next,
            "partOf": status_urls.replies,
            "items": reply_items,
        },
    });
    note["likes"] = json!({
        "id": status_urls.likes,
        "type": "Collection",
        "totalItems": params.favourites_count,
    });
    note["shares"] = json!({
        "id": status_urls.shares,
        "type": "Collection",
        "totalItems": params.reblogs_count,
    });
}

/// A federated poll vote: a bare `Note` whose `name` is the chosen option,
/// wrapped in a `Create` addressed to the poll's author only — Mastodon's
/// exact shape.
#[must_use]
pub fn create_vote(
    domain: &str,
    username: &str,
    vote_id: i64,
    option_name: &str,
    poll_status_uri: &str,
    poll_author_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    // The vote Note id is a fragment-free path (`.../votes/{id}`), not the
    // Mastodon-style `{actor}#votes/{id}` fragment: Misskey-family servers
    // (Sharkey/Firefish/…) validate an inbound Note's id with
    // `assertUrl(id, { allowFragment: false })` and drop any Note whose id
    // carries a fragment, so a fragment id silently loses the vote there.
    // Mastodon/Pleroma/GoToSocial read the inline object and never dereference
    // this id, so a path form federates everywhere.
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}/votes/{vote_id}/activity", urls.id),
        "type": "Create",
        "actor": urls.id,
        "to": poll_author_uri,
        "object": {
            "id": format!("{}/votes/{vote_id}", urls.id),
            "type": "Note",
            "name": option_name,
            "attributedTo": urls.id,
            "inReplyTo": poll_status_uri,
            "to": poll_author_uri,
        },
    })
}

/// The `Create` wrapping a [`note_object`], ready for delivery.
#[must_use]
pub fn create_note(params: &NoteParams<'_>) -> Value {
    let mut create = create_with_note(params.domain, params.username, note_object(params));
    // The extended context covers the quote/interaction-policy terms
    // notes can carry (Mastodon also always ships its full context).
    create["@context"] = quote_context();
    create
}

/// FEP-171b: the container owner's `Add` wrapping an inner activity (a
/// `Create`/`Update`/…) into a conversation container. `target` is the partial
/// collection object the spec requires (`type`/`id`/`attributedTo`). `to` is
/// the conversation audience. Carries the extended context, since the inner
/// activity's Note may bear quote/interaction terms.
#[must_use]
pub fn container_add(
    add_id: &str,
    owner_uri: &str,
    container_uri: &str,
    inner: &Value,
    to: &[String],
) -> Value {
    json!({
        "@context": quote_context(),
        "id": add_id,
        "type": "Add",
        "actor": owner_uri,
        "object": inner,
        "target": {
            "type": "OrderedCollection",
            "id": container_uri,
            "attributedTo": owner_uri,
        },
        "to": to,
    })
}

/// The `Create` around an already-rendered note, addressing copied from the
/// note itself. Carries no `@context`: the outbox page inlining it ships the
/// context once at the top, and [`create_note`] adds its own.
#[must_use]
pub fn create_with_note(domain: &str, username: &str, note: Value) -> Value {
    // The object's persisted `attributedTo` is authoritative. New local
    // accounts use handle-independent actor IDs while legacy accounts retain
    // `/users/:handle`; rebuilding the actor from `username` here made the
    // Create claim a different actor from its Note/Page after the immutable-ID
    // migration. Besides violating ActivityPub identity binding, Lemmy rejects
    // that shape with `verify_urls_match(actor, object.attributedTo)`.
    let actor = note
        .get("attributedTo")
        .and_then(Value::as_str)
        .map_or_else(|| LocalUserUrls::new(domain, username).id, str::to_owned);
    let mut create = json!({
        "id": format!("{}/activity", note["id"].as_str().expect("note has an id")),
        "type": "Create",
        "actor": actor,
        "published": note["published"],
        "to": note["to"],
        "cc": note["cc"],
    });
    // A group submission's community claim rides the activity too (FEP-1b12
    // consumers check the wrapper, the inner activity and its object).
    if let Some(audience) = note.get("audience").filter(|a| !a.is_null()) {
        create["audience"] = audience.clone();
    }
    create["object"] = note;
    create
}

/// An outgoing `Like` of a status.
#[must_use]
pub fn like(domain: &str, username: &str, marker: i64, object_uri: &str) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#likes/{marker}", urls.id),
        "type": "Like",
        "actor": urls.id,
        "object": object_uri,
    })
}

/// An outgoing `Dislike` — a downvote on a group post. Callers
/// stamp `audience` with the group's id, the FEP-1b12 community claim vote
/// consumers gate on.
#[must_use]
pub fn dislike(domain: &str, username: &str, marker: i64, object_uri: &str) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#dislikes/{marker}", urls.id),
        "type": "Dislike",
        "actor": urls.id,
        "object": object_uri,
    })
}

/// Parameters for an outgoing RSVP (`Join` / `Leave`) on an event.
#[derive(Debug)]
pub struct RsvpParams<'a> {
    pub domain: &'a str,
    pub username: &'a str,
    /// Our participation row id, which makes the activity id stable and
    /// idempotent: the origin stores it as the participant's url and echoes it
    /// back in its `Accept`/`Reject`, and that is how we find the row again.
    pub marker: i64,
    /// The event's IRI. Sent as a bare IRI, never embedded — Mobilizon
    /// re-fetches the event from its origin regardless.
    pub event_uri: &'a str,
    /// The organizer, or the group when the event is attributed to one. Named in
    /// `to` beside ourselves so the activity reaches whoever decides.
    pub target_uri: Option<&'a str>,
    /// The attendee's optional note to the organizer.
    pub message: Option<&'a str>,
}

/// An outgoing `Join` — an RSVP to an event.
///
/// Mobilizon's inbound handler needs only `type`, `actor`, `object` (the event
/// IRI) and `id`, plus an optional `participationMessage`; it stores our `id` as
/// the participant url. Addressed to ourselves and the deciding party rather
/// than to `Public`: an RSVP is not an announcement, and widening it would leak
/// attendance to everyone who follows us.
#[must_use]
pub fn join_event(params: &RsvpParams<'_>) -> Value {
    let urls = LocalUserUrls::new(params.domain, params.username);
    let mut to = vec![Value::String(urls.id.clone())];
    if let Some(target) = params.target_uri {
        to.push(Value::String(target.to_owned()));
    }
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": join_uri(params.domain, params.username, params.marker),
        "type": "Join",
        "actor": urls.id,
        "object": params.event_uri,
        "to": to,
    });
    if let Some(message) = params.message.filter(|m| !m.trim().is_empty()) {
        activity["participationMessage"] = Value::String(message.to_owned());
    }
    activity
}

/// An outgoing `Leave` — withdrawing an RSVP.
///
/// A bare `Leave`, **not** `Undo(Join)`: Mobilizon emits `Leave` and its
/// transmogrifier has no `Undo(Join)` arm at all, so an `Undo` would be
/// silently dropped by the one peer that hosts events. (We still *accept*
/// `Undo(Join)` inbound, which costs one match arm.)
#[must_use]
pub fn leave_event(params: &RsvpParams<'_>) -> Value {
    let urls = LocalUserUrls::new(params.domain, params.username);
    let mut to = vec![Value::String(urls.id.clone())];
    if let Some(target) = params.target_uri {
        to.push(Value::String(target.to_owned()));
    }
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}/leave", join_uri(params.domain, params.username, params.marker)),
        "type": "Leave",
        "actor": urls.id,
        "object": params.event_uri,
        "to": to,
    })
}

/// The id our outgoing `Join` activities carry. The origin echoes it back as the
/// object of its `Accept`/`Reject`, so it is the handle that resolves a pending
/// RSVP — see `status_participation::settle_by_uri`.
#[must_use]
pub fn join_uri(domain: &str, local_username: &str, participation_row_id: i64) -> String {
    let urls = LocalUserUrls::new(domain, local_username);
    format!("{}#joins/{participation_row_id}", urls.id)
}

#[must_use]
pub fn join_uri_for_actor(actor_id: &str, participation_row_id: i64) -> String {
    format!("{actor_id}#joins/{participation_row_id}")
}

/// Parameters for our verdict on someone else's `Join`: `Accept(Join)` or
/// `Reject(Join)` on an event we host.
#[derive(Debug)]
pub struct JoinVerdictParams<'a> {
    pub domain: &'a str,
    /// The local actor answering. For a group-attributed event that is a
    /// moderator of the group, with the group itself in `attributedTo` — the
    /// Lemmy-shaped mod action.
    pub username: &'a str,
    pub marker: i64,
    /// The group the event belongs to, when it has one.
    pub group_uri: Option<&'a str>,
    /// The attendee, who must be addressed for the verdict to reach them.
    pub attendee_uri: &'a str,
    /// The `Join` being answered, echoed back whole — Mobilizon reads the
    /// participant from it and cannot resolve a bare IRI it never stored.
    pub join: Value,
}

/// Our `Accept(Join)` or `Reject(Join)` for an RSVP to an event we host.
#[must_use]
pub fn join_verdict(params: &JoinVerdictParams<'_>, accept: bool) -> Value {
    let urls = LocalUserUrls::new(params.domain, params.username);
    let kind = if accept { "Accept" } else { "Reject" };
    let slug = if accept { "accept" } else { "reject" };
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#{slug}/join/{}", urls.id, params.marker),
        "type": kind,
        "actor": urls.id,
        "to": [params.attendee_uri],
        "object": Value::Null,
    });
    // The group's claim on a moderator action rides the activity,
    // that is what lets the receiver check the
    // actor's affiliation instead of trusting the actor alone.
    if let Some(group) = params.group_uri {
        activity["attributedTo"] = Value::String(group.to_owned());
        activity["audience"] = Value::String(group.to_owned());
    }
    activity["object"] = params.join.clone();
    activity
}

/// Parameters for an outgoing Pleroma/litepub `EmojiReact`.
#[derive(Debug)]
pub struct EmojiReactParams<'a> {
    pub domain: &'a str,
    pub username: &'a str,
    pub marker: i64,
    pub object_uri: &'a str,
    pub object_author_uri: &'a str,
    pub public: bool,
    pub name: &'a str,
    pub custom_emoji_url: Option<&'a str>,
}

/// An outgoing Pleroma/litepub `EmojiReact` of a status. Unicode reactions
/// carry the emoji as bare `content`; custom emoji carry `:shortcode:` plus an
/// `Emoji` tag so the receiver can render the image.
#[must_use]
pub fn emoji_react(params: &EmojiReactParams<'_>) -> Value {
    let urls = LocalUserUrls::new(params.domain, params.username);
    let content = if params.custom_emoji_url.is_some() {
        format!(":{}:", params.name)
    } else {
        params.name.to_owned()
    };
    let (to, cc) = if params.public {
        (
            json!([urls.followers, params.object_author_uri]),
            json!([PUBLIC]),
        )
    } else {
        (json!([params.object_author_uri]), json!([]))
    };
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#emoji_reactions/{}", urls.id, params.marker),
        "type": "EmojiReact",
        "actor": urls.id,
        "context": params.object_uri,
        "to": to,
        "cc": cc,
        "content": content,
        "object": params.object_uri,
    });
    if let Some(url) = params.custom_emoji_url {
        activity["tag"] = json!([{
            "type": "Emoji",
            "id": url,
            "name": format!(":{}:", params.name),
            "icon": { "type": "Image", "url": url },
            "updated": "1970-01-01T00:00:00Z",
        }]);
    }
    activity
}

/// An outgoing `Announce` (boost). `boost_id` is the local boost status row,
/// making the id the boost's own `…/statuses/{id}/activity` URL.
#[must_use]
pub fn announce(
    domain: &str,
    username: &str,
    boost_id: i64,
    object_uri: &str,
    published: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}/statuses/{boost_id}/activity", urls.id),
        "type": "Announce",
        "actor": urls.id,
        "published": published,
        "to": [PUBLIC],
        "cc": [urls.followers],
        "object": object_uri,
    })
}

/// A local group's FEP-1b12 `Announce` wrapper: the accepted member activity
/// embedded **verbatim** (the FEP's MUST — never mutate the inner activity;
/// its own author proof stays valid) and fanned out to the group's followers.
/// `marker` makes the id unique per announcement; the fragment id scheme
/// keeps wrapper ids distinct from boost-row `…/statuses/{id}/activity` ids,
/// so the Mastodon-compat bare `Announce` of the same post never collides.
#[must_use]
pub fn group_announce(domain: &str, groupname: &str, marker: i64, inner: Value) -> Value {
    let urls = LocalUserUrls::new(domain, groupname);
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#announce/{marker}", urls.id),
        "type": "Announce",
        "actor": urls.id,
        "to": [PUBLIC],
        "cc": [urls.followers],
        "audience": urls.id,
        "object": null,
    });
    activity["object"] = inner;
    activity
}

/// The shared envelope of a group moderator's action (Lemmy's mod activities):
/// the acting moderator is the `actor`, the community is the `audience` and in
/// `cc`, addressed `to` Public. The group wraps this verbatim in its `Announce`
/// (like every FEP-1b12 activity), so the group's signature — not the
/// moderator's — authenticates it downstream; every consumer verifies the
/// actor against the group's moderator list.
fn group_mod_action(
    kind: &str,
    actor_id: &str,
    id: String,
    object: Value,
    group_uri: &str,
) -> Value {
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": null,
        "type": kind,
        "actor": actor_id,
        "to": [PUBLIC],
        "cc": [group_uri],
        "audience": group_uri,
        "object": null,
    });
    activity["id"] = Value::String(id);
    activity["object"] = object;
    activity
}

/// A group moderator's removal of a post/comment — Lemmy's mod-`Delete`: a
/// `Delete` carrying a `summary` (the reason) is a moderator action, distinct
/// from an author deleting their own content. `object_uri` is the removed
/// status; `group_uri` the community.
#[must_use]
pub fn group_remove(
    domain: &str,
    mod_username: &str,
    marker: i64,
    object_uri: &str,
    group_uri: &str,
    reason: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    let mut activity = group_mod_action(
        "Delete",
        &urls.id,
        format!("{}#remove/{marker}", urls.id),
        json!({ "id": object_uri, "type": "Tombstone" }),
        group_uri,
    );
    activity["summary"] = Value::String(reason.to_owned());
    activity
}

/// A group moderator's thread lock — Lemmy's `Lock` (no new comments). `Undo`
/// this with [`undo`] to reopen the thread.
#[must_use]
pub fn group_lock(
    domain: &str,
    mod_username: &str,
    marker: i64,
    object_uri: &str,
    group_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    group_mod_action(
        "Lock",
        &urls.id,
        format!("{}#lock/{marker}", urls.id),
        Value::String(object_uri.to_owned()),
        group_uri,
    )
}

/// A group moderator's ban (Lemmy's `Block` with `target` = community): the
/// banned actor is `object`, the community is `target`. `remove_data` asks
/// followers to purge the actor's content; `expires` (RFC 3339) makes it a
/// temp ban; `reason` fills `summary`. `Undo` this with [`undo`] to unban.
#[must_use]
#[allow(clippy::too_many_arguments)] // a faithful builder for Lemmy's Block shape
pub fn group_ban(
    domain: &str,
    mod_username: &str,
    marker: i64,
    target_actor_uri: &str,
    group_uri: &str,
    remove_data: bool,
    expires: Option<&str>,
    reason: Option<&str>,
) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    let mut activity = group_mod_action(
        "Block",
        &urls.id,
        format!("{}#ban/{marker}", urls.id),
        Value::String(target_actor_uri.to_owned()),
        group_uri,
    );
    activity["target"] = Value::String(group_uri.to_owned());
    activity["removeData"] = Value::Bool(remove_data);
    if let Some(expires) = expires {
        activity["expires"] = Value::String(expires.to_owned());
    }
    if let Some(reason) = reason {
        activity["summary"] = Value::String(reason.to_owned());
    }
    activity
}

/// A group moderator's pin/unpin of a community post — Lemmy's `Add`/`Remove`
/// targeting the community's `featured` collection. `add` chooses the verb.
#[must_use]
pub fn group_feature(
    domain: &str,
    mod_username: &str,
    marker: i64,
    object_uri: &str,
    group_uri: &str,
    add: bool,
) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    let kind = if add { "Add" } else { "Remove" };
    let mut activity = group_mod_action(
        kind,
        &urls.id,
        format!("{}#feature/{marker}", urls.id),
        Value::String(object_uri.to_owned()),
        group_uri,
    );
    activity["target"] = Value::String(format!("{group_uri}/collections/featured"));
    activity
}

/// A group owner's moderator grant/revoke — Lemmy's `Add`/`Remove` targeting
/// the community's `moderators` collection (FEP-1b12). `add` chooses the verb.
#[must_use]
pub fn group_moderator(
    domain: &str,
    owner_username: &str,
    marker: i64,
    target_actor_uri: &str,
    group_uri: &str,
    add: bool,
) -> Value {
    let urls = LocalUserUrls::new(domain, owner_username);
    let kind = if add { "Add" } else { "Remove" };
    let mut activity = group_mod_action(
        kind,
        &urls.id,
        format!("{}#moderator/{marker}", urls.id),
        Value::String(target_actor_uri.to_owned()),
        group_uri,
    );
    activity["target"] = Value::String(format!("{group_uri}/moderators"));
    activity
}

/// A group's profile/settings change federated as Lemmy's `UpdateCommunity`:
/// the acting moderator (the owner) is the `actor` and the refreshed Group
/// actor document is the `object`, addressed to the community (`cc`/`audience`)
/// and Public. The group wraps this verbatim in its `Announce`; Lemmy accepts
/// it because the actor shares the community's domain (`verify_mod_action`
/// treats a same-instance actor as staff), then refreshes its cached
/// community. `group_doc` should already have its own `@context` stripped so
/// the wire matches Lemmy's (the wrapper and the object each carry one
/// otherwise). Distinct from the plain `Update(Actor)` we also fan out so
/// Mastodon followers of the group actor refresh it.
#[must_use]
pub fn group_update(
    domain: &str,
    mod_username: &str,
    marker: i64,
    group_uri: &str,
    group_doc: Value,
) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    group_mod_action(
        "Update",
        &urls.id,
        format!("{}#update-community/{marker}", urls.id),
        group_doc,
        group_uri,
    )
}

/// A group's deletion federated as Lemmy's community `Delete`: the owner is the
/// `actor`, a `Tombstone` of the group is the `object`. Same envelope as the
/// other mod actions, so a subscribing Lemmy marks its cached community
/// deleted. Carries no `summary` — a community deletion, not a mod removal.
/// Distinct from the plain `Delete(Actor)` we also fan out (which drops the
/// actor on Mastodon followers).
#[must_use]
pub fn group_delete(domain: &str, mod_username: &str, marker: i64, group_uri: &str) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    group_mod_action(
        "Delete",
        &urls.id,
        format!("{}#delete-community/{marker}", urls.id),
        json!({ "id": group_uri, "type": "Tombstone" }),
        group_uri,
    )
}

/// A group moderator's `Undo` of a prior mod action (`Undo(Lock)`,
/// `Undo(Block)`): like [`undo`] but carrying the community addressing Lemmy
/// insists on (`verify_is_public` checks `to`/`cc`, plus the `audience`).
#[must_use]
pub fn group_undo(domain: &str, mod_username: &str, group_uri: &str, inner: Value) -> Value {
    let urls = LocalUserUrls::new(domain, mod_username);
    let inner_id = inner["id"].as_str().unwrap_or_default();
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": format!("{inner_id}/undo"),
        "type": "Undo",
        "actor": urls.id,
        "to": [PUBLIC],
        "cc": [group_uri],
        "audience": group_uri,
        "object": null,
    });
    activity["object"] = inner;
    activity
}

/// An `Undo` wrapping a previously-sent activity (Like, Announce, Follow).
#[must_use]
pub fn undo(domain: &str, username: &str, inner: Value) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    let inner_id = inner["id"].as_str().unwrap_or_default();
    let mut activity = json!({
        "@context": AS_CONTEXT,
        "id": format!("{inner_id}/undo"),
        "type": "Undo",
        "actor": urls.id,
        "object": null,
    });
    activity["object"] = inner;
    activity
}

/// The id our outgoing `Block` activities carry (Mastodon's synthetic
/// `{actor}#blocks/{id}` scheme), so `Undo(Block)` can reference it.
#[must_use]
pub fn block_uri(domain: &str, local_username: &str, block_row_id: i64) -> String {
    let urls = LocalUserUrls::new(domain, local_username);
    format!("{}#blocks/{block_row_id}", urls.id)
}

/// Builds an outgoing `Block` of a remote actor, the shape Mastodon's
/// `BlockSerializer` emits.
#[must_use]
pub fn block(
    domain: &str,
    local_username: &str,
    block_row_id: i64,
    target_actor_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    json!({
        "@context": AS_CONTEXT,
        "id": block_uri(domain, local_username, block_row_id),
        "type": "Block",
        "actor": urls.id,
        "object": target_actor_uri,
    })
}

/// Builds an outgoing `Flag` (a moderation report), the shape Mastodon's
/// `FlagSerializer` emits: it is signed and delivered by the instance actor
/// (so the reporting user stays anonymous to the target's server), and its
/// `object` is always an array — the reported account's URI plus any reported
/// status URIs.
#[must_use]
pub fn flag(
    instance_actor_uri: &str,
    report_uri: &str,
    comment: &str,
    object_uris: &[String],
) -> Value {
    json!({
        "@context": AS_CONTEXT,
        "id": report_uri,
        "type": "Flag",
        "actor": instance_actor_uri,
        "content": comment,
        "object": object_uris,
    })
}

/// Rebuilds a follower's `Follow` of us from its stored activity id, so a
/// `Reject` can reference the exact activity the remote side sent.
#[must_use]
pub fn follow_as_received(
    follow_activity_uri: &str,
    follower_actor_uri: &str,
    domain: &str,
    local_username: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    json!({
        "id": follow_activity_uri,
        "type": "Follow",
        "actor": follower_actor_uri,
        "object": urls.id,
    })
}

/// Builds a `Reject` of a received `Follow` — what severs a follower when
/// blocking, or answers a follow from someone blocked (Mastodon's
/// `RejectFollowService`).
#[must_use]
pub fn reject_follow(
    domain: &str,
    local_username: &str,
    marker: i64,
    follow_activity: Value,
) -> Value {
    let urls = LocalUserUrls::new(domain, local_username);
    let mut activity = json!({
        "@context": AS_CONTEXT,
        // Mirrors Mastodon's synthetic id scheme for rejects.
        "id": format!("{}#rejects/follows/{marker}", urls.id),
        "type": "Reject",
        "actor": urls.id,
        "object": null,
    });
    activity["object"] = follow_activity;
    activity
}

/// JSON-LD context extension for FEP-044f quote vocabulary, matching the
/// term definitions Mastodon publishes.
#[must_use]
pub fn quote_context() -> Value {
    json!([AS_CONTEXT, {
        "sensitive": "as:sensitive",
        "toot": "http://joinmastodon.org/ns#",
        "votersCount": "toot:votersCount",
        "Emoji": "toot:Emoji",
        "blurhash": "toot:blurhash",
        "focalPoint": { "@container": "@list", "@id": "toot:focalPoint" },
        "QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest",
        "quote": { "@id": "https://w3id.org/fep/044f#quote", "@type": "@id" },
        "quoteUri": "http://fedibird.com/ns#quoteUri",
        "_misskey_quote": "https://misskey-hub.net/ns#_misskey_quote",
        "quoteAuthorization": {
            "@id": "https://w3id.org/fep/044f#quoteAuthorization",
            "@type": "@id",
        },
        "QuoteAuthorization": "https://w3id.org/fep/044f#QuoteAuthorization",
        // FEP-171b conversation containers. `context` is standard AS vocab;
        // `contextHistory` is the container (collection of activities) link.
        // `conversation` is Mastodon's legacy alias for the same grouping.
        "contextHistory": { "@id": "https://w3id.org/fep/171b#contextHistory", "@type": "@id" },
        "ostatus": "http://ostatus.org#",
        "conversation": "ostatus:conversation",
        "gts": "https://gotosocial.org/ns#",
        "interactingObject": { "@id": "gts:interactingObject", "@type": "@id" },
        "interactionTarget": { "@id": "gts:interactionTarget", "@type": "@id" },
        "interactionPolicy": { "@id": "gts:interactionPolicy", "@type": "@id" },
        "canQuote": { "@id": "gts:canQuote", "@type": "@id" },
        "automaticApproval": { "@id": "gts:automaticApproval", "@type": "@id" },
    }])
}

/// The id of our outgoing `QuoteRequest` for a quote row.
#[must_use]
pub fn quote_request_uri(domain: &str, username: &str, quote_id: i64) -> String {
    format!(
        "{}#quote_requests/{quote_id}",
        LocalUserUrls::new(domain, username).id
    )
}

#[must_use]
pub fn quote_request_uri_for_actor(actor_id: &str, quote_id: i64) -> String {
    format!("{actor_id}#quote_requests/{quote_id}")
}

/// The URL of a `QuoteAuthorization` stamp we issue.
#[must_use]
pub fn quote_authorization_uri(domain: &str, username: &str, quote_id: i64) -> String {
    format!(
        "{}/quote_authorizations/{quote_id}",
        LocalUserUrls::new(domain, username).id
    )
}

#[must_use]
pub fn quote_authorization_uri_for_actor(actor_id: &str, quote_id: i64) -> String {
    format!("{actor_id}/quote_authorizations/{quote_id}")
}

/// An outgoing `QuoteRequest`: asks the author of `quoted_uri` for consent,
/// with our quote post inlined as the instrument.
#[must_use]
pub fn quote_request(
    domain: &str,
    username: &str,
    quote_id: i64,
    quoted_uri: &str,
    instrument: Value,
) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    let mut activity = json!({
        "@context": quote_context(),
        "id": quote_request_uri(domain, username, quote_id),
        "type": "QuoteRequest",
        "actor": urls.id,
        "object": quoted_uri,
        "instrument": null,
    });
    activity["instrument"] = instrument;
    activity
}

/// Parameters for answering an inbound `QuoteRequest`.
#[derive(Debug)]
pub struct QuoteResponseParams<'a> {
    /// The local (quoted) account.
    pub domain: &'a str,
    pub username: &'a str,
    pub actor_id: Option<&'a str>,
    pub quote_id: i64,
    /// The original `QuoteRequest`'s id, echoed back.
    pub request_activity_uri: &'a str,
    /// The quoting (remote) actor.
    pub quoting_actor_uri: &'a str,
    /// The quoting post.
    pub quote_status_uri: &'a str,
    /// The quoted (local) post.
    pub quoted_status_uri: &'a str,
}

fn quote_request_echo(params: &QuoteResponseParams<'_>) -> Value {
    json!({
        "id": params.request_activity_uri,
        "type": "QuoteRequest",
        "actor": params.quoting_actor_uri,
        "object": params.quoted_status_uri,
        "instrument": params.quote_status_uri,
    })
}

/// `Accept(QuoteRequest)` with the authorization stamp as `result`.
#[must_use]
pub fn accept_quote_request(params: &QuoteResponseParams<'_>) -> Value {
    let urls = LocalUserUrls::for_account(params.domain, params.username, params.actor_id);
    let result = quote_authorization_uri_for_actor(&urls.id, params.quote_id);
    json!({
        "@context": quote_context(),
        "id": format!("{}#accepts/quote_requests/{}", urls.id, params.quote_id),
        "type": "Accept",
        "actor": urls.id,
        "object": quote_request_echo(params),
        "result": result,
    })
}

/// `Reject(QuoteRequest)`.
#[must_use]
pub fn reject_quote_request(params: &QuoteResponseParams<'_>) -> Value {
    let urls = LocalUserUrls::for_account(params.domain, params.username, params.actor_id);
    json!({
        "@context": quote_context(),
        "id": format!("{}#rejects/quote_requests/{}", urls.id, params.quote_id),
        "type": "Reject",
        "actor": urls.id,
        "object": quote_request_echo(params),
    })
}

/// The `QuoteAuthorization` stamp served at
/// [`quote_authorization_uri`]; other servers fetch it to verify a quote.
#[must_use]
pub fn quote_authorization(
    domain: &str,
    username: &str,
    quote_id: i64,
    quoting_status_uri: &str,
    quoted_status_uri: &str,
) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": quote_context(),
        "id": quote_authorization_uri(domain, username, quote_id),
        "type": "QuoteAuthorization",
        "attributedTo": urls.id,
        "interactingObject": quoting_status_uri,
        "interactionTarget": quoted_status_uri,
    })
}

#[must_use]
pub fn quote_authorization_for_actor(
    actor_id: &str,
    quote_id: i64,
    quoting_status_uri: &str,
    quoted_status_uri: &str,
) -> Value {
    json!({
        "@context": quote_context(),
        "id": quote_authorization_uri_for_actor(actor_id, quote_id),
        "type": "QuoteAuthorization",
        "attributedTo": actor_id,
        "interactingObject": quoting_status_uri,
        "interactionTarget": quoted_status_uri,
    })
}

/// A `Delete` of a `QuoteAuthorization` stamp: the quoted author revokes a
/// previously-granted quote. The quoter drops the embed on receipt. Mirrors
/// Mastodon's `DeleteQuoteAuthorizationSerializer` (id is the stamp uri with a
/// `#delete` fragment, the stamp is inlined as the object).
#[must_use]
pub fn delete_quote_authorization(
    domain: &str,
    username: &str,
    quote_id: i64,
    quoting_status_uri: &str,
    quoted_status_uri: &str,
) -> Value {
    let stamp = quote_authorization(
        domain,
        username,
        quote_id,
        quoting_status_uri,
        quoted_status_uri,
    );
    let approval_uri = quote_authorization_uri(domain, username, quote_id);
    json!({
        "@context": quote_context(),
        "id": format!("{approval_uri}#delete"),
        "type": "Delete",
        "actor": LocalUserUrls::new(domain, username).id,
        "to": [PUBLIC],
        "object": stamp,
    })
}

#[must_use]
pub fn delete_quote_authorization_for_actor(
    actor_id: &str,
    quote_id: i64,
    quoting_status_uri: &str,
    quoted_status_uri: &str,
) -> Value {
    let stamp =
        quote_authorization_for_actor(actor_id, quote_id, quoting_status_uri, quoted_status_uri);
    let approval_uri = quote_authorization_uri_for_actor(actor_id, quote_id);
    json!({
        "@context": quote_context(),
        "id": format!("{approval_uri}#delete"),
        "type": "Delete",
        "actor": actor_id,
        "to": [PUBLIC],
        "object": stamp,
    })
}

/// An `Update` wrapping a Note (used to re-distribute a quote post once its
/// authorization arrives).
#[must_use]
pub fn update_note(domain: &str, username: &str, note: Value, updated: &str) -> Value {
    // As with `Create`, the persisted Note identity wins over the mutable
    // handle. This matters when the Update is embedded verbatim in a group's
    // Announce: only the outer group activity is rebased at delivery time.
    let actor = note
        .get("attributedTo")
        .and_then(Value::as_str)
        .map_or_else(|| LocalUserUrls::new(domain, username).id, str::to_owned);
    let note_id = note["id"].as_str().unwrap_or_default();
    let mut activity = json!({
        "@context": quote_context(),
        "id": format!("{note_id}#updates/{updated}"),
        "type": "Update",
        "actor": actor,
        "published": updated,
        "to": note["to"],
        "cc": note["cc"],
        "object": null,
    });
    activity["object"] = note;
    activity
}

/// An `Update` of a local actor (profile change), fanned out to followers.
/// `epoch` makes the id unique per change, like Mastodon's
/// `#updates/{timestamp}` scheme.
#[must_use]
pub fn update_actor(domain: &str, username: &str, actor_doc: Value, epoch: i64) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    update_actor_for_actor(&urls.id, actor_doc, epoch)
}

/// `Update(Actor)` when the canonical actor identifier is already known.
#[must_use]
pub fn update_actor_for_actor(actor_id: &str, actor_doc: Value, marker: i64) -> Value {
    let context = actor_doc
        .get("@context")
        .cloned()
        .unwrap_or_else(|| json!(AS_CONTEXT));
    let mut activity = json!({
        "@context": context,
        "id": format!("{actor_id}#updates/{marker}"),
        "type": "Update",
        "actor": actor_id,
        "to": [PUBLIC],
        "object": null,
    });
    activity["object"] = actor_doc;
    activity
}

/// A `Delete` of a local status, with a `Tombstone` object like Mastodon's.
#[must_use]
pub fn delete_note(domain: &str, username: &str, status_uri: &str) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{status_uri}#delete"),
        "type": "Delete",
        "actor": urls.id,
        "to": [PUBLIC],
        "object": { "id": status_uri, "type": "Tombstone" },
    })
}

/// A `Delete` of a local actor, with a `Tombstone` object.
#[must_use]
pub fn delete_actor(domain: &str, username: &str) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}#delete", urls.id),
        "type": "Delete",
        "actor": urls.id,
        "to": [PUBLIC],
        "object": { "id": urls.id, "type": "Tombstone" },
    })
}

/// An `Add` of a status to the actor's featured collection (pinning), the
/// shape Mastodon's `AddNoteSerializer` emits — no `id`, the collection as
/// `target`.
#[must_use]
pub fn add_to_featured(domain: &str, username: &str, status_uri: &str) -> Value {
    featured_change("Add", domain, username, status_uri)
}

/// A `Remove` of a status from the featured collection (unpinning).
#[must_use]
pub fn remove_from_featured(domain: &str, username: &str, status_uri: &str) -> Value {
    featured_change("Remove", domain, username, status_uri)
}

/// An `Add` of a featured hashtag, targeting the actor's featured collection;
/// the object is the `Hashtag` tag object Mastodon dispatches on.
#[must_use]
pub fn add_hashtag_to_featured(domain: &str, username: &str, name: &str) -> Value {
    featured_change_object("Add", domain, username, &hashtag_object(domain, name))
}

/// A `Remove` of a featured hashtag.
#[must_use]
pub fn remove_hashtag_from_featured(domain: &str, username: &str, name: &str) -> Value {
    featured_change_object("Remove", domain, username, &hashtag_object(domain, name))
}

/// The `Hashtag` tag object as Mastodon publishes it in `Note.tag` and
/// featured-tag `Add`/`Remove` activities.
#[must_use]
pub fn hashtag_object(domain: &str, name: &str) -> Value {
    let lower = name.to_lowercase();
    json!({
        "type": "Hashtag",
        "href": format!("https://{domain}/tags/{lower}"),
        "name": format!("#{lower}"),
    })
}

fn featured_change(kind: &str, domain: &str, username: &str, status_uri: &str) -> Value {
    featured_change_object(
        kind,
        domain,
        username,
        &Value::String(status_uri.to_owned()),
    )
}

fn featured_change_object(kind: &str, domain: &str, username: &str, object: &Value) -> Value {
    let urls = LocalUserUrls::new(domain, username);
    json!({
        "@context": AS_CONTEXT,
        "type": kind,
        "actor": urls.id,
        "object": object,
        "target": urls.featured,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_uses_the_notes_persisted_actor_identity() {
        let actor_id = "https://plamenu.test/users/10000000000000042";
        let note = json!({
            "id": format!("{actor_id}/statuses/7"),
            "type": "Page",
            "attributedTo": actor_id,
            "published": "2026-08-12T00:00:00Z",
            "to": [PUBLIC],
            "cc": [],
        });

        let create = create_with_note("plamenu.test", "renamed_handle", note);

        assert_eq!(create["actor"], actor_id);
        assert_eq!(create["object"]["attributedTo"], actor_id);
        assert_eq!(create["id"], format!("{actor_id}/statuses/7/activity"));
    }

    #[test]
    fn update_uses_the_notes_persisted_actor_identity() {
        let actor_id = "https://plamenu.test/users/10000000000000042";
        let note = json!({
            "id": format!("{actor_id}/statuses/7"),
            "type": "Page",
            "attributedTo": actor_id,
            "to": [PUBLIC],
            "cc": [],
        });

        let update = update_note(
            "plamenu.test",
            "renamed_handle",
            note,
            "2026-08-12T00:01:00Z",
        );

        assert_eq!(update["actor"], actor_id);
        assert_eq!(update["object"]["attributedTo"], actor_id);
    }

    #[test]
    fn event_organizer_wins_over_a_group_attribution() {
        // The Mobilizon group-event shape: the group in `attributedTo`, the
        // organizing Person in the object's own `actor`. Attributing this to
        // the group loses the organizer — and the group already appears as the
        // announcer, so the card would credit it twice and name no human.
        let event = json!({
            "type": "Event",
            "id": "https://mz.example/events/1",
            "actor": "https://mz.example/@grace",
            "attributedTo": "https://mz.example/@thegroup",
        });
        assert_eq!(
            attributed_to_id(&event),
            Some("https://mz.example/@grace"),
            "the organizer is the author"
        );
        // The ownership check must accept either, since the organizer is the
        // actor that signs and delivers the Create.
        let owners: Vec<&str> = attributed_to_ids(&event).collect();
        assert!(owners.contains(&"https://mz.example/@thegroup"));
        assert!(owners.contains(&"https://mz.example/@grace"));

        // An explicit Person in `attributedTo` is a direct authorship claim and
        // still outranks the organizer heuristic.
        let claimed = json!({
            "type": "Event",
            "actor": "https://mz.example/@grace",
            "attributedTo": [
                {"type": "Group", "id": "https://mz.example/@thegroup"},
                {"type": "Person", "id": "https://mz.example/@heidi"},
            ],
        });
        assert_eq!(
            attributed_to_id(&claimed),
            Some("https://mz.example/@heidi")
        );

        // A group-less (organizer-owned) event still resolves.
        let solo = json!({
            "type": "Event",
            "actor": "https://mz.example/@grace",
        });
        assert_eq!(attributed_to_id(&solo), Some("https://mz.example/@grace"));
    }

    #[test]
    fn an_actor_on_a_non_event_object_is_not_an_authorship_claim() {
        // Only Mobilizon's Event puts the author in `actor`. On anything else an
        // `actor` inside an object means something different (or nothing), so
        // reading it as authorship would let a forged `actor` re-attribute a
        // Note away from the account that actually wrote it.
        let note = json!({
            "type": "Note",
            "actor": "https://evil.example/users/impostor",
            "attributedTo": "https://good.example/users/real",
        });
        assert_eq!(
            attributed_to_id(&note),
            Some("https://good.example/users/real")
        );
        let owners: Vec<&str> = attributed_to_ids(&note).collect();
        assert_eq!(owners, vec!["https://good.example/users/real"]);
    }

    #[test]
    fn clamp_visibility_never_widens_past_the_root() {
        // A reply may narrow, never broaden.
        assert_eq!(clamp_visibility("public", "private"), "private");
        assert_eq!(clamp_visibility("unlisted", "private"), "private");
        assert_eq!(clamp_visibility("public", "unlisted"), "unlisted");
        // A direct root forces every reply direct.
        assert_eq!(clamp_visibility("public", "direct"), "direct");
        assert_eq!(clamp_visibility("private", "direct"), "direct");
        // Narrowing is always honoured.
        assert_eq!(clamp_visibility("direct", "public"), "direct");
        assert_eq!(clamp_visibility("private", "public"), "private");
        // A public root clamps nothing.
        assert_eq!(clamp_visibility("public", "public"), "public");
        assert_eq!(clamp_visibility("unlisted", "public"), "unlisted");
        // local ranks with private (both closed); ties keep the request.
        assert_eq!(clamp_visibility("public", "local"), "local");
        assert_eq!(clamp_visibility("local", "private"), "local");
        assert_eq!(clamp_visibility("private", "local"), "private");
        // An unrecognised requested value is treated as broad → clamped down.
        assert_eq!(clamp_visibility("bogus", "private"), "private");
    }

    #[test]
    fn parses_string_and_object_actors() {
        let activity: Activity = serde_json::from_value(json!({
            "id": "https://remote.example/activities/1",
            "type": "Follow",
            "actor": "https://remote.example/users/bob",
            "object": "https://plamenu.test/users/alice",
        }))
        .unwrap();
        assert_eq!(activity.kind, "Follow");
        assert_eq!(
            activity.actor_id(),
            Some("https://remote.example/users/bob")
        );
        assert_eq!(
            activity.object_id(),
            Some("https://plamenu.test/users/alice")
        );

        let embedded: Activity = serde_json::from_value(json!({
            "type": "Undo",
            "actor": { "id": "https://remote.example/users/bob", "type": "Person" },
            "object": { "id": "https://remote.example/activities/1", "type": "Follow" },
        }))
        .unwrap();
        assert_eq!(embedded.id, None);
        assert_eq!(
            embedded.actor_id(),
            Some("https://remote.example/users/bob")
        );
        assert_eq!(
            embedded.object_id(),
            Some("https://remote.example/activities/1")
        );
    }

    #[test]
    fn one_or_many_accepts_singleton_array_and_absent() {
        let object = json!({
            "tag": { "type": "Mention", "href": "https://plamenu.test/users/alice" },
            "attachment": [{ "type": "Document" }, { "type": "Document" }],
            "closed": null,
        });
        let singleton = one_or_many(object.get("tag"));
        assert_eq!(singleton.len(), 1);
        assert_eq!(
            singleton[0].get("type").and_then(Value::as_str),
            Some("Mention")
        );
        assert_eq!(one_or_many(object.get("attachment")).len(), 2);
        assert!(one_or_many(object.get("missing")).is_empty());
        assert!(one_or_many(object.get("closed")).is_empty());
    }

    #[test]
    fn missing_object_is_null_not_an_error() {
        let activity: Activity = serde_json::from_value(json!({
            "type": "Ping",
            "actor": "https://remote.example/users/bob",
        }))
        .unwrap();
        assert_eq!(activity.object_id(), None);
    }

    #[test]
    fn featured_add_and_remove_match_mastodons_shapes() {
        let status = "https://plamenu.test/users/alice/statuses/42";
        let value = add_to_featured("plamenu.test", "alice", status);
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "type": "Add",
                "actor": "https://plamenu.test/users/alice",
                "object": status,
                "target": "https://plamenu.test/users/alice/collections/featured",
            }),
            "Mastodon sends featured Adds without an id"
        );
        let value = remove_from_featured("plamenu.test", "alice", status);
        assert_eq!(value["type"], "Remove");
        assert_eq!(value["object"], status);
        assert_eq!(
            value["target"],
            "https://plamenu.test/users/alice/collections/featured"
        );
    }

    #[test]
    fn move_matches_mastodons_shape() {
        let value = move_account(
            "plamenu.test",
            "alice",
            7,
            "https://new.example/users/alice",
        );
        assert_eq!(
            value,
            json!({
                "@context": AS_CONTEXT,
                "id": "https://plamenu.test/users/alice#moves/7",
                "type": "Move",
                "actor": "https://plamenu.test/users/alice",
                "object": "https://plamenu.test/users/alice",
                "target": "https://new.example/users/alice",
            })
        );
    }

    #[test]
    fn outgoing_block_and_reject_match_mastodons_shapes() {
        let value = block(
            "plamenu.test",
            "alice",
            9,
            "https://remote.example/users/bob",
        );
        assert_eq!(value["type"], "Block");
        assert_eq!(value["id"], "https://plamenu.test/users/alice#blocks/9");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["object"], "https://remote.example/users/bob");

        let follow = follow_as_received(
            "https://remote.example/activities/3",
            "https://remote.example/users/bob",
            "plamenu.test",
            "alice",
        );
        let value = reject_follow("plamenu.test", "alice", 4, follow);
        assert_eq!(value["type"], "Reject");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice#rejects/follows/4"
        );
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["object"]["type"], "Follow");
        assert_eq!(value["object"]["id"], "https://remote.example/activities/3");
        assert_eq!(value["object"]["actor"], "https://remote.example/users/bob");
        assert_eq!(
            value["object"]["object"],
            "https://plamenu.test/users/alice"
        );
    }

    #[test]
    fn outgoing_flag_matches_mastodons_shape() {
        let value = flag(
            "https://plamenu.test/actor",
            "https://plamenu.test/reports/42",
            "please review",
            &[
                "https://remote.example/users/bob".to_owned(),
                "https://remote.example/users/bob/statuses/1".to_owned(),
            ],
        );
        assert_eq!(value["@context"], AS_CONTEXT);
        assert_eq!(value["type"], "Flag");
        assert_eq!(value["id"], "https://plamenu.test/reports/42");
        // Signed by the instance actor, not the reporting user.
        assert_eq!(value["actor"], "https://plamenu.test/actor");
        assert_eq!(value["content"], "please review");
        // `object` is always an array, even for a single target.
        assert_eq!(
            value["object"],
            json!([
                "https://remote.example/users/bob",
                "https://remote.example/users/bob/statuses/1",
            ])
        );
    }

    #[test]
    fn outgoing_follow_has_dereferenceable_id_and_target() {
        let value = follow(
            "plamenu.test",
            "alice",
            7,
            "https://remote.example/users/bob",
        );
        assert_eq!(value["type"], "Follow");
        assert_eq!(value["id"], "https://plamenu.test/users/alice#follows/7");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["object"], "https://remote.example/users/bob");
        assert_eq!(
            value["id"].as_str().unwrap(),
            follow_uri("plamenu.test", "alice", 7)
        );
    }

    fn note(visibility: &'static str, reply: Option<&'static str>) -> Value {
        create_note(&NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>hi</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility,
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: reply,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        })
    }

    #[test]
    fn create_note_is_public_and_addressed_to_followers() {
        let value = note("public", None);
        assert_eq!(value["type"], "Create");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice/statuses/42/activity"
        );
        assert_eq!(value["to"], json!([PUBLIC]));
        assert_eq!(
            value["cc"],
            json!(["https://plamenu.test/users/alice/followers"])
        );

        let object = &value["object"];
        assert_eq!(object["id"], "https://plamenu.test/users/alice/statuses/42");
        // The Note's `url` is the human web page, not the AP id.
        assert_eq!(object["url"], "https://plamenu.test/@alice/42");
        assert_eq!(object["type"], "Note");
        assert_eq!(object["attributedTo"], "https://plamenu.test/users/alice");
        assert_eq!(object["content"], "<p>hi</p>");
        assert_eq!(object["published"], "2026-06-10T00:00:00Z");
        assert_eq!(object["to"], json!([PUBLIC]));
        assert!(object.get("inReplyTo").is_none());
    }

    #[test]
    fn note_carries_source_content_and_media_type() {
        // Pleroma's rich-text `source` shape: raw text + mediaType,
        // exactly as Akkoma federates it.
        let mut params = NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p><strong>hi</strong></p>",
            source: Some(NoteSource {
                content: "**hi**",
                media_type: "text/markdown",
            }),
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "public",
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        };
        let object = note_object(&params);
        assert_eq!(
            object["source"],
            json!({ "content": "**hi**", "mediaType": "text/markdown" })
        );
        params.source = None;
        assert!(note_object(&params).get("source").is_none());
    }

    #[test]
    fn note_carries_summary_sensitive_and_content_map() {
        let object = &note("public", None)["object"];
        assert_eq!(object["summary"], json!(null), "no CW serializes as null");
        assert_eq!(object["sensitive"], json!(false));
        assert!(object.get("contentMap").is_none());
        assert!(object.get("updated").is_none(), "never edited: no updated");

        let value = create_note(&NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>hi</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: Some("2026-06-11T00:00:00Z"),
            visibility: "public",
            summary: Some("spider photos"),
            sensitive: true,
            language: Some("en"),
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        });
        let object = &value["object"];
        assert_eq!(object["summary"], "spider photos");
        assert_eq!(object["sensitive"], json!(true));
        assert_eq!(object["contentMap"], json!({"en": "<p>hi</p>"}));
        assert_eq!(object["updated"], "2026-06-11T00:00:00Z");
    }

    #[test]
    fn quote_bakes_re_fallback_into_content() {
        let quoted = "https://remote.test/users/bob/statuses/7";
        let object = note_object(&NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>look</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "public",
            summary: None,
            sensitive: false,
            language: Some("en"),
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: Some(NoteQuote {
                quoted_uri: quoted,
                authorization_uri: None,
            }),
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        });
        // Structural quote fields are still present...
        assert_eq!(object["quote"], quoted);
        assert_eq!(object["quoteUri"], quoted);
        // ...the FEP-e232 Link tag is dual-emitted (Mitra's shape)...
        let tags = object["tag"].as_array().expect("tag array");
        let link = tags
            .iter()
            .find(|tag| tag["type"] == "Link")
            .expect("an e232 quote Link tag");
        assert_eq!(link["href"], quoted);
        assert_eq!(
            link["mediaType"],
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        );
        assert_eq!(
            link["rel"],
            json!(["https://misskey-hub.net/ns#_misskey_quote"])
        );
        // ...and the `RE:` fallback is prepended to content and contentMap,
        // carrying the `quote-inline` marker quote-aware servers strip.
        let expected = format!(
            r#"<p class="quote-inline">RE: {}</p><p>look</p>"#,
            crate::text::shortened_link_anchor(quoted)
        );
        assert_eq!(object["content"], expected);
        assert_eq!(object["contentMap"], json!({ "en": expected }));

        // When the author already linked the post, no duplicate fallback.
        let already = note_object(&NoteParams {
            content_html: "<p>see https://remote.test/users/bob/statuses/7</p>",
            source: None,
            quote: Some(NoteQuote {
                quoted_uri: quoted,
                authorization_uri: None,
            }),
            ..base_params()
        });
        assert_eq!(
            already["content"],
            "<p>see https://remote.test/users/bob/statuses/7</p>"
        );
    }

    fn base_params() -> NoteParams<'static> {
        NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>hi</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "public",
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        }
    }

    #[test]
    fn visibility_addressing_roundtrips() {
        const FOLLOWERS: &str = "https://plamenu.test/users/alice/followers";
        for (visibility, to, cc) in [
            ("public", json!([PUBLIC]), json!([FOLLOWERS])),
            ("unlisted", json!([FOLLOWERS]), json!([PUBLIC])),
            ("private", json!([FOLLOWERS]), json!([])),
            ("direct", json!([]), json!([])),
        ] {
            let object = &note(visibility, None)["object"];
            assert_eq!(object["to"], to, "{visibility}");
            assert_eq!(object["cc"], cc, "{visibility}");
            assert_eq!(
                visibility_from_addressing(&object["to"], &object["cc"], FOLLOWERS),
                visibility
            );
        }
        // Bare-string addressing parses too.
        assert_eq!(
            visibility_from_addressing(&json!(PUBLIC), &json!(null), FOLLOWERS),
            "public"
        );
        // Mentions-only (or empty) addressing is a direct message — never
        // followers-only, which would leak it to the author's followers.
        assert_eq!(
            visibility_from_addressing(&json!(null), &json!(null), FOLLOWERS),
            "direct"
        );
        assert_eq!(
            visibility_from_addressing(
                &json!(["https://plamenu.test/users/bob"]),
                &json!(null),
                FOLLOWERS
            ),
            "direct"
        );
        // The author's followers collection in either field is private.
        assert_eq!(
            visibility_from_addressing(
                &json!(["https://plamenu.test/users/bob"]),
                &json!([FOLLOWERS]),
                FOLLOWERS
            ),
            "private"
        );
        // Someone *else's* followers collection is not ours: stays direct.
        assert_eq!(
            visibility_from_addressing(
                &json!(["https://other.example/users/eve/followers"]),
                &json!(null),
                FOLLOWERS
            ),
            "direct"
        );
    }

    #[test]
    fn direct_notes_address_mentions_in_to() {
        let value = create_note(&NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>psst</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "direct",
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &["https://remote.example/users/bob".to_owned()],
            quote: None,
            poll: None,
            quote_approval_policy: 0,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        });
        let object = &value["object"];
        assert_eq!(object["to"], json!(["https://remote.example/users/bob"]));
        assert_eq!(object["cc"], json!([]));
        assert_eq!(value["to"], object["to"]);
        // A non-quotable post still carries `interactionPolicy`, but its only
        // approver is the author (Mastodon's self-fallback) — nobody else.
        assert_eq!(
            object["interactionPolicy"]["canQuote"]["automaticApproval"],
            json!(["https://plamenu.test/users/alice"]),
            "direct posts grant quoting to nobody but the author"
        );
    }

    #[test]
    fn question_notes_carry_poll_fields() {
        let options = vec!["yes".to_owned(), "no".to_owned()];
        let tallies = vec![3, 4];
        let mut params = NoteParams {
            kind: PostKind::Question,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>pick</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "public",
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[],
            favourites_count: 0,
            reblogs_count: 0,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
            poll: Some(NotePoll {
                options: &options,
                tallies: &tallies,
                multiple: false,
                end_time: Some("2026-06-12T00:00:00Z"),
                expired: false,
                voters_count: Some(7),
            }),
        };
        let object = note_object(&params);
        assert_eq!(object["type"], "Question");
        assert_eq!(
            object["oneOf"],
            json!([
                {"type": "Note", "name": "yes",
                 "replies": {"type": "Collection", "totalItems": 3}},
                {"type": "Note", "name": "no",
                 "replies": {"type": "Collection", "totalItems": 4}},
            ])
        );
        assert!(object.get("anyOf").is_none());
        assert_eq!(object["endTime"], "2026-06-12T00:00:00Z");
        assert!(object.get("closed").is_none(), "running poll: no closed");
        assert_eq!(object["votersCount"], 7);

        params.poll = Some(NotePoll {
            options: &options,
            tallies: &tallies,
            multiple: true,
            end_time: Some("2026-06-12T00:00:00Z"),
            expired: true,
            voters_count: None,
        });
        let object = note_object(&params);
        assert!(object.get("oneOf").is_none());
        assert_eq!(object["anyOf"][1]["name"], "no");
        assert_eq!(object["closed"], "2026-06-12T00:00:00Z");
        assert!(object.get("votersCount").is_none());
    }

    #[test]
    fn vote_is_a_named_note_addressed_to_the_poll_author() {
        let value = create_vote(
            "plamenu.test",
            "alice",
            9,
            "yes",
            "https://remote.example/users/bob/statuses/1",
            "https://remote.example/users/bob",
        );
        assert_eq!(value["type"], "Create");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice/votes/9/activity"
        );
        assert_eq!(value["to"], "https://remote.example/users/bob");
        let object = &value["object"];
        // Fragment-free id (not `#votes/9`): Misskey rejects fragment note ids.
        assert_eq!(object["id"], "https://plamenu.test/users/alice/votes/9");
        assert!(
            !object["id"].as_str().unwrap().contains('#'),
            "vote note id must not carry a fragment (Misskey drops it)"
        );
        assert_eq!(object["type"], "Note");
        assert_eq!(object["name"], "yes");
        assert_eq!(object["attributedTo"], "https://plamenu.test/users/alice");
        assert_eq!(
            object["inReplyTo"],
            "https://remote.example/users/bob/statuses/1"
        );
        assert!(object.get("content").is_none(), "votes carry no content");
    }

    #[test]
    fn note_advertises_replies_likes_and_shares_collections() {
        const STATUS: &str = "https://plamenu.test/users/alice/statuses/42";
        // No self-replies: empty first page, `next` jumps straight to the
        // other-accounts page (Mastodon's shape).
        let object = &note("public", None)["object"];
        assert_eq!(
            object["replies"],
            json!({
                "id": format!("{STATUS}/replies"),
                "type": "Collection",
                "first": {
                    "type": "CollectionPage",
                    "next": format!("{STATUS}/replies?only_other_accounts=true&page=true"),
                    "partOf": format!("{STATUS}/replies"),
                    "items": [],
                },
            })
        );
        assert_eq!(
            object["likes"],
            json!({"id": format!("{STATUS}/likes"), "type": "Collection", "totalItems": 0})
        );
        assert_eq!(
            object["shares"],
            json!({"id": format!("{STATUS}/shares"), "type": "Collection", "totalItems": 0})
        );

        // With self-replies the first page inlines their IRIs and `next`
        // pages by min_id; counts flow into likes/shares.
        let value = create_note(&NoteParams {
            kind: PostKind::Note,
            domain: "plamenu.test",
            username: "alice",
            actor_id: None,
            status_id: 42,
            content_html: "<p>hi</p>",
            source: None,
            published: "2026-06-10T00:00:00Z",
            updated: None,
            visibility: "public",
            summary: None,
            sensitive: false,
            language: None,
            in_reply_to_uri: None,
            attachments: &[],
            tag: &[],
            mentioned_uris: &[],
            quote: None,
            poll: None,
            quote_approval_policy: crate::quote_policy::AUTOMATIC_PUBLIC,
            self_reply_ids: &[77, 78],
            favourites_count: 3,
            reblogs_count: 1,
            title: None,
            external_url: None,
            group_uri: None,
            context: None,
            context_history: None,
            event: None,
        });
        let object = &value["object"];
        assert_eq!(
            object["replies"]["first"]["items"],
            json!([
                "https://plamenu.test/users/alice/statuses/77",
                "https://plamenu.test/users/alice/statuses/78",
            ])
        );
        assert_eq!(
            object["replies"]["first"]["next"],
            format!("{STATUS}/replies?min_id=78&page=true")
        );
        assert_eq!(object["likes"]["totalItems"], 3);
        assert_eq!(object["shares"]["totalItems"], 1);
    }

    #[test]
    fn replies_carry_in_reply_to() {
        let object = &note(
            "public",
            Some("https://remote.example/users/bob/statuses/9"),
        )["object"];
        assert_eq!(
            object["inReplyTo"],
            "https://remote.example/users/bob/statuses/9"
        );
    }

    #[test]
    fn like_announce_undo_delete_builders() {
        let like_value = like("plamenu.test", "alice", 5, "https://remote.example/s/1");
        assert_eq!(like_value["type"], "Like");
        assert_eq!(like_value["id"], "https://plamenu.test/users/alice#likes/5");
        assert_eq!(like_value["object"], "https://remote.example/s/1");

        let dislike_value = dislike("plamenu.test", "alice", 6, "https://remote.example/s/1");
        assert_eq!(dislike_value["type"], "Dislike");
        assert_eq!(
            dislike_value["id"],
            "https://plamenu.test/users/alice#dislikes/6"
        );
        assert_eq!(dislike_value["object"], "https://remote.example/s/1");
        assert_eq!(dislike_value["actor"], "https://plamenu.test/users/alice");

        let announce_value = announce(
            "plamenu.test",
            "alice",
            7,
            "https://remote.example/s/1",
            "2026-06-10T00:00:00Z",
        );
        assert_eq!(announce_value["type"], "Announce");
        assert_eq!(
            announce_value["id"],
            "https://plamenu.test/users/alice/statuses/7/activity"
        );
        assert_eq!(announce_value["to"], json!([PUBLIC]));

        let undo_value = undo("plamenu.test", "alice", like_value.clone());
        assert_eq!(undo_value["type"], "Undo");
        assert_eq!(
            undo_value["id"],
            "https://plamenu.test/users/alice#likes/5/undo"
        );
        assert_eq!(undo_value["object"], like_value);

        let delete_value = delete_note(
            "plamenu.test",
            "alice",
            "https://plamenu.test/users/alice/statuses/42",
        );
        assert_eq!(delete_value["type"], "Delete");
        assert_eq!(delete_value["object"]["type"], "Tombstone");
        assert_eq!(
            delete_value["object"]["id"],
            "https://plamenu.test/users/alice/statuses/42"
        );

        let actor_delete = delete_actor("plamenu.test", "alice");
        assert_eq!(actor_delete["type"], "Delete");
        assert_eq!(actor_delete["actor"], "https://plamenu.test/users/alice");
        assert_eq!(actor_delete["object"]["type"], "Tombstone");
        assert_eq!(
            actor_delete["object"]["id"],
            "https://plamenu.test/users/alice"
        );
    }

    #[test]
    fn update_actor_addresses_public_and_embeds_the_actor() {
        let actor = json!({
            "@context": ["ctx"],
            "id": "https://plamenu.test/users/alice",
            "type": "Person",
        });
        let value = update_actor("plamenu.test", "alice", actor.clone(), 1_750_000_000);
        assert_eq!(value["type"], "Update");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice#updates/1750000000"
        );
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["to"], json!([PUBLIC]));
        assert_eq!(value["@context"], json!(["ctx"]));
        assert_eq!(value["object"], actor);
    }

    #[test]
    fn titled_group_submission_is_a_page_with_audience() {
        let value = create_note(&NoteParams {
            kind: PostKind::Page,
            title: Some("Interesting article"),
            external_url: Some("https://example.com/article"),
            group_uri: Some("https://plamenu.test/users/rustaceans"),
            content_html: "<p>worth a read</p>",
            ..base_params()
        });
        let object = &value["object"];
        assert_eq!(object["type"], "Page", "titled posts are Lemmy-shaped");
        assert_eq!(object["name"], "Interesting article");
        assert_eq!(object["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(
            object["attachment"][0],
            json!({ "type": "Link", "href": "https://example.com/article" }),
            "the link target leads the attachments as a bare Link"
        );
        // The group rides `cc` beside the author's followers, and the claim
        // is copied onto the Create itself.
        assert_eq!(
            object["cc"],
            json!([
                "https://plamenu.test/users/alice/followers",
                "https://plamenu.test/users/rustaceans",
            ])
        );
        assert_eq!(object["to"], json!([PUBLIC]));
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
    }

    #[test]
    fn long_form_carries_its_title_as_name_and_a_heading() {
        let value = create_note(&NoteParams {
            kind: PostKind::Article,
            title: Some("Pimping my board games"),
            content_html: "<p>I recently got a 3D printer.</p>",
            language: Some("en"),
            ..base_params()
        });
        let object = &value["object"];
        assert_eq!(object["type"], "Article");
        assert_eq!(object["name"], "Pimping my board games");
        // The heading is what keeps the headline on Pleroma, GoToSocial, Sharkey
        // and Mitra, none of which render `name`.
        assert_eq!(
            object["content"],
            "<h1>Pimping my board games</h1><p>I recently got a 3D printer.</p>"
        );
        // `contentMap` must carry the same body, or a language-aware receiver
        // renders a different post than everyone else.
        assert_eq!(object["content"], object["contentMap"]["en"]);
        // No Lemmy link-post attachment: that shape belongs to `Page`.
        assert_eq!(object["attachment"], json!([]));
        // The CW stays the CW — no excerpt in `summary` (`LONGFORM_DESIGN.md`
        // §2.2), or four peers would show a spurious content warning.
        assert_eq!(object["summary"], Value::Null);
    }

    #[test]
    fn long_form_does_not_repeat_a_heading_the_author_wrote() {
        let value = create_note(&NoteParams {
            kind: PostKind::Article,
            title: Some("On typography"),
            content_html: "<h1>On typography</h1><p>Kerning matters.</p>",
            ..base_params()
        });
        assert_eq!(
            value["object"]["content"], "<h1>On typography</h1><p>Kerning matters.</p>",
            "an author's own heading is left alone"
        );
    }

    #[test]
    fn only_long_form_bakes_the_title_into_the_body() {
        // A group `Page` must not bake: Lemmy renders `name` as the post title
        // itself, so a heading would show up twice there.
        for kind in [PostKind::Page, PostKind::Event] {
            let value = create_note(&NoteParams {
                kind,
                title: Some("Interesting article"),
                content_html: "<p>worth a read</p>",
                ..base_params()
            });
            assert_eq!(
                value["object"]["content"],
                "<p>worth a read</p>",
                "{} must not bake its title",
                kind.as_str()
            );
        }
    }

    #[test]
    fn stored_kind_reads_the_column_before_the_fields() {
        use PostKind::{Article, Event, Note, Page, Question};
        // `Page` and `Article` are both titled, which is why the column decides.
        assert_eq!(PostKind::of_stored(Some("Article"), false, true), Article);
        assert_eq!(PostKind::of_stored(Some("Page"), false, true), Page);
        assert_eq!(PostKind::of_stored(Some("Event"), false, true), Event);
        // Untyped rows keep the historical inference.
        assert_eq!(PostKind::of_stored(None, true, false), Question);
        assert_eq!(PostKind::of_stored(None, false, true), Page);
        assert_eq!(PostKind::of_stored(None, false, false), Note);
    }

    #[test]
    fn untitled_group_submission_stays_a_note() {
        let value = create_note(&NoteParams {
            group_uri: Some("https://plamenu.test/users/rustaceans"),
            ..base_params()
        });
        let object = &value["object"];
        assert_eq!(object["type"], "Note");
        assert!(object.get("name").is_none());
        assert_eq!(object["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(object["attachment"], json!([]));
    }

    #[test]
    fn plain_notes_carry_no_group_fields() {
        let value = create_note(&base_params());
        assert!(value["object"].get("name").is_none());
        assert!(value["object"].get("audience").is_none());
        assert!(value.get("audience").is_none());
    }

    #[test]
    fn group_announce_wraps_the_inner_activity_verbatim() {
        let inner = json!({
            "id": "https://remote.example/activities/99",
            "type": "Create",
            "actor": "https://remote.example/users/bob",
            "object": { "id": "https://remote.example/objects/1", "type": "Page" },
            "cc": ["https://plamenu.test/users/rustaceans"],
        });
        let value = group_announce("plamenu.test", "rustaceans", 7, inner.clone());
        assert_eq!(value["type"], "Announce");
        assert_eq!(
            value["id"], "https://plamenu.test/users/rustaceans#announce/7",
            "fragment ids never collide with boost-row activity ids"
        );
        assert_eq!(value["actor"], "https://plamenu.test/users/rustaceans");
        assert_eq!(value["to"], json!([PUBLIC]));
        assert_eq!(
            value["cc"],
            json!(["https://plamenu.test/users/rustaceans/followers"])
        );
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(
            value["object"], inner,
            "FEP-1b12: inner stays byte-for-byte"
        );
    }

    #[test]
    fn accept_follow_echoes_the_original_activity() {
        let follow = json!({
            "id": "https://remote.example/activities/1",
            "type": "Follow",
            "actor": "https://remote.example/users/bob",
            "object": "https://plamenu.test/users/alice",
        });
        let accept = accept_follow("plamenu.test", "alice", 42, &follow);
        assert_eq!(accept["type"], "Accept");
        assert_eq!(accept["actor"], "https://plamenu.test/users/alice");
        assert_eq!(
            accept["id"],
            "https://plamenu.test/users/alice#accepts/follows/42"
        );
        assert_eq!(accept["object"], follow);
    }

    #[test]
    fn group_remove_is_a_delete_with_a_reason() {
        // Lemmy's mod-removal: a `Delete` carrying `summary` (the reason),
        // actor = moderator, audience = community.
        let value = group_remove(
            "plamenu.test",
            "alice",
            5,
            "https://plamenu.test/users/bob/statuses/9",
            "https://plamenu.test/users/rustaceans",
            "spam",
        );
        assert_eq!(value["type"], "Delete");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["id"], "https://plamenu.test/users/alice#remove/5");
        assert_eq!(value["summary"], "spam");
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(
            value["cc"],
            json!(["https://plamenu.test/users/rustaceans"])
        );
        assert_eq!(value["to"], json!([PUBLIC]));
        assert_eq!(value["object"]["type"], "Tombstone");
        assert_eq!(
            value["object"]["id"],
            "https://plamenu.test/users/bob/statuses/9"
        );
    }

    #[test]
    fn group_lock_matches_lemmys_shape() {
        let value = group_lock(
            "plamenu.test",
            "alice",
            3,
            "https://plamenu.test/users/bob/statuses/9",
            "https://plamenu.test/users/rustaceans",
        );
        assert_eq!(value["type"], "Lock");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["object"], "https://plamenu.test/users/bob/statuses/9");
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
        // `Undo(Lock)` reopens: the group undo wraps it verbatim and keeps the
        // community addressing Lemmy's `verify_is_public` requires.
        let undo = group_undo(
            "plamenu.test",
            "alice",
            "https://plamenu.test/users/rustaceans",
            value.clone(),
        );
        assert_eq!(undo["type"], "Undo");
        assert_eq!(undo["object"], value);
        assert_eq!(undo["to"], json!([PUBLIC]));
        assert_eq!(undo["cc"], json!(["https://plamenu.test/users/rustaceans"]));
        assert_eq!(undo["audience"], "https://plamenu.test/users/rustaceans");
    }

    #[test]
    fn group_ban_carries_target_removedata_and_expiry() {
        // Lemmy's community ban: `Block` with the community as `target`,
        // `removeData`, an optional `expires` and `summary`.
        let value = group_ban(
            "plamenu.test",
            "alice",
            8,
            "https://remote.example/users/spammer",
            "https://plamenu.test/users/rustaceans",
            false,
            Some("2026-08-01T00:00:00Z"),
            Some("repeat spam"),
        );
        assert_eq!(value["type"], "Block");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(value["object"], "https://remote.example/users/spammer");
        assert_eq!(value["target"], "https://plamenu.test/users/rustaceans");
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(value["removeData"], false);
        assert_eq!(value["expires"], "2026-08-01T00:00:00Z");
        assert_eq!(value["summary"], "repeat spam");
        // A permanent, reason-less ban omits the optional fields.
        let plain = group_ban(
            "plamenu.test",
            "alice",
            9,
            "https://remote.example/users/spammer",
            "https://plamenu.test/users/rustaceans",
            false,
            None,
            None,
        );
        assert!(plain.get("expires").is_none());
        assert!(plain.get("summary").is_none());
    }

    #[test]
    fn group_feature_and_moderator_target_the_right_collections() {
        let pin = group_feature(
            "plamenu.test",
            "alice",
            1,
            "https://plamenu.test/users/bob/statuses/9",
            "https://plamenu.test/users/rustaceans",
            true,
        );
        assert_eq!(pin["type"], "Add");
        assert_eq!(
            pin["target"],
            "https://plamenu.test/users/rustaceans/collections/featured"
        );
        assert_eq!(pin["audience"], "https://plamenu.test/users/rustaceans");
        let unpin = group_feature(
            "plamenu.test",
            "alice",
            2,
            "https://plamenu.test/users/bob/statuses/9",
            "https://plamenu.test/users/rustaceans",
            false,
        );
        assert_eq!(unpin["type"], "Remove");

        let grant = group_moderator(
            "plamenu.test",
            "alice",
            1,
            "https://remote.example/users/carol",
            "https://plamenu.test/users/rustaceans",
            true,
        );
        assert_eq!(grant["type"], "Add");
        assert_eq!(grant["object"], "https://remote.example/users/carol");
        assert_eq!(
            grant["target"],
            "https://plamenu.test/users/rustaceans/moderators"
        );
    }

    #[test]
    fn group_update_matches_lemmys_update_community() {
        // Lemmy's `UpdateCommunity`: actor = a moderator, object = the full
        // Group document, addressed to the community (`cc`/`audience`) and
        // Public. The wrapped-in-Announce delivery is asserted elsewhere.
        let doc = json!({ "type": "Group", "id": "https://plamenu.test/users/rustaceans", "name": "Rustaceans" });
        let value = group_update(
            "plamenu.test",
            "alice",
            42,
            "https://plamenu.test/users/rustaceans",
            doc.clone(),
        );
        assert_eq!(value["type"], "Update");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice#update-community/42"
        );
        assert_eq!(value["object"], doc);
        assert_eq!(value["object"]["type"], "Group");
        assert_eq!(value["to"], json!([PUBLIC]));
        assert_eq!(
            value["cc"],
            json!(["https://plamenu.test/users/rustaceans"])
        );
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
    }

    #[test]
    fn group_delete_is_a_reasonless_tombstone_delete() {
        // Lemmy's community `Delete`: owner = actor, a `Tombstone` of the
        // community as object, no `summary` (a deletion, not a mod removal).
        let value = group_delete(
            "plamenu.test",
            "alice",
            7,
            "https://plamenu.test/users/rustaceans",
        );
        assert_eq!(value["type"], "Delete");
        assert_eq!(value["actor"], "https://plamenu.test/users/alice");
        assert_eq!(
            value["id"],
            "https://plamenu.test/users/alice#delete-community/7"
        );
        assert_eq!(value["object"]["type"], "Tombstone");
        assert_eq!(
            value["object"]["id"],
            "https://plamenu.test/users/rustaceans"
        );
        assert_eq!(value["audience"], "https://plamenu.test/users/rustaceans");
        assert_eq!(value["to"], json!([PUBLIC]));
        assert!(value.get("summary").is_none());
    }
}
