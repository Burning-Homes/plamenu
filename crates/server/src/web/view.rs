//! Read-only typed views over the `entities` JSON values, and the `maud`
//! renderers that turn them into page markup.
//!
//! The JSON API and the web UI share one presentation source: the `entities`
//! module builds Mastodon `Status`/`Account` entities as `serde_json::Value`,
//! and these thin wrappers read the fields the HTML needs. Because the UI never
//! reconstructs entities of its own, it cannot drift from the API.
//!
//! Status `content` (and account `note`) arrive as server-sanitised HTML, so
//! they are emitted with `PreEscaped`; every other field is escaped by `maud`.

use std::collections::HashMap;

use fluent_bundle::FluentArgs;
use maud::{Markup, PreEscaped, html};
use plamenu_ap::emoji::replace_shortcodes;
use plamenu_ap::text::escape_html;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::languages::{self, Language};
use crate::web::clock::ViewerClock;
use crate::web::collapse;
use crate::web::i18n::Locale;
use crate::web::session::AdminCapabilities;
use crate::web::thread;

/// Swaps `:shortcode:` references for the entity's custom-emoji images —
/// what Mastodon's client-side `emojify` does, applied at render time.
/// `html` is already-sanitised markup: text between tags is rewritten,
/// anything inside a tag (attribute values, URLs) is left alone.
pub(super) fn emojify(html: &str, emojis: &[Value]) -> Markup {
    if emojis.is_empty() {
        return PreEscaped(html.to_owned());
    }
    let urls: HashMap<&str, &str> = emojis
        .iter()
        .filter_map(|e| {
            Some((
                e.get("shortcode")?.as_str()?,
                e.get("url")?.as_str().filter(|url| !url.is_empty())?,
            ))
        })
        .collect();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let (text, tag_onward) = match rest.find('<') {
            Some(open) => rest.split_at(open),
            None => (rest, ""),
        };
        out.push_str(&replace_shortcodes(text, |code| {
            urls.get(code).map(|url| emoji_img(code, url))
        }));
        if tag_onward.is_empty() {
            return PreEscaped(out);
        }
        let tag_end = tag_onward.find('>').map_or(tag_onward.len(), |i| i + 1);
        out.push_str(&tag_onward[..tag_end]);
        rest = &tag_onward[tag_end..];
    }
}

/// Render-time link pass over sanitised status content: anchors that
/// match the entity's `mentions` are rewritten to the local `/@acct` profile
/// and hashtag anchors to the local `/tags/{name}` timeline, so both stay
/// in-app instead of bouncing to the origin server. When `wrap_external` is
/// set (a signed-in viewer), every *other* absolute http(s) link is routed
/// through the in-app resolver (`/web/go`), which on click opens our local
/// copy of the actor/post it dereferences to, or falls through to the original
/// page. Links either way open in a new tab (locally-composed HTML already
/// carries `target="_blank"`, remote sanitised HTML gets `rel` from ammonia).
fn rewrite_content_links(
    html: &str,
    mentions: &[Value],
    tags: &[Value],
    wrap_external: bool,
) -> String {
    if !html.contains("<a ") {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let tag_onward = &rest[open..];
        let tag_end = tag_onward.find('>').map_or(tag_onward.len(), |i| i + 1);
        let tag = &tag_onward[..tag_end];
        if tag.starts_with("<a ") && tag.ends_with('>') {
            out.push_str(&rewrite_anchor(tag, mentions, tags, wrap_external));
        } else {
            out.push_str(tag);
        }
        rest = &tag_onward[tag_end..];
    }
    out.push_str(rest);
    out
}

/// The in-app resolver path for an external link: `/web/go?url=…`. Only
/// absolute http(s) URLs qualify; a click resolves them to our local copy of a
/// federated actor/post, falling back to the original page. Returns `None` for
/// anything else (relative links, `mailto:`, fragments), which is left as-is.
fn go_href(href: &str) -> Option<String> {
    (href.starts_with("https://") || href.starts_with("http://")).then(|| {
        let query = serde_urlencoded::to_string([("url", href)]).unwrap_or_default();
        format!("/web/go?{query}")
    })
}

/// Rewrites one sanitised `<a …>` opening tag per the [`rewrite_content_links`]
/// rules, preserving every other attribute in place.
fn rewrite_anchor(tag: &str, mentions: &[Value], tags: &[Value], wrap_external: bool) -> String {
    let attrs = parse_attrs(&tag[2..tag.len() - 1]);
    let raw_href = attrs
        .iter()
        .find(|(name, _)| *name == "href")
        .map(|(_, value)| unescape_attr(value));
    let in_app = raw_href
        .as_deref()
        .and_then(|href| in_app_href(href, mentions, tags));
    // A link we can't keep in-app by mention/tag still routes through the
    // resolver when the viewer is signed in, so a click can land on our copy.
    let external = match (&in_app, wrap_external, &raw_href) {
        (None, true, Some(href)) => go_href(href),
        _ => None,
    };
    let mut out = String::with_capacity(tag.len() + 48);
    out.push_str("<a");
    for (name, value) in &attrs {
        // An in-app link must not open a new tab; external targets are
        // re-added uniformly below.
        if *name == "target" {
            continue;
        }
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        match (*name, &in_app, &external) {
            ("href", Some(path), _) | ("href", None, Some(path)) => {
                out.push_str(&escape_html(path));
            }
            _ => out.push_str(value),
        }
        out.push('"');
    }
    if in_app.is_none() {
        out.push_str(r#" target="_blank""#);
        if !attrs.iter().any(|(name, _)| *name == "rel") {
            out.push_str(r#" rel="nofollow noopener noreferrer""#);
        }
    }
    out.push('>');
    out
}

/// Parses a sanitised tag's attribute list: `name="value"` pairs with
/// double-quoted values — the only shape ammonia and maud emit. Values are
/// returned raw (still entity-escaped). Bare (valueless) attributes are
/// dropped.
fn parse_attrs(mut rest: &str) -> Vec<(&str, &str)> {
    let mut attrs = Vec::new();
    loop {
        rest = rest.trim_start();
        let Some(eq) = rest.find('=') else { break };
        let name = rest[..eq]
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        let Some(quoted) = rest[eq + 1..].strip_prefix('"') else {
            break;
        };
        let Some(end) = quoted.find('"') else { break };
        if !name.is_empty() {
            attrs.push((name, &quoted[..end]));
        }
        rest = &quoted[end + 1..];
    }
    attrs
}

/// Decodes the entities `maud`/`ammonia` escape in attribute values, so a
/// stored href can be compared against the entity's plain-text URLs.
/// (`&amp;` last, so double-encoded input stays encoded.)
fn unescape_attr(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// `https://host/@name` → `https://host/users/name`: the AP-id path layout
/// we, Pleroma and classic Mastodon share. Pleroma links mention anchors to
/// the AP id rather than the profile page, so a mention whose `url` is the
/// `/@name` page also matches an anchor pointing at the `/users/name` id.
fn users_url_alias(url: &str) -> Option<String> {
    let (base, name) = url.rsplit_once("/@")?;
    (base.starts_with("https://") && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{base}/users/{name}"))
}

/// The in-app path for a content anchor, when it targets someone the status
/// `mentions` (the anchor carries the account's profile URL, or its AP id in
/// the shared `/users/` layout) or one of its `tags` — matched by a trailing
/// `/tags/{name}` (Mastodon) or `/tag/{name}` (Pleroma) path, since remote
/// tag anchors point at the origin server while the entity's tag URLs are
/// local.
fn in_app_href(href: &str, mentions: &[Value], tags: &[Value]) -> Option<String> {
    fn field<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
        v.get(key).and_then(Value::as_str)
    }
    if let Some(mention) = mentions.iter().find(|m| {
        field(m, "url")
            .is_some_and(|url| url == href || users_url_alias(url).as_deref() == Some(href))
    }) {
        return Some(format!("/@{}", field(mention, "acct")?));
    }
    let mut segments = href.split("://").nth(1)?.trim_end_matches('/').rsplit('/');
    let name = segments.next()?.to_lowercase();
    if !matches!(segments.next()?, "tags" | "tag") {
        return None;
    }
    tags.iter()
        .any(|t| field(t, "name") == Some(name.as_str()))
        .then(|| format!("/tags/{name}"))
}

/// [`emojify`] for a plain-text field (display name, CW text, poll option):
/// the text is HTML-escaped first, then the references are swapped.
fn emojify_text(text: &str, emojis: &[Value]) -> Markup {
    emojify(&html! { (text) }.into_string(), emojis)
}

/// One inline emoji image, Mastodon's shape: the `:shortcode:` kept as
/// alt/title so copy-paste and hover still show the reference.
fn emoji_img(code: &str, url: &str) -> String {
    let name = format!(":{code}:");
    html! { img.emoji src=(url) alt=(name) title=(name) draggable="false" loading="lazy"; }
        .into_string()
}

/// A Mastodon `Account` entity (`entities::account_json`).
pub struct Account<'a>(pub &'a Value);

impl Account<'_> {
    #[must_use]
    pub fn has_emojis(&self) -> bool {
        !self.emojis().is_empty()
    }

    fn s(&self, key: &str) -> &str {
        self.0.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    fn n(&self, key: &str) -> i64 {
        self.0.get(key).and_then(Value::as_i64).unwrap_or_default()
    }

    pub fn id(&self) -> &str {
        self.s("id")
    }

    pub fn acct(&self) -> &str {
        self.s("acct")
    }

    /// The display name, falling back to `@username` when unset.
    pub fn name(&self) -> &str {
        let display = self.s("display_name");
        if display.is_empty() {
            self.s("username")
        } else {
            display
        }
    }

    pub fn avatar(&self) -> &str {
        self.s("avatar")
    }

    /// The profile header image, absent when the account entity carries the
    /// generic missing-image placeholder.
    pub fn header(&self) -> Option<&str> {
        let header = self.s("header");
        (!header.is_empty() && !header.ends_with("/static/missing.png")).then_some(header)
    }

    pub fn avatar_description(&self) -> &str {
        self.s("avatar_description")
    }

    pub fn header_description(&self) -> &str {
        self.s("header_description")
    }

    /// The account's `ActivityPub` actor id (the entity's `uri`).
    pub fn uri(&self) -> &str {
        self.s("uri")
    }

    /// The account's `emojis` — the custom emoji its name/note/fields
    /// reference, resolved server-side by the entity builder.
    fn emojis(&self) -> &[Value] {
        self.0
            .get("emojis")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The display name with custom emoji applied, ready to embed.
    pub fn name_markup(&self) -> Markup {
        emojify_text(self.name(), self.emojis())
    }

    pub fn note_html(&self) -> &str {
        self.s("note")
    }

    /// The sanitised note HTML with custom emoji applied. When `resolve` is set
    /// (a signed-in viewer), external links are routed through the in-app
    /// resolver so links to federated actors/posts open our local copy — the
    /// case that matters for remote group descriptions (Lemmy communities link
    /// to sibling communities, users and posts on their home server).
    pub fn note_markup(&self, resolve: bool) -> Markup {
        let html = if resolve {
            rewrite_content_links(self.note_html(), &[], &[], true)
        } else {
            self.note_html().to_owned()
        };
        emojify(&html, self.emojis())
    }

    /// Whether the account requires manual approval of follow requests.
    pub fn locked(&self) -> bool {
        self.0
            .get("locked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether the account is flagged as an automated (bot) account.
    pub fn bot(&self) -> bool {
        self.0.get("bot").and_then(Value::as_bool).unwrap_or(false)
    }

    /// Whether the account is a Group actor — a local community or a
    /// remote Lemmy/Mbin community or Mitra group.
    pub fn group(&self) -> bool {
        self.0
            .get("group")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The account's publicly visible roles as `(name, color)` pairs — the
    /// API's `roles` array (local accounts, highlighted roles only), the same
    /// data 3rd-party clients render as profile badges.
    pub fn roles(&self) -> Vec<(&str, &str)> {
        self.0
            .get("roles")
            .and_then(Value::as_array)
            .map(|roles| {
                roles
                    .iter()
                    .filter_map(|role| {
                        let name = role.get("name").and_then(Value::as_str)?;
                        let color = role.get("color").and_then(Value::as_str).unwrap_or("");
                        (!name.is_empty()).then_some((name, color))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The sigil that introduces this account's handle in the UI: `!` for a
    /// group (the Lemmy/Mbin community convention, e.g. `!memes@lemmy.world`),
    /// `@` for everyone else.
    pub fn handle_prefix(&self) -> &'static str {
        if self.group() { "!" } else { "@" }
    }

    /// The profile metadata rows as ready-to-embed markup (escaped `name`,
    /// sanitised `value` HTML), custom emoji applied to both, plus whether the
    /// row's link is rel="me"-verified. Empty rows are dropped so the
    /// profile only shows filled-in fields.
    pub fn fields(&self, resolve: bool) -> Vec<(Markup, Markup, bool)> {
        self.0
            .get("fields")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        let get = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or_default();
                        let (name, value) = (get("name"), get("value"));
                        let verified = row.get("verified_at").and_then(Value::as_str).is_some();
                        let value = if resolve {
                            rewrite_content_links(value, &[], &[], true)
                        } else {
                            value.to_owned()
                        };
                        (!name.is_empty() || !value.is_empty()).then(|| {
                            (
                                emojify_text(name, self.emojis()),
                                emojify(&value, self.emojis()),
                                verified,
                            )
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn followers_count(&self) -> i64 {
        self.n("followers_count")
    }

    pub fn following_count(&self) -> i64 {
        self.n("following_count")
    }

    pub fn statuses_count(&self) -> i64 {
        self.n("statuses_count")
    }

    /// In-app profile path (the handle is `username` locally, `username@domain`
    /// for remote accounts). A **remote** Group uses the disambiguating
    /// `/!acct` route, since a same-named person may share its handle; everyone
    /// else — including local groups, whose namespace is exclusive — uses
    /// `/@acct`.
    pub fn profile_path(&self) -> String {
        if self.group() && self.acct().contains('@') {
            format!("/!{}", self.acct())
        } else {
            format!("/@{}", self.acct())
        }
    }

    /// The account's canonical web URL — its profile page on its home server
    /// (the same page locally, the origin's profile for remote accounts).
    pub fn url(&self) -> &str {
        self.s("url")
    }

    /// When the account was created (RFC 3339), i.e. when it joined.
    pub fn created_at(&self) -> &str {
        self.s("created_at")
    }

    /// Whether the account lives on another server (its acct carries a domain).
    pub fn is_remote(&self) -> bool {
        self.acct().contains('@')
    }

    /// The account's home domain, for remote accounts only.
    pub fn remote_domain(&self) -> Option<&str> {
        self.acct().split_once('@').map(|(_, domain)| domain)
    }
}

/// A Mastodon `Status` entity (`entities::render_status`).
pub struct Status<'a>(pub &'a Value);

impl<'a> Status<'a> {
    fn s(&self, key: &str) -> &str {
        self.0.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    fn n(&self, key: &str) -> i64 {
        self.0.get(key).and_then(Value::as_i64).unwrap_or_default()
    }

    fn b(&self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or_default()
    }

    pub fn id(&self) -> &str {
        self.s("id")
    }

    pub fn account(&self) -> Account<'a> {
        Account(&self.0["account"])
    }

    pub fn content_html(&self) -> &str {
        self.s("content")
    }

    /// The status' `emojis` — the custom emoji its text (spoiler, body,
    /// poll options) references, resolved server-side by the entity builder.
    fn emojis(&self) -> &[Value] {
        self.0
            .get("emojis")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// Whether the member borrow page has a body/poll emoji or a resolvable
    /// custom-emoji reaction to offer. Reaction URLs are client-safe local or
    /// proxy URLs for both local and remote source emoji.
    fn has_borrowable_emojis(&self) -> bool {
        !self.emojis().is_empty()
            || self
                .0
                .get("emoji_reactions")
                .and_then(Value::as_array)
                .is_some_and(|groups| {
                    groups.iter().any(|group| {
                        group
                            .get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| !url.is_empty())
                    })
                })
    }

    /// The status' `mentions` — the accounts its content references, resolved
    /// server-side by the entity builder.
    fn mentions(&self) -> &[Value] {
        self.0
            .get("mentions")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The status' `tags` — the hashtags its content carries.
    fn tags(&self) -> &[Value] {
        self.0
            .get("tags")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The `deleted: true` marker on the soft-deleted thread-stub placeholder
    /// (`entities::render_plain`) — the card branches to a tombstone on it.
    fn deleted(&self) -> bool {
        self.b("deleted")
    }

    /// The web-only `_boosters` hint set by `collapse::collapse`: the accounts
    /// whose boosts of this post were merged into one card, newest first.
    /// Empty on an uncollapsed card, which is every card the API ever sees.
    fn boosters(&self) -> &[Value] {
        self.0
            .get(collapse::BOOSTERS)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// Whether a merged card is the boosted post itself rather than a boost of
    /// it — its booster line reads "also boosted by …".
    fn also_boosted(&self) -> bool {
        self.b(collapse::ALSO_BOOSTED)
    }

    /// The web-only `_tag_source` hint set by `pages::annotate_tag_sources`: the
    /// followed hashtag name(s) that pulled this post into the home feed, empty
    /// when the post is not a followed-tag injection.
    fn tag_source(&self) -> Vec<&str> {
        self.0
            .get("_tag_source")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// The sanitised content HTML with the link pass and custom emoji applied.
    pub fn content_markup(&self, resolve: bool) -> Markup {
        let content =
            rewrite_content_links(self.content_html(), self.mentions(), self.tags(), resolve);
        emojify(&content, self.emojis())
    }

    // The post's real title (Lemmy/PeerTube/Article/Event `name`) and a link
    // post's target URL are folded into `content` by the entity serializer
    // (`entities::fold_typed_content`), so the card renders them from the
    // content markup rather than reading dedicated fields here.

    /// The source object's AS type when it wasn't a plain Note
    /// (`Article` | `Page` | `Video` | `Audio` | `Image` | `Event` |
    /// `Document`).
    pub fn object_type(&self) -> Option<&str> {
        self.0
            .get("object_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// The typed event sidecar (`Event` objects).
    pub fn event(&self) -> Option<&Value> {
        self.0.get("event").filter(|event| event.is_object())
    }

    pub fn spoiler_text(&self) -> &str {
        self.s("spoiler_text")
    }

    /// The content warning with custom emoji applied, ready to embed.
    pub fn spoiler_markup(&self) -> Markup {
        emojify_text(self.spoiler_text(), self.emojis())
    }

    /// Whether the post is flagged sensitive — media is then revealed only on
    /// demand (Mastodon's blur-then-click; here a `<details>` toggle).
    pub fn sensitive(&self) -> bool {
        self.b("sensitive")
    }

    pub fn created_at(&self) -> &str {
        self.s("created_at")
    }

    /// The last edit instant, when the post has been edited.
    pub fn edited_at(&self) -> Option<&str> {
        self.0
            .get("edited_at")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    pub fn visibility(&self) -> &str {
        self.s("visibility")
    }

    /// The post's declared language (BCP 47 code), when set.
    pub fn language(&self) -> Option<&str> {
        self.0
            .get("language")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// The posting application's name — present only where the entity carries
    /// it (own posts, or authors who opted in to showing it).
    pub fn application_name(&self) -> Option<&str> {
        self.0
            .get("application")
            .and_then(|app| app.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    pub fn in_reply_to_id(&self) -> Option<&str> {
        self.0.get("in_reply_to_id").and_then(Value::as_str)
    }

    /// The AP URI of a reply parent we never fetched (`in_reply_to_id` null but
    /// the object referenced a parent) — a Plamenu extension. `None` when the
    /// parent resolved locally or the post isn't a reply. Lets the thread and
    /// card views show an "unfetched parent" notice instead of orphaning it.
    pub fn in_reply_to_uri(&self) -> Option<&str> {
        self.0
            .get("in_reply_to_uri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.is_empty())
    }

    /// The handle this status replies to, resolved without extra lookups:
    /// `in_reply_to_account_id` matched against the status' own `mentions`
    /// (replies conventionally mention their target), falling back to the
    /// author for self-threads. `None` when the parent author isn't in view.
    pub fn reply_to_acct(&self) -> Option<&str> {
        let target = self
            .0
            .get("in_reply_to_account_id")
            .and_then(Value::as_str)?;
        let account = self.0.get("account")?;
        if account.get("id").and_then(Value::as_str) == Some(target) {
            return account.get("acct").and_then(Value::as_str);
        }
        self.0
            .get("mentions")
            .and_then(Value::as_array)?
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(target))?
            .get("acct")
            .and_then(Value::as_str)
    }

    /// The one-line look at this reply's parent, when the parent is not itself
    /// on the page and the viewer may see it. `None` on the overwhelming
    /// majority of cards, which are not replies at all.
    pub fn reply_peek(&self) -> Option<&'a Value> {
        self.0
            .get(thread::REPLY_PEEK)
            .filter(|peek| peek.is_object())
    }

    /// The boosted status, when this entity is a boost wrapper.
    pub fn reblog(&self) -> Option<Status<'a>> {
        match self.0.get("reblog") {
            Some(inner) if !inner.is_null() => Some(Status(inner)),
            _ => None,
        }
    }

    pub fn media(&self) -> &'a [Value] {
        self.0
            .get("media_attachments")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    pub fn replies_count(&self) -> i64 {
        self.n("replies_count")
    }

    pub fn reblogs_count(&self) -> i64 {
        self.n("reblogs_count")
    }

    pub fn favourites_count(&self) -> i64 {
        self.n("favourites_count")
    }

    pub fn quotes_count(&self) -> i64 {
        self.n("quotes_count")
    }

    pub fn favourited(&self) -> bool {
        self.b("favourited")
    }

    /// Whether this status is a group post — where the favourite
    /// doubles as the upvote and the card wears the vote cluster.
    pub fn group_post(&self) -> bool {
        self.b("group_post")
    }

    pub fn downvotes_count(&self) -> i64 {
        self.n("downvotes_count")
    }

    pub fn downvoted(&self) -> bool {
        self.b("downvoted")
    }

    /// A group post's score: upvotes (favourites) minus downvotes.
    pub fn score(&self) -> i64 {
        self.favourites_count() - self.downvotes_count()
    }

    pub fn reblogged(&self) -> bool {
        self.b("reblogged")
    }

    pub fn bookmarked(&self) -> bool {
        self.b("bookmarked")
    }

    /// Whether the viewer muted this status' conversation.
    pub fn muted(&self) -> bool {
        self.b("muted")
    }

    /// Whether the owner pinned this status — `None` when the entity carries
    /// no `pinned` field (not the viewer's own post, or not pinnable).
    pub fn pinned(&self) -> Option<bool> {
        self.0.get("pinned").and_then(Value::as_bool)
    }

    /// The group-moderation context the web layer injected onto this entity
    /// (via [`inject_group_mod`]) for a moderator viewing the post in a local
    /// group they run — drives the overflow menu's Moderate section. `None` for
    /// everyone else. Web-only: never part of the API entity.
    pub fn group_mod(&self) -> Option<GroupMod> {
        let value = self.0.get("_group_mod")?;
        Some(GroupMod {
            group_id: value.get("group_id")?.as_i64()?,
            pinned: value
                .get("pinned")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            locked: value
                .get("locked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// The community/communities this post was submitted to. The shared Status
    /// entity renderer carries these on every surface, including bare originals
    /// that have lost their Group Announce wrapper.
    pub fn group_attribution(&self) -> Vec<Account<'a>> {
        self.0
            .get("groups")
            .and_then(Value::as_array)
            .map_or(Vec::new(), |items| items.iter().map(Account).collect())
    }

    /// The viewer's `filtered` results (`FilterResult` entities) — empty for
    /// anonymous viewers and unmatched statuses alike.
    fn filtered(&self) -> &'a [Value] {
        self.0
            .get("filtered")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The status' human web URL: the local permalink page for local posts,
    /// the origin's published page for remote ones.
    pub fn url(&self) -> &str {
        self.s("url")
    }

    /// The status' `ActivityPub` object id (the entity's `uri`).
    pub fn uri(&self) -> &str {
        self.s("uri")
    }

    /// The Pleroma emoji-reaction groups (`pleroma.emoji_reactions`): one
    /// entry per emoji with its `name`, `count`, `me` flag and — for custom
    /// emoji — the image `url`.
    fn reactions(&self) -> &[Value] {
        self.0
            .get("pleroma")
            .and_then(|p| p.get("emoji_reactions"))
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// Whether the author lives on another server (the acct carries a domain).
    pub fn is_remote(&self) -> bool {
        self.account().acct().contains('@')
    }

    /// The author's domain, for remote statuses only.
    pub fn remote_domain(&self) -> Option<&str> {
        let (_, domain) = self
            .0
            .get("account")?
            .get("acct")?
            .as_str()?
            .split_once('@')?;
        Some(domain)
    }

    /// Boostable visibilities only; followers-only and direct cannot be
    /// reblogged (Mastodon hides the control).
    pub fn boostable(&self) -> bool {
        matches!(self.s("visibility"), "public" | "unlisted")
    }

    /// Whether the current signed-in viewer may start a quote under the
    /// target's advertised policy. Mastodon's composer accepts only automatic
    /// and manual states; denied/missing/unknown policies are not actionable.
    pub fn quotable(&self) -> bool {
        self.boostable()
            && matches!(
                self.0
                    .get("quote_approval")
                    .and_then(|approval| approval.get("current_user"))
                    .and_then(Value::as_str),
                Some("automatic" | "manual")
            )
    }

    /// The attached poll entity, if any.
    pub fn poll(&self) -> Option<&'a Value> {
        self.0.get("poll").filter(|v| !v.is_null())
    }

    /// The link preview card entity, if any.
    pub fn card(&self) -> Option<&'a Value> {
        self.0.get("card").filter(|v| !v.is_null())
    }

    /// The quoted status, when this status quotes another (FEP-044f).
    pub fn quote(&self) -> Option<Status<'a>> {
        let inner = self.0.get("quote")?.get("quoted_status")?;
        (!inner.is_null()).then_some(Status(inner))
    }

    /// The quote attachment's `state` (`accepted`, `pending`, `deleted`,
    /// `revoked`, `rejected`), when this status quotes another.
    pub fn quote_state(&self) -> Option<&str> {
        self.0.get("quote")?.get("state").and_then(Value::as_str)
    }

    /// The quoted status id of a shallow `accepted` quote — the nesting-capped
    /// shape where the target is referenced but not embedded.
    pub fn quoted_status_id(&self) -> Option<&str> {
        self.0
            .get("quote")?
            .get("quoted_status_id")
            .and_then(Value::as_str)
    }

    /// The post's quote policy (`quote_approval`) as the composer dropdown's
    /// glyph plus a one-line label, or `None` when the entity carries no
    /// policy. The glyphs match [`QUOTE_POLICIES`] so the chip reads the same
    /// as the "who can quote" selector; the manual-approval and disabled
    /// states both show the restricted glyph, distinguished by the label.
    pub fn quote_policy_meta(&self) -> Option<(&'static str, &'static str)> {
        let approval = self.0.get("quote_approval")?;
        let has = |key: &str, needle: &str| {
            approval
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(needle)))
        };
        Some(if has("automatic", "public") {
            ("quote-any", "Anyone can quote")
        } else if has("automatic", "followers") {
            ("quote-followers", "Followers can quote")
        } else if has("manual", "public") || has("manual", "followers") {
            ("quote-none", "Quotes need approval")
        } else {
            ("quote-none", "Quotes disabled")
        })
    }

    /// The post's quote policy as the client string the interaction-policy
    /// endpoint accepts (`public` | `followers` | `nobody`) — the preset for
    /// the overflow menu's policy selector. A manual policy (unsettable from
    /// this UI) reads as `nobody`, the closest selectable value.
    pub fn quote_policy_value(&self) -> Option<&'static str> {
        let approval = self.0.get("quote_approval")?;
        let automatic = |needle: &str| {
            approval
                .get("automatic")
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(needle)))
        };
        Some(if automatic("public") {
            "public"
        } else if automatic("followers") {
            "followers"
        } else {
            "nobody"
        })
    }

    /// In-app thread permalink, `/@acct/id`.
    pub fn permalink(&self) -> String {
        format!("/@{}/{}", self.account().acct(), self.id())
    }

    /// Thread permalink with a `#post-id` fragment so the browser scrolls to
    /// this post on load — a reply deep in a long thread lands in view instead
    /// of at the OP. The target `id` is on every `article.status`
    /// ([`status_article`]); the focus post always carries it, so this resolves
    /// even when descendants paginate. Pure-HTML anchor, no scripting. Kept
    /// separate from [`permalink`](Self::permalink) so canonical/OG URLs and
    /// `/history`, `/reblogs` … sub-paths stay fragment-free.
    pub fn permalink_anchored(&self) -> String {
        format!(
            "/@{}/{}#post-{}",
            self.account().acct(),
            self.id(),
            self.id()
        )
    }

    /// The applied translation's attribution: provider name plus the
    /// detected source language, when this entity renders translated.
    fn translation(&self) -> Option<&Value> {
        self.0.get("_translation").filter(|value| value.is_object())
    }
}

/// How media should be revealed in the timeline, mirroring the viewer's
/// `reading:expand:media` preference (Mastodon's "Media display").
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum MediaDisplay {
    /// Hide media only when the post is flagged sensitive (the default).
    #[default]
    Default,
    /// Always reveal media, even when flagged sensitive.
    ShowAll,
    /// Always gate media behind a toggle, sensitive or not.
    HideAll,
}

/// The viewer's reading preferences that affect how a status renders, threaded
/// alongside [`Ctx`]. Defaults match a logged-out viewer (and the server
/// defaults), so anonymous pages can use `ViewPrefs::default()`.
#[derive(Clone, Default)]
pub struct ViewPrefs {
    pub expand_media: MediaDisplay,
    /// Reveal content-warning text without the extra click.
    pub expand_spoilers: bool,
    /// Auto-play gifv loops in the feed; off renders a still poster instead.
    pub autoplay_gifs: bool,
    /// The language the per-post Translate control targets; `None` (anonymous
    /// viewers, or no backend configured) hides the control.
    pub translate_to: Option<String>,
    /// The backend's `source → [targets]` language map, for offering the
    /// control only on posts the backend can actually translate; `None`
    /// hides it (no backend, or its language list is unavailable).
    pub translate_languages:
        Option<std::sync::Arc<std::collections::BTreeMap<String, Vec<String>>>>,
}

/// Which filter `context` applies to the statuses on the page being rendered
/// — the web equivalent of Mastodon's client-side `contextType`. `None` on
/// [`Ctx`] skips filtering entirely, for deliberate single-post surfaces like
/// the composer's reply/quote preview.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilterContext {
    Home,
    Notifications,
    Public,
    Thread,
    Account,
    /// Search results match filters' `public` context, but a `hide` match
    /// only collapses (Mastodon's `warnInsteadOfHide`): the viewer asked for
    /// these results, so nothing silently vanishes.
    Search,
}

impl FilterContext {
    /// The server-side context tag matched against each filter's `context`.
    fn server_side(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Notifications => "notifications",
            Self::Public | Self::Search => "public",
            Self::Thread => "thread",
            Self::Account => "account",
        }
    }

    /// Whether a `hide` match is downgraded to the warn bar here.
    fn warn_instead_of_hide(self) -> bool {
        matches!(self, Self::Search)
    }
}

/// What the viewer's filters say about presenting one status on this page
/// drop it, collapse it behind a named "Filtered" bar, and/or gate
/// its media — the web equivalent of Mastodon's `makeGetStatus` filter logic.
#[derive(Default)]
struct FilterVerdict {
    /// A `hide` filter matched: drop the status without a trace.
    hide: bool,
    /// Titles of matching warn filters (plus hide ones on surfaces that
    /// downgrade them): collapse the body behind the bar naming them.
    warn: Vec<String>,
    /// Titles of matching blur filters: gate only the media.
    blur: Vec<String>,
}

/// Applies the page's filter context to a status entity's `filtered` results.
/// A boost is judged by its target (which carries the match), and — like
/// Mastodon — the viewer's own posts are never filtered. `detail` marks the
/// thread page's focused post, where `hide` downgrades to the warn bar: the
/// viewer deliberately navigated here.
fn filter_verdict(status: &Status, ctx: &Ctx, detail: bool) -> FilterVerdict {
    let mut verdict = FilterVerdict::default();
    let Some(context) = ctx.filter_context else {
        return verdict;
    };
    let proper = status.reblog().unwrap_or(Status(status.0));
    if ctx.viewer_id == Some(proper.account().id()) {
        return verdict;
    }
    let warn_instead_of_hide = detail || context.warn_instead_of_hide();
    let results = if proper.filtered().is_empty() {
        status.filtered()
    } else {
        proper.filtered()
    };
    for result in results {
        let Some(filter) = result.get("filter") else {
            continue;
        };
        let applies = filter
            .get("context")
            .and_then(Value::as_array)
            .is_some_and(|ctxs| {
                ctxs.iter()
                    .any(|c| c.as_str() == Some(context.server_side()))
            });
        if !applies {
            continue;
        }
        let title = filter
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match filter.get("filter_action").and_then(Value::as_str) {
            Some("hide") if !warn_instead_of_hide => verdict.hide = true,
            Some("blur") => verdict.blur.push(title),
            _ => verdict.warn.push(title),
        }
    }
    verdict
}

/// The collapsed-post bar: "Filtered: {names}" with the whole bar as the
/// no-JS show-anyway toggle, wrapping the gated markup.
fn filter_bar(titles: &[String], gated: &Markup, locale: Locale) -> Markup {
    let mut args = FluentArgs::new();
    args.set("filters", titles.join(", "));
    html! {
        details.status__filtered {
            summary {
                span.status__filtered-label { (locale.text_with("status-filtered", &args)) }
                span.status__filtered-show { (locale.text("status-filter-show")) }
            }
            (gated)
        }
    }
}

/// Group-moderation context for one status, injected onto the rendered entity
/// by the web layer (see [`inject_group_mod`]) when the viewer moderates the
/// local group the post lives in. Drives the overflow menu's Moderate section.
pub struct GroupMod {
    pub group_id: i64,
    pub pinned: bool,
    pub locked: bool,
}

/// The moderation-target status id a rendered entity actually shows: the
/// reblogged object for a boost (a group's announce, which the card unwraps),
/// else the entity itself. Used to key group-moderation state onto entities.
pub fn displayed_status_id(entity: &Value) -> Option<i64> {
    let shown = entity
        .get("reblog")
        .filter(|reblog| reblog.is_object())
        .unwrap_or(entity);
    shown.get("id").and_then(Value::as_str)?.parse().ok()
}

/// Attaches group-moderation context to a rendered status entity so the
/// overflow menu can offer Remove / Lock / Pin to a group moderator. For a
/// boost (a group's announce) the context rides the *reblogged* object, which
/// is what the card renders and the menu acts on — mirroring
/// [`displayed_status_id`].
pub fn inject_group_mod(entity: &mut Value, group_id: i64, pinned: bool, locked: bool) {
    let target = if entity.get("reblog").is_some_and(Value::is_object) {
        entity.get_mut("reblog").expect("reblog object present")
    } else {
        entity
    };
    if let Some(object) = target.as_object_mut() {
        object.insert(
            "_group_mod".to_owned(),
            serde_json::json!({ "group_id": group_id, "pinned": pinned, "locked": locked }),
        );
    }
}

/// The per-request rendering context threaded through the status renderers:
/// the CSRF token (present only for signed-in viewers, which turns the action
/// forms live), the viewer's own account id (to reveal owner-only controls),
/// the path to return to after a POST action, the page's filter context, and
/// the viewer's reading prefs.
pub struct Ctx<'a> {
    pub csrf: Option<&'a str>,
    pub viewer_id: Option<&'a str>,
    pub return_to: &'a str,
    pub filter_context: Option<FilterContext>,
    pub prefs: ViewPrefs,
    pub locale: Locale,
    /// How this viewer reads a timestamp. Built from the same locale as
    /// `locale` above, so the two can never disagree.
    pub clock: ViewerClock,
    /// Exact admin destinations this viewer may open. This is deliberately
    /// target-oriented rather than a coarse `is_staff` flag, so every
    /// rendered shortcut agrees with the destination page's own gate.
    pub admin: AdminCapabilities,
}

impl Ctx<'_> {
    fn owns(&self, status: &Status) -> bool {
        self.viewer_id.is_some() && self.viewer_id == Some(status.account().id())
    }
}

/// Renders a coarse relative age ("3m", "5h", "2d") from an RFC 3339 instant,
/// falling back to the raw date on a parse failure. JS may later refine this
/// in place; the static label keeps the timeline legible without scripting.
fn relative_time(iso: &str, locale: Locale) -> String {
    let Ok(then) = OffsetDateTime::parse(iso, &Rfc3339) else {
        return iso.to_owned();
    };
    let secs = (OffsetDateTime::now_utc() - then).whole_seconds().max(0);
    let (message, count) = match secs {
        s if s < 60 => ("time-ago-seconds", s),
        s if s < 3600 => ("time-ago-minutes", s / 60),
        s if s < 86_400 => ("time-ago-hours", s / 3600),
        s if s < 2_592_000 => ("time-ago-days", s / 86_400),
        s => ("time-ago-months", s / 2_592_000),
    };
    let mut args = FluentArgs::new();
    args.set("count", count);
    locale.text_with(message, &args)
}

/// The glyph name and human label for a status' visibility, from the same
/// inventory the composer's selector uses. Unknown values render nothing.
fn visibility_meta(visibility: &str) -> Option<(&'static str, &'static str)> {
    VISIBILITY_LEVELS
        .iter()
        .find(|(value, ..)| *value == visibility)
        .map(|&(_, label, glyph, _)| (glyph, label))
}

fn visibility_label(visibility: &str, locale: Locale) -> Option<String> {
    let id = match visibility {
        "public" => "visibility-public",
        "unlisted" => "visibility-unlisted",
        "private" => "visibility-private",
        "direct" => "visibility-direct",
        "local" => "visibility-local",
        _ => return None,
    };
    Some(locale.text(id))
}

fn localized_arg(locale: Locale, id: &str, name: &'static str, value: &str) -> String {
    let mut args = FluentArgs::new();
    args.set(name, value);
    locale.text_with(id, &args)
}

/// The target-specific destinations available in a privileged-tools menu.
/// A local Group has its dedicated group console; remote Groups currently
/// remain account records because the server has no remote-group admin page.
struct AdminTargetLinks {
    target: Option<(String, String)>,
    server: Option<(String, String)>,
}

impl AdminTargetLinks {
    fn for_account(account: &Account, admin: AdminCapabilities, locale: Locale) -> Self {
        let handle = format!("{}{}", account.handle_prefix(), account.acct());
        let target = if account.group() && !account.is_remote() {
            admin.manage_groups.then(|| {
                (
                    format!("/admin/groups/{}", account.id()),
                    localized_arg(locale, "moderation-open-group", "account", &handle),
                )
            })
        } else {
            admin.manage_users.then(|| {
                (
                    format!("/admin/accounts/{}", account.id()),
                    localized_arg(locale, "moderation-open-account", "account", &handle),
                )
            })
        };
        let server = account.remote_domain().and_then(|domain| {
            admin.manage_federation.then(|| {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(domain.as_bytes()).collect();
                (
                    format!("/admin/instances/{encoded}"),
                    localized_arg(locale, "moderation-open-server", "domain", domain),
                )
            })
        });
        Self { target, server }
    }

    fn is_empty(&self) -> bool {
        self.target.is_none() && self.server.is_none()
    }

    fn markup(&self) -> Markup {
        html! {
            @if let Some((href, label)) = &self.target {
                a.status__menu-item href=(href) { (label) }
            }
            @if let Some((href, label)) = &self.server {
                a.status__menu-item href=(href) { (label) }
            }
        }
    }
}

/// A profile/group's shield menu. `manage_group_href` is affiliation-based
/// community authority rather than a server role, but belongs in the same
/// privileged-tools affordance so a group owner who is also staff never gets
/// two competing shield buttons.
pub fn account_privileged_menu(
    account: &Account,
    admin: AdminCapabilities,
    manage_group_href: Option<&str>,
    locale: Locale,
) -> Option<Markup> {
    let links = AdminTargetLinks::for_account(account, admin, locale);
    let borrow_href =
        (admin.manage_custom_emojis && account.is_remote() && !account.emojis().is_empty())
            .then(|| format!("/admin/custom-emojis/borrow/account/{}", account.id()));
    if links.is_empty() && manage_group_href.is_none() && borrow_href.is_none() {
        return None;
    }
    let title = locale.text("moderation-tools");
    Some(html! {
        details.status__menu.privileged-menu data-status-menu data-privileged-menu {
            summary.action title=(title) aria-label=(title) aria-haspopup="menu" {
                (icon("shield"))
            }
            div.status__menu-pop role="menu" {
                @if let Some(href) = manage_group_href {
                    a.status__menu-item href=(href) { (locale.text("profile-manage-group")) }
                }
                (links.markup())
                @if let Some(href) = &borrow_href {
                    a.status__menu-item href=(href) {
                        (locale.text("moderation-borrow-emojis"))
                    }
                }
            }
        }
    })
}

/// An inline SVG icon (24×24, `currentColor`, feather-style). Unknown names
/// render nothing.
#[allow(clippy::too_many_lines)]
pub fn icon(name: &str) -> Markup {
    let path = match name {
        "reply" => r#"<path d="M9 17l-5-5 5-5"/><path d="M4 12h11a5 5 0 0 1 5 5v1"/>"#,
        "boost" => {
            r#"<path d="M17 2l4 4-4 4"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><path d="M7 22l-4-4 4-4"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/>"#
        }
        "favourite" => {
            r#"<path d="M12 17.3l-6.2 3.7 1.6-7L2 9.2l7.1-.6L12 2l2.9 6.6 7.1.6-5.4 4.8 1.6 7z"/>"#
        }
        "bookmark" => r#"<path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/>"#,
        // The group-post vote arrows.
        "upvote" => r#"<path d="M12 19V5"/><path d="M5 12l7-7 7 7"/>"#,
        "downvote" => r#"<path d="M12 5v14"/><path d="M19 12l-7 7-7-7"/>"#,
        // The user-lists nav entry and list-page headers.
        "list" => {
            r#"<path d="M8 6h13"/><path d="M8 12h13"/><path d="M8 18h13"/><path d="M3 6h.01"/><path d="M3 12h.01"/><path d="M3 18h.01"/>"#
        }
        // Account collections (FEP-7aa9): a curated set, drawn as a star.
        "collection" => {
            r#"<path d="M12 2l2.9 6.3 6.9.8-5.1 4.7 1.4 6.8L12 18l-6 3.4 1.4-6.8L2.3 9.1l6.9-.8z"/>"#
        }
        // Mastodon's Material `format_quote` / `format_quote_off` glyphs. The
        // paths retain their native 960-unit coordinates. `format_quote` uses
        // only a 680×480 patch of that canvas, so give it a centred optical
        // scale to match the neighbouring Feather icons; `format_quote_off`'s
        // diagonal already occupies almost the full canvas.
        "quote" => {
            r#"<path fill="currentColor" stroke="none" transform="translate(-3.625 27) scale(.03125)" d="m228-240 92-160q-66 0-113-47t-47-113q0-66 47-113t113-47q66 0 113 47t47 113q0 23-5.5 42.5T458-480L320-240h-92Zm360 0 92-160q-66 0-113-47t-47-113q0-66 47-113t113-47q66 0 113 47t47 113q0 23-5.5 42.5T818-480L680-240h-92Z"/>"#
        }
        "quote-off" => {
            r#"<path fill="currentColor" stroke="none" transform="translate(0 24) scale(.025)" d="M791-56 425-422 320-240h-92l92-160q-66 0-113-47t-47-113q0-27 8.5-51t23.5-44L56-791l56-57 736 736-57 56Zm-55-281L520-553v-7q0-66 47-113t113-47q66 0 113 47t47 113q0 23-5.5 42.5T818-480l-82 143Z"/>"#
        }
        "delete" => {
            r#"<path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/>"#
        }
        "home" => {
            r#"<path d="M3 11l9-8 9 8"/><path d="M5 10v10a1 1 0 0 0 1 1h4v-6h4v6h4a1 1 0 0 0 1-1V10"/>"#
        }
        "explore" => {
            r#"<circle cx="12" cy="12" r="10"/><path d="M16.2 7.8l-2.9 6.4-6.4 2.9 2.9-6.4z"/>"#
        }
        "feeds" => r#"<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>"#,
        "bell" => {
            r#"<path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 0 1-3.4 0"/>"#
        }
        "search" => r#"<circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/>"#,
        "compose" => {
            r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>"#
        }
        "profile" => {
            r#"<circle cx="12" cy="8" r="4"/><path d="M4 21v-1a6 6 0 0 1 6-6h4a6 6 0 0 1 6 6v1"/>"#
        }
        "logout" => {
            r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="M16 17l5-5-5-5"/><path d="M21 12H9"/>"#
        }
        // The account-switcher affordance: two arrows swapping direction.
        "switch" => {
            r#"<path d="M7 4v13"/><path d="M4 7l3-3 3 3"/><path d="M17 20V7"/><path d="M20 17l-3 3-3-3"/>"#
        }
        "settings" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09a1.65 1.65 0 0 0-1.08-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09a1.65 1.65 0 0 0 1.51-1.08 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>"#
        }
        "shield" => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>"#,
        // The locked-thread marker: a padlock.
        "lock" => {
            r#"<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#
        }
        "menu" => r#"<path d="M3 6h18"/><path d="M3 12h18"/><path d="M3 18h18"/>"#,
        "download" => {
            r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/>"#
        }
        "upload" => {
            r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M17 8l-5-5-5 5"/><path d="M12 3v12"/>"#
        }
        "alert" => {
            r#"<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
        }
        "poll" => {
            r#"<path d="M3 21h18"/><path d="M6 21V11"/><path d="M12 21V4"/><path d="M18 21v-6"/>"#
        }
        // Composer selector chrome: dropdown caret and selected check.
        "chevron" => r#"<path d="M6 9l6 6 6-6"/>"#,
        // The custom-emoji picker trigger.
        "emoji" => {
            r#"<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><path d="M9 9h.01"/><path d="M15 9h.01"/>"#
        }
        "check" => r#"<path d="M20 6L9 17l-5-5"/>"#,
        // The action bar's "…" overflow-menu trigger.
        "more" => {
            r#"<circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/>"#
        }
        // The pinned-post marker atop profiles: a thumbtack.
        "pin" => r#"<path d="M12 17v5"/><path d="M9 3h6l-1 6 3 3v2H7v-2l3-3z"/>"#,
        // A link post's external target: box with an out-arrow.
        "link" => {
            r#"<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><path d="M15 3h6v6"/><path d="M10 14L21 3"/>"#
        }
        // Event posts: the date box.
        "calendar" => {
            r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M16 2v4"/><path d="M8 2v4"/><path d="M3 10h18"/>"#
        }
        // The bot flag on automated accounts: a robot head.
        "bot" => {
            r#"<rect x="4" y="9" width="16" height="11" rx="2"/><path d="M12 5v4"/><circle cx="12" cy="4" r="1"/><path d="M9 13v2.5"/><path d="M15 13v2.5"/>"#
        }
        // Group actors: two heads, for the flag and the boost line.
        "group" => {
            r#"<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20a6.5 6.5 0 0 1 13 0"/><circle cx="17.5" cy="9.5" r="2.5"/><path d="M14.5 15.5a5 5 0 0 1 7 4.5"/>"#
        }
        // Sandboxed Webxdc applications: a compact four-tile launcher.
        "apps" => {
            r#"<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>"#
        }
        // Quote-policy glyphs — a quotation-mark family, deliberately distinct
        // from the visibility set (no globe) so the two controls don't read
        // alike. "Anyone" is the canonical double quote mark; followers/nobody
        // hang a people / slash badge off the same quote-bubble base.
        "quote-any" => {
            r#"<path d="M5 4a2 2 0 0 0-2 2v5a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2v2a4 4 0 0 0 4-4V6a2 2 0 0 0-2-2z"/><path d="M16 4a2 2 0 0 0-2 2v5a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2v2a4 4 0 0 0 4-4V6a2 2 0 0 0-2-2z"/>"#
        }
        "quote-followers" => {
            r#"<path d="M4 4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h1l2 3 2-3h1a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2z"/><circle cx="18" cy="8" r="3"/><path d="M13.5 20a4.5 4.5 0 0 1 9 0"/>"#
        }
        "quote-none" => {
            r#"<path d="M4 4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h1l2 3 2-3h1a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2z"/><circle cx="18" cy="15" r="4.5"/><path d="M15 12l6 6"/>"#
        }
        // Per-visibility glyphs for the composer's visibility selector.
        "vis-public" => {
            r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a15 15 0 0 1 0 18 15 15 0 0 1 0-18z"/>"#
        }
        "vis-unlisted" => {
            r#"<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 7.5-2"/>"#
        }
        "vis-private" => {
            r#"<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>"#
        }
        "vis-direct" => {
            r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3 7l9 6 9-6"/>"#
        }
        "vis-local" => {
            r#"<path d="M4 9l8-5 8 5"/><path d="M6 9v11h12V9"/><path d="M10 20v-5h4v5"/>"#
        }
        // The full-account-archive request/download: a storage box.
        "archive" => {
            r#"<rect x="2" y="4" width="20" height="5" rx="1"/><path d="M4 9v9a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9"/><path d="M10 13h4"/>"#
        }
        // The Private-mentions at-sign (nav entry for /conversations).
        "mention" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M16 8v5a3 3 0 0 0 6 0v-1a10 10 0 1 0-3.92 7.94"/>"#
        }
        // Composer text-format glyphs (P4): plain text is a serif "T",
        // markdown the classic "M↓" mark, HTML angle brackets with a slash.
        "fmt-plain" => r#"<path d="M5 5h14"/><path d="M12 5v14"/><path d="M9 19h6"/>"#,
        "fmt-markdown" => {
            r#"<path d="M3 18V6l4.5 5L12 6v12"/><path d="M18 6v9"/><path d="M15 12.5L18 16l3-3.5"/>"#
        }
        "fmt-html" => {
            r#"<path d="M8 6l-5.5 6L8 18"/><path d="M16 6l5.5 6L16 18"/><path d="M13.2 4l-2.4 16"/>"#
        }
        // A followed-hashtag marker: the classic number sign.
        "hashtag" => {
            r#"<path d="M4 9h16"/><path d="M4 15h16"/><path d="M10 3L8 21"/><path d="M16 3l-2 18"/>"#
        }
        _ => "",
    };
    PreEscaped(format!(
        r#"<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{path}</svg>"#,
    ))
}

/// A single status (or boost) as a timeline/thread card.
pub fn status_card(status: &Status, ctx: &Ctx) -> Markup {
    status_article(status, ctx, false, true)
}

/// A read-only card: the same post representation as the feed (author, content,
/// content warning, media, poll) but without the interaction bar or reactions.
/// Used where a post is shown for reference rather than for interaction — the
/// admin trends review queue.
pub fn status_preview(status: &Status, ctx: &Ctx) -> Markup {
    status_article(status, ctx, false, false)
}

/// A status as the focused post of its thread page: the same card plus
/// Mastodon's detail treatment — full timestamp, client application,
/// visibility / language / quote policy spelled out, and an engagement
/// summary (its counts become links once the lists exist).
pub fn status_detail(status: &Status, ctx: &Ctx) -> Markup {
    status_article(status, ctx, true, true)
}

fn status_article(status: &Status, ctx: &Ctx, detail: bool, interactions: bool) -> Markup {
    // A soft-deleted thread stub renders a dedicated muted tombstone — no
    // author block, no interaction bar, no media affordances — instead of
    // dressing the placeholder entity up as a real card. It keeps the article
    // id (anchor targets, tree indentation) and the author data attribute
    // (in-place moderation dimming), and skips the filter pass: the
    // placeholder content is server-authored, not the author's words.
    if status.deleted() {
        return html! {
            article.status.status--tombstone id=(format!("post-{}", status.id()))
                data-author-id=(status.account().id())
                data-author-domain=[status.account().remote_domain()] {
                p.status__tombstone {
                    (icon("deleted"))
                    span { (ctx.locale.text("status-tombstone")) }
                }
            }
        };
    }
    // A hide-filter match drops the card from the page entirely.
    let verdict = filter_verdict(status, ctx, detail);
    if verdict.hide {
        return html! {};
    }
    // The author/booster data attributes key the in-place moderation JS:
    // muting or blocking an account (or its server) from a status menu dims
    // every visible card it authored or boosted, with no page reload. On a
    // merged card the booster attributes go plural and carry no single
    // account, so muting one of several boosters dims that name instead of a
    // card whose post is legitimately in the feed.
    let boosters = status.boosters();
    if let Some(inner) = status.reblog() {
        html! {
            article.status id=(format!("post-{}", status.id())) data-kind="boost"
                data-author-id=(inner.account().id())
                data-author-domain=[inner.account().remote_domain()]
                data-booster-id=(if boosters.is_empty() {
                    status.account().id().to_owned()
                } else {
                    booster_ids(boosters)
                })
                data-booster-domain=[if boosters.is_empty() {
                    status.account().remote_domain().map(str::to_owned)
                } else {
                    booster_domains_attr(boosters)
                }] {
                // A Group's announce isn't a boost to the reader — it's the
                // post reaching the community's followers.
                @if status.account().group() {
                    p.status__boost {
                        (icon("group"))
                        (ctx.locale.text("status-posted-in")) " "
                        a href=(status.account().profile_path()) { (status.account().name_markup()) }
                    }
                } @else if boosters.is_empty() {
                    p.status__boost {
                        (icon("boost"))
                        a href=(status.account().profile_path()) { (status.account().name_markup()) }
                        " " (ctx.locale.text("status-boosted"))
                    }
                } @else {
                    (booster_line(boosters, false, ctx.locale))
                }
                (status_main(&inner, ctx, detail, &verdict, interactions))
            }
        }
    } else {
        html! {
            article.status id=(format!("post-{}", status.id()))
                data-author-id=(status.account().id())
                data-author-domain=[status.account().remote_domain()] {
                // The post itself won the card over the boosts of it that
                // shared this page, so the boosters ride along as a line
                // rather than as cards. No article-level booster attribute
                // here: this card belongs in the feed on its author's account.
                @if !boosters.is_empty() {
                    (booster_line(boosters, status.also_boosted(), ctx.locale))
                }
                (group_attribution_banner(status, ctx.locale))
                (tag_source_banner(status, ctx.locale))
                (status_main(status, ctx, detail, &verdict, interactions))
            }
        }
    }
}

/// A merged card's booster line: "Alice, Bob and 3 others boosted", or "also
/// boosted by …" when the card is the post itself. The whole sentence is
/// one translatable unit with the linked names interpolated, so a language
/// that puts the verb elsewhere can.
fn booster_line(boosters: &[Value], also: bool, locale: Locale) -> Markup {
    let message = if also {
        "status-also-boosted-by"
    } else {
        "status-boosted-by-several"
    };
    html! {
        p.status__boost {
            (icon("boost"))
            // One span, so several names wrap as a sentence instead of each
            // becoming its own flex item on the line.
            span.status__boosters {
                (locale.markup(message, &[("names", booster_names(boosters, locale))]))
            }
        }
    }
}

/// The linked booster names, capped at [`collapse::split_boosters`]'s cut with
/// "and N others" standing in for the rest. Each name carries the moderation
/// data attributes so muting one booster dims that name alone.
fn booster_names(boosters: &[Value], locale: Locale) -> Markup {
    let (named, others) = collapse::split_boosters(boosters);
    let rest = if others > 0 {
        let mut args = FluentArgs::new();
        args.set("count", i64::try_from(others).unwrap_or(i64::MAX));
        locale.text_with("status-boosters-and-others", &args)
    } else {
        String::new()
    };
    html! {
        @for (i, value) in named.iter().enumerate() {
            @let account = Account(value);
            @if i > 0 { ", " }
            a.status__booster href=(account.profile_path())
                data-booster-id=(account.id())
                data-booster-domain=[account.remote_domain()] {
                (account.name_markup())
            }
        }
        @if !rest.is_empty() { " " (rest) }
    }
}

/// A merged card's plural `data-booster-id`: the boosters' account ids,
/// space-separated (a whitespace-token attribute, matched with `~=`).
fn booster_ids(boosters: &[Value]) -> String {
    let mut out = String::new();
    for value in boosters {
        let account = Account(value);
        let id = account.id();
        if id.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(id);
    }
    out
}

/// [`booster_attr`] for the domains, which are absent on local accounts — so
/// an all-local booster list renders no attribute at all.
fn booster_domains_attr(boosters: &[Value]) -> Option<String> {
    let mut out = String::new();
    for value in boosters {
        let account = Account(value);
        let Some(domain) = account.remote_domain() else {
            continue;
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(domain);
    }
    (!out.is_empty()).then_some(out)
}

/// The "posted in [group]" banner on a bare group post. A Group Announce
/// wrapper renders the equivalent line in [`status_article`] and never reaches
/// this branch; every other surface reads the status entity's `groups`
/// extension, so profile/public/search/thread cards cannot lose the context.
fn group_attribution_banner(status: &Status, locale: Locale) -> Markup {
    let groups = status.group_attribution();
    if groups.is_empty() {
        return html! {};
    }
    html! {
        @for group in &groups {
            p.status__boost {
                (icon("group"))
                (locale.text("status-posted-in")) " "
                a href=(group.profile_path()) { (group.name_markup()) }
            }
        }
    }
}

/// The "In your feed because you follow #tag" banner shown above a home-timeline
/// post that arrived via a followed hashtag. Each named tag links to its
/// local timeline so the reader can jump straight to it. Renders nothing unless
/// `pages::annotate_tag_sources` flagged the entity — so it is home-only and
/// never appears on a post the viewer would see anyway.
fn tag_source_banner(status: &Status, locale: Locale) -> Markup {
    let names = status.tag_source();
    if names.is_empty() {
        return html! {};
    }
    html! {
        p.status__tag-source {
            (icon("hashtag"))
            span {
                (locale.text("status-followed-tag-source")) " "
                @for (i, name) in names.iter().enumerate() {
                    @if i > 0 { ", " }
                    a.status__tag-source-link href=(format!("/tags/{name}")) { "#" (name) }
                }
            }
        }
    }
}

fn status_main(
    status: &Status,
    ctx: &Ctx,
    detail: bool,
    verdict: &FilterVerdict,
    interactions: bool,
) -> Markup {
    let account = status.account();
    // A warn-filter match keeps the author line and action bar but collapses
    // everything the post says behind the "Filtered" bar, like Mastodon.
    let body = status_body(status, ctx, detail, verdict);
    let body = if verdict.warn.is_empty() {
        body
    } else {
        filter_bar(&verdict.warn, &body, ctx.locale)
    };
    html! {
        header.status__head {
            a.status__avatar href=(account.profile_path()) {
                img src=(account.avatar()) alt="" width="48" height="48" loading="lazy";
            }
            div.status__author {
                a.status__name href=(account.profile_path()) { (account.name_markup()) }
                span.status__handle {
                    a.status__acct href=(account.profile_path()) { (account.handle_prefix()) (account.acct()) }
                    @if account.group() {
                        span.status__flag title=(ctx.locale.text("status-account-group")) { (icon("group")) }
                    }
                    @if account.bot() {
                        span.status__flag title=(ctx.locale.text("status-account-bot")) { (icon("bot")) }
                    }
                    @if account.locked() {
                        span.status__flag title=(ctx.locale.text("status-account-locked")) { (icon("vis-private")) }
                    }
                }
            }
            div.status__meta {
                // The compact metadata chips stay off the detail view, where
                // the meta row below the content covers them. The language
                // chip is detail-only: on a closed card it's noise.
                @if !detail {
                    @if let Some(edited) = status.edited_at() {
                        @let mut args = FluentArgs::new();
                        // A tooltip, so it carries the dual reading in full.
                        @let date = ctx.clock.tooltip_iso(edited);
                        @let () = args.set("date", date.as_str());
                        a.status__edited href=(format!("{}/history", status.permalink()))
                            title=(ctx.locale.text_with("status-edited-at", &args)) {
                            (ctx.locale.text("status-edited"))
                        }
                    }
                    @if let Some((glyph, _)) = visibility_meta(status.visibility()) {
                        span.status__vis
                            title=[visibility_label(status.visibility(), ctx.locale)] {
                            (icon(glyph))
                        }
                    }
                }
                // The tooltip belongs on the `<time>`, not the anchor: `app.js`
                // rewrites the element's body to a live relative label, and an
                // inner element with no `title` of its own would shadow the
                // anchor's on hover (G6).
                a.status__time href=(status.permalink_anchored()) {
                    (ctx.clock.element_relative_iso(
                        status.created_at(),
                        &relative_time(status.created_at(), ctx.locale),
                    ))
                }
            }
        }
        (body)
        @if detail {
            (detail_meta(status, ctx))
        }
        // A read-only preview (admin trends review) shows the post but not the
        // interaction bar or reactions.
        @if interactions {
            (action_row(status, ctx))
            (reactions_row(status, ctx))
        }
    }
}

/// Everything a warn filter collapses: the reply context and the post's
/// title, content, quote, poll, media, external link and preview card.
fn status_body(status: &Status, ctx: &Ctx, detail: bool, verdict: &FilterVerdict) -> Markup {
    html! {
        // reply_context renders for a locally-resolved parent *and* for an
        // unfetched one (URI only); it returns nothing when the post isn't a
        // reply, so it needs no guard here.
        (reply_context(status, ctx.locale))
        @if status.spoiler_text().is_empty() {
            (event_box(status, ctx))
            // The title heading and external-link pill are folded into `content`
            // by the entity serializer (so stock Mastodon clients see them too),
            // so the card renders them straight out of the content markup.
            div.status__content { (status.content_markup(ctx.viewer_id.is_some())) }
            (quote_section(status, ctx, detail))
            @if status.poll().is_some() {
                (poll_view(status, ctx))
            }
            (media_section(status, ctx, &verdict.blur))
            (webxdc_invitation_card(status.0.get("webxdc_invitation")))
            (preview_card(status))
        } @else {
            // A content warning is a single gate over the whole post: its text,
            // poll and media all live inside one toggle, so a sensitive post
            // never shows a second, separate media spoiler.
            details.status__cw open[ctx.prefs.expand_spoilers] {
                summary { (status.spoiler_markup()) }
                (event_box(status, ctx))
                // Title/link folded into `content` (see the non-CW branch).
                div.status__content { (status.content_markup(ctx.viewer_id.is_some())) }
                @if status.poll().is_some() {
                    (poll_view(status, ctx))
                }
                (blur_gated_gallery(status, ctx, &verdict.blur))
                (webxdc_invitation_card(status.0.get("webxdc_invitation")))
                (preview_card(status))
            }
            (quote_section(status, ctx, detail))
        }
        (translation_note(status, ctx))
    }
}

/// The Translate control / translation attribution under a post's body.
/// A translated entity (the thread page's `?translate=1` render) shows its
/// attribution with a Show-original link. Any other eligible post gets the
/// Translate control: a POST form whose no-JS submit redirects to the
/// translated permalink, and whose JS enhancement (`bindTranslateForms`)
/// swaps the translation in place.
fn translation_note(status: &Status, ctx: &Ctx) -> Markup {
    if let Some(translation) = status.translation() {
        let provider = translation
            .get("provider")
            .and_then(Value::as_str)
            .map_or_else(
                || ctx.locale.text("status-translation-service"),
                str::to_owned,
            );
        let source = translation
            .get("detected_source_language")
            .and_then(Value::as_str)
            .map(language_label);
        let mut args = FluentArgs::new();
        args.set("provider", provider.as_str());
        if let Some(language) = source.as_deref() {
            args.set("language", language);
        }
        return html! {
            p.status__translation {
                (ctx.locale.text_with(
                    if source.is_some() { "status-translated-from" } else { "status-translated" },
                    &args))
                " · "
                a href=(status.permalink_anchored()) { (ctx.locale.text("status-show-original")) }
            }
        };
    }
    let (Some(csrf), Some(target), Some(languages)) = (
        ctx.csrf,
        ctx.prefs.translate_to.as_deref(),
        ctx.prefs.translate_languages.as_ref(),
    ) else {
        return html! {};
    };
    // `content` is the folded form, so a title-only Page still counts as
    // having text. The backend-support check keeps the control off posts in
    // languages the backend can't translate (a confident-but-wrong
    // translation is worse than none).
    let eligible = matches!(status.visibility(), "public" | "unlisted")
        && !status.content_html().is_empty()
        && status
            .language()
            .is_none_or(|lang| primary_language(lang) != primary_language(target))
        && crate::translation::permitted_target(languages, status.language().unwrap_or(""), target)
            .is_some();
    if !eligible {
        return html! {};
    }
    html! {
        // A div, not a p: a <form> may not sit inside a paragraph, and the
        // browser parser would hoist it out — detaching it from the
        // container the JS enhancement anchors its state to.
        div.status__translation {
            form.translate-form method="post"
                action=(format!("/web/statuses/{}/translate", status.id()))
                data-translate
                data-i18n-translating=(ctx.locale.text("status-translating"))
                data-i18n-failed=(ctx.locale.text("status-translation-failed"))
                data-i18n-network-failed=(ctx.locale.text("status-translation-network-failed"))
                data-i18n-translated=(ctx.locale.text("status-translated-fallback"))
                data-i18n-show-original=(ctx.locale.text("status-show-original")) {
                input type="hidden" name="csrf" value=(csrf);
                button.status__translation-link type="submit" {
                    (ctx.locale.text("status-translate"))
                }
            }
        }
    }
}

/// The primary language subtag (`pt-BR` → `pt`), for deciding whether a post
/// is already in the viewer's target language.
fn primary_language(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// A language code's display label ("Deutsch (German)"), falling back to the
/// raw code outside the inventory.
pub fn language_label(code: &str) -> String {
    languages::find(code).map_or_else(|| code.to_owned(), Language::label)
}

/// The venue line of an event: the `Place` name followed by whatever of the
/// structured `PostalAddress` the origin sent, in reading order.
///
/// Assembled rather than rendered field-by-field because the components are
/// individually meaningless — a bare "Osrednjeslovenska" on its own line is
/// noise, while "Community Hall, Trg 1, Ljubljana, 1000, Slovenia" is an
/// address someone can act on. Any subset may be missing, so the join has to
/// tolerate holes rather than assume a fixed shape.
fn event_venue(event: &Value) -> Option<String> {
    let part = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let parts: Vec<&str> = [
        "location",
        "location_street",
        "location_locality",
        "location_region",
        "location_postal_code",
        "location_country",
    ]
    .iter()
    .filter_map(|key| part(key))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The typed event facts (`Event` posts, E1 the rest): when, where, how to
/// attend, and whether it still happens. Dates render in the event's own
/// timezone only as text — the instants are shown as absolute UTC-offset times
/// like the rest of the UI.
///
/// Every fact is optional, and an absent one is simply not shown: a lean
/// dialect that sends `name` + `startTime` must render as a clean two-line box,
/// not as a grid of "unknown"s.
#[allow(
    clippy::too_many_lines,
    reason = "one linear list of independent optional facts; splitting scatters the box"
)]
fn event_box(status: &Status, ctx: &Ctx) -> Markup {
    let (clock, locale) = (&ctx.clock, ctx.locale);
    let Some(event) = status.event() else {
        return html! {};
    };
    let field = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let count = |key: &str| event.get(key).and_then(Value::as_i64);
    let cancelled = field("status") == Some("CANCELLED");
    let tentative = field("status") == Some("TENTATIVE");
    let online = event.get("is_online").and_then(Value::as_bool) == Some(true);
    let venue = event_venue(event);
    let capacity = count("max_attendees");
    let attending = count("participants_count");
    html! {
        div.status__event {
            (icon("calendar"))
            div.status__event-facts {
                @if let Some(start) = field("start_time") {
                    span {
                        (clock.element_absolute_iso(start))
                        @if let Some(end) = field("end_time") {
                            " – " (clock.element_absolute_iso(end))
                        }
                    }
                }
                // The event's own zone is the *venue's*, and the readings above
                // are the *viewer's* — so it gets its own labelled line. Left
                // as a bare parenthetical it reads as if it qualified the times
                // beside it, which is exactly backwards.
                @if let Some(venue_zone) = field("timezone") {
                    @let mut args = FluentArgs::new();
                    @let () = args.set("zone", venue_zone);
                    span.status__event-zone {
                        (locale.text_with("status-event-venue-zone", &args))
                    }
                }
                @if online {
                    span { (locale.text("status-event-online")) }
                }
                @if let Some(venue) = venue {
                    // The Place's own id is a real page on the origin (a venue
                    // several events share), so the address links out when we
                    // have one and stays plain text when we don't.
                    @if let Some(url) = field("location_url") {
                        span {
                            a href=(url) target="_blank" rel="noopener noreferrer" {
                                (venue)
                            }
                        }
                    } @else {
                        span { (venue) }
                    }
                }
                // Attendance: whichever of the count/capacity pair the origin
                // sent. "12 attending", "12 of 40 attending", or "40 places" —
                // never a bare "of 40", which reads as a missing number.
                @if attending.is_some() || capacity.is_some() {
                    @let mut args = FluentArgs::new();
                    @let () = args.set("count", attending.unwrap_or_default());
                    @let () = args.set("capacity", capacity.unwrap_or_default());
                    span.status__event-attendance {
                        @match (attending, capacity) {
                            (Some(_), Some(_)) => {
                                (locale.text_with("status-event-attending-of", &args))
                            }
                            (Some(_), None) => {
                                (locale.text_with("status-event-attending", &args))
                            }
                            _ => (locale.text_with("status-event-capacity", &args)),
                        }
                    }
                }
                // A category is an open vocabulary from the origin (Mobilizon's
                // `MEETING`, `SPORTS`, …) — shown, never acted on.
                @if let Some(category) = field("category") {
                    span.status__event-category { (category) }
                }
                @if cancelled {
                    span.status__event-cancelled { (locale.text("status-event-cancelled")) }
                } @else if tentative {
                    span { (locale.text("status-event-tentative")) }
                }
                (rsvp_cluster(status, event, ctx))
            }
        }
    }
}

/// The RSVP cluster (E2): what the viewer can do about attending.
///
/// Deliberately not an `action_form` toggle. An RSVP is a negotiation, so the
/// button has more than two states and most of them are *statements* rather than
/// controls:
///
/// * no viewer — nothing at all (a control that only leads to a login prompt is
///   noise on every event on the timeline);
/// * `external` — a link out to the origin's own ticketing, since there is no
///   activity we could send;
/// * `pending` — "awaiting approval" plus a way to withdraw. It may stay here
///   forever: a `restricted` event waits on a human, and a full one gets no reply
///   at all. So this reads as a state, never as a spinner;
/// * `accepted` — "going", with a cancel;
/// * `rejected` / `invite`-only / `full` / cancelled — a plain sentence and no
///   button, because each sends the viewer somewhere different and a single
///   greyed-out control would say none of it.
pub(crate) fn rsvp_cluster(status: &Status, event: &Value, ctx: &Ctx) -> Markup {
    let Some(csrf) = ctx.csrf else {
        return html! {};
    };
    let text = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let state = text("participation");
    let can = event.get("can_participate").and_then(Value::as_bool) == Some(true);
    let refusal = text("participation_refusal");
    let post = |verb: &str, label: &str| -> Markup {
        let path = format!("/web/statuses/{}/{verb}", status.id());
        html! {
            form.action-form method="post" action=(path) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value=(ctx.return_to);
                button.action.action__btn type="submit" { (ctx.locale.text(label)) }
            }
        }
    };
    let base = format!("/web/statuses/{}", status.id());
    html! {
        div.status__event-rsvp data-rsvp=(base)
            data-rsvp-failed=(ctx.locale.text("status-event-rsvp-failed"))
            data-rsvp-network-failed=(ctx.locale.text("status-event-rsvp-network-failed")) {
            @match state {
                Some("accepted") => {
                    span.status__event-going { (ctx.locale.text("status-event-going")) }
                    (post("unparticipate", "status-event-cancel-rsvp"))
                }
                Some("pending") => {
                    span.status__event-pending { (ctx.locale.text("status-event-pending")) }
                    (post("unparticipate", "status-event-withdraw-rsvp"))
                }
                Some("rejected") => {
                    span.status__event-refused { (ctx.locale.text("status-event-refused")) }
                }
                // An invitation we haven't acted on: the button is the point.
                Some("invited") => {
                    span.status__event-invited { (ctx.locale.text("status-event-invited")) }
                    (post("participate", "status-event-rsvp"))
                }
                _ => {
                    @if refusal == Some("external") {
                        @if let Some(url) = text("external_participation_url") {
                            a.action.action__btn href=(url)
                                target="_blank" rel="noopener noreferrer" {
                                (ctx.locale.text("status-event-rsvp-external"))
                            }
                        }
                    } @else if let Some(reason) = refusal {
                        span.status__event-closed {
                            (ctx.locale.text(match reason {
                                "invite_only" => "status-event-invite-only",
                                "full" => "status-event-full",
                                "cancelled" => "status-event-cancelled",
                                _ => "status-event-closed",
                            }))
                        }
                    } @else if can {
                        (post("participate", "status-event-rsvp"))
                    }
                }
            }
        }
    }
}

/// The organizer's attendee panel for an event we host (E3).
///
/// Pending requests come first and carry the approve/reject buttons: they are the
/// only rows that need a decision, and burying them under a long "going" list is
/// how an approval queue silently stops being worked. A request's message to the
/// organizer is shown beside it — it is usually the whole basis for the decision.
pub fn event_attendees(
    status_id: i64,
    entries: &[(plamenu_db::status_participation::Participation, Value)],
    cancelled: bool,
    csrf: &str,
    locale: Locale,
) -> Markup {
    use plamenu_db::status_participation::State;
    let mut ordered: Vec<&(plamenu_db::status_participation::Participation, Value)> =
        entries.iter().collect();
    ordered.sort_by_key(|(row, _)| match row.state {
        State::Pending => 0,
        State::Accepted => 1,
        State::Invited => 2,
        State::Rejected => 3,
    });
    let going = entries
        .iter()
        .filter(|(row, _)| row.state.is_attending())
        .count();
    let mut count_args = FluentArgs::new();
    count_args.set("count", going);
    html! {
        section.event-attendees {
            h2 { (locale.text("event-attendees-heading")) }
            // Cancelling lives with the attendee list, beside the people it
            // affects, rather than in the edit form full of address fields.
            @if !cancelled {
                form.action-form.event-attendees__cancel method="post"
                    action=(format!("/web/statuses/{status_id}/cancel-event")) {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="return_to"
                        value=(format!("/web/statuses/{status_id}"));
                    button.action.action__btn.is-danger type="submit" {
                        (locale.text("event-cancel"))
                    }
                    span.compose__hint { (locale.text("event-cancel-hint")) }
                }
            } @else {
                p.status__event-cancelled { (locale.text("status-event-cancelled")) }
            }
            p.event-attendees__summary {
                (locale.text_with("status-event-attending", &count_args))
            }
            ul.event-attendees__list {
                @for (row, account) in ordered {
                    li.event-attendees__row {
                        // The canonical account row: it uses `profile_path()`, so a
                        // remote GROUP attendee links to `/!name@host` rather than
                        // `/@name@host`, and `name_markup()` renders custom emoji in
                        // a display name instead of leaving `:shortcode:` visible.
                        (account_card(&Account(account)))
                        span.event-attendees__state {
                            (locale.text(match row.state {
                                State::Pending => "event-attendee-pending",
                                State::Accepted => "event-attendee-going",
                                State::Rejected => "event-attendee-rejected",
                                State::Invited => "event-attendee-invited",
                            }))
                        }
                        @if let Some(message) = row.message.as_deref() {
                            p.event-attendees__message { (message) }
                        }
                        @if row.state == State::Pending {
                            span.event-attendees__actions {
                                (attendee_verdict_form(
                                    status_id, row.account_id, true, csrf, locale))
                                (attendee_verdict_form(
                                    status_id, row.account_id, false, csrf, locale))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One approve/reject button in the attendee panel.
fn attendee_verdict_form(
    status_id: i64,
    account_id: i64,
    approve: bool,
    csrf: &str,
    locale: Locale,
) -> Markup {
    let verb = if approve { "approve" } else { "reject" };
    let path = format!("/web/statuses/{status_id}/participants/{account_id}/{verb}");
    html! {
        form.action-form method="post" action=(path) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(format!("/web/statuses/{status_id}"));
            button.action.action__btn type="submit" {
                (locale.text(if approve {
                    "event-attendee-approve"
                } else {
                    "event-attendee-reject"
                }))
            }
        }
    }
}

/// The "Replying to …" context line: names the reply target when it can be
/// read off the entity, and always links into the parent thread through the
/// `/web/statuses/{id}` resolver (the parent's handle isn't in this entity).
fn reply_context(status: &Status, locale: Locale) -> Markup {
    if let Some(parent) = status.in_reply_to_id() {
        // The parent is not on this page, so the card carries a look at it
        // who *and* what, in the slot that otherwise says only who.
        if let Some(peek) = status.reply_peek() {
            return reply_peek(parent, peek, locale);
        }
        let account = status.reply_to_acct();
        let mut args = FluentArgs::new();
        if let Some(account) = account {
            args.set("account", account);
        }
        return html! {
            p.status__reply-to {
                (icon("reply"))
                a href=(format!("/web/statuses/{parent}")) {
                    (locale.text_with(
                        if account.is_some() { "status-replying-to" } else { "status-replying-earlier" },
                        &args))
                }
            }
        };
    }
    // Parent never fetched (e.g. its host black-holed our pull): we still know
    // its AP URI, so show the post as a reply and link out to the original
    // rather than rendering it as a standalone post.
    if let Some(uri) = status.in_reply_to_uri() {
        let mut args = FluentArgs::new();
        args.set("host", uri_host(uri));
        return html! {
            p.status__reply-to.status__reply-to--unfetched {
                (icon("reply"))
                a href=(uri) target="_blank" rel="noopener noreferrer"
                    title=(locale.text("status-parent-unavailable-title")) {
                    (locale.text_with("status-replying-host", &args))
                }
            }
        };
    }
    html! {}
}

/// The reply-context line for a parent that is off the page: the parent's
/// author and the first line of what they said, as one link into the thread.
/// Deliberately not a card — it is the same slot the "Replying to …"
/// line occupies, carrying more of the answer to "replying to *what*".
///
/// The excerpt is plain text extracted from the parent's already-sanitised
/// content, so it goes through the plain-text emoji renderer (which escapes it
/// before swapping known shortcodes for images).
fn reply_peek(parent: &str, peek: &Value, locale: Locale) -> Markup {
    let field = |key: &str| peek.get(key).and_then(Value::as_str).unwrap_or_default();
    let emojis: &[Value] = peek
        .get("emojis")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let mut args = FluentArgs::new();
    // The handle, not the display name: the tooltip is the same sentence the
    // plain reply line shows, and that one names the account.
    args.set("account", format!("@{}", field("acct")));
    html! {
        a.status__reply-peek href=(format!("/web/statuses/{parent}"))
            title=(locale.text_with("status-replying-to", &args)) {
            (icon("reply"))
            @if !field("avatar").is_empty() {
                img.status__reply-peek-avatar src=(field("avatar")) alt="" loading="lazy";
            }
            span.status__reply-peek-author { (emojify_text(field("name"), emojis)) }
            span.status__reply-peek-handle { "@" (field("acct")) }
            span.status__reply-peek-text { (emojify_text(field("excerpt"), emojis)) }
        }
    }
}

/// The bare host of an absolute URI (`https://aussie.zone/comment/1` →
/// `aussie.zone`), for human-readable "on {host}" labels. Falls back to the
/// whole string if it isn't a scheme-prefixed URL.
fn uri_host(uri: &str) -> &str {
    uri.split_once("://")
        .map_or(uri, |(_, rest)| rest.split('/').next().unwrap_or(rest))
}

/// A post-shaped placeholder standing in for a reply's parent we could not
/// fetch. Rendered above the focus in the thread view (where there are no
/// ancestors to show) so the reader sees the post is a reply and can open the
/// original on its home server.
pub fn unfetched_parent_card(uri: &str, locale: Locale) -> Markup {
    let mut args = FluentArgs::new();
    args.set("host", uri_host(uri));
    html! {
        article.status.status--placeholder.unfetched-parent {
            p.unfetched-parent__notice {
                (icon("reply"))
                span { (locale.text_with("status-parent-unavailable", &args)) }
            }
            a.unfetched-parent__link href=(uri) target="_blank" rel="noopener noreferrer" {
                (icon("link")) (locale.text("status-open-original-post"))
            }
        }
    }
}

/// The detail view's metadata row: full timestamp, edit time, compact
/// visibility / language / quote-policy chips (labels in their titles), and
/// the client application. The engagement counts live on the action bar and
/// their lists behind the overflow menu, so no stats line here.
fn detail_meta(status: &Status, ctx: &Ctx) -> Markup {
    html! {
        div.status__detail-meta {
            span { (ctx.clock.element_absolute_iso(status.created_at())) }
            @if let Some(edited) = status.edited_at() {
                @let mut args = FluentArgs::new();
                @let date = ctx.clock.stamp_iso(edited);
                @let () = args.set("date", date.as_str());
                a.status__detail-edited href=(format!("{}/history", status.permalink())) {
                    (ctx.locale.text_with("status-edited-at", &args))
                }
            }
            @if let Some((glyph, _)) = visibility_meta(status.visibility()) {
                span.status__vis.status__detail-icon data-detail-icon=(glyph)
                    title=[visibility_label(status.visibility(), ctx.locale)] {
                    (icon(glyph))
                }
            }
            @if let Some(code) = status.language() {
                span.status__lang
                    title=(languages::find(code).map_or_else(|| code.to_owned(), Language::label)) {
                    (code)
                }
            }
            @if let Some((glyph, policy)) = status.quote_policy_meta() {
                span.status__vis.status__detail-icon data-detail-icon=(glyph)
                    title=(policy) { (icon(glyph)) }
            }
            // The source object's kind, when it wasn't a plain post:
            // an Article from a blog, a Video from PeerTube, an Event.
            @if let Some(kind) = status.object_type() {
                span { (kind) }
            }
            @if let Some(app) = status.application_name() {
                span { (app) }
            }
        }
    }
}

/// The quote attachment in all its states: an embedded card when the quoted
/// status is present, a link through the status resolver for the shallow
/// (nesting-capped) shape, and an explanatory placeholder for the pending /
/// deleted / revoked states, which previously rendered nothing at all.
fn quote_section(status: &Status, ctx: &Ctx, detail: bool) -> Markup {
    let Some(state) = status.quote_state() else {
        return html! {};
    };
    if let Some(quoted) = status.quote() {
        // The quoted post is filtered on its own `filtered` results, like
        // Mastodon's QuotedStatus: hide swaps in a placeholder, warn
        // collapses the embedded card behind the bar.
        let verdict = filter_verdict(&quoted, ctx, detail);
        if verdict.hide {
            return html! {
                div.quote-card.quote-card--placeholder {
                    (ctx.locale.text("status-filtered-hidden"))
                }
            };
        }
        if !verdict.warn.is_empty() {
            return filter_bar(
                &verdict.warn,
                &quote_card(&quoted, ctx, &verdict.blur),
                ctx.locale,
            );
        }
        return quote_card(&quoted, ctx, &verdict.blur);
    }
    if state == "accepted"
        && let Some(id) = status.quoted_status_id()
    {
        return html! {
            div.quote-card.quote-card--placeholder {
                a href=(format!("/web/statuses/{id}")) {
                    (ctx.locale.text("status-view-quoted"))
                }
            }
        };
    }
    let message = match state {
        "pending" => "status-quote-pending",
        "deleted" => "status-quote-deleted",
        "revoked" | "rejected" => "status-quote-removed",
        _ => "status-quote-unavailable",
    };
    html! {
        div.quote-card.quote-card--placeholder { (ctx.locale.text(message)) }
    }
}

/// A compact card for a quoted status, embedded inside the quoting one. Its
/// media is part of the quotation's context, and follows the same sensitive /
/// filtered-media preferences as a top-level status.
fn quote_card(quoted: &Status, ctx: &Ctx, blur: &[String]) -> Markup {
    cited_status_card_with(
        quoted,
        ctx.viewer_id.is_some(),
        &media_section(quoted, ctx, blur),
    )
}

/// The compact embedded-status card itself — the quote attachment and the
/// announcement's cited statuses (`status_ids`) share this rendering.
pub(crate) fn cited_status_card(status: &Status, signed_in: bool) -> Markup {
    cited_status_card_with(status, signed_in, &html! {})
}

/// Shared cited-status shell. Quote cards pass their media as the tail while
/// announcement citations retain their existing text-only compact form.
fn cited_status_card_with(status: &Status, signed_in: bool, tail: &Markup) -> Markup {
    let account = status.account();
    html! {
        div.quote-card {
            a.quote-card__head href=(status.permalink_anchored()) {
                img.quote-card__avatar src=(account.avatar()) alt="" width="20" height="20" loading="lazy";
                span.quote-card__name { (account.name_markup()) }
                span.quote-card__acct { (account.handle_prefix()) (account.acct()) }
            }
            div.status__content { (status.content_markup(signed_in)) }
            (tail)
        }
    }
}

/// The link preview card: the entity's `card` — built and API-shipped
/// since the link-preview milestone — as Mastodon's compact layout under the
/// content: thumbnail, provider, title, description, the whole card one
/// external link. The entity builder only attaches cards to statuses without
/// media or quotes, so no gating is needed here.
fn preview_card(status: &Status) -> Markup {
    if let Some(card) = status.card() {
        if status
            .0
            .get("webxdc_invitation")
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
            .is_some_and(|url| card.get("url").and_then(Value::as_str) == Some(url))
        {
            return html! {};
        }
        link_card(card)
    } else {
        html! {}
    }
}

/// One `PreviewCard` entity as a link card — the status preview card and the
/// Explore News tab share this rendering.
pub fn link_card(card: &Value) -> Markup {
    let field = |key: &str| card.get(key).and_then(Value::as_str).unwrap_or_default();
    let url = field("url");
    if url.is_empty() {
        return html! {};
    }
    let image = Some(field("image")).filter(|s| !s.is_empty());
    let title = Some(field("title"))
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    let provider = Some(field("provider_name"))
        .filter(|s| !s.is_empty())
        .unwrap_or(host);
    // Video and photo cards get Mastodon's large layout: the thumbnail full
    // width on top instead of a side strip.
    let large = image.is_some() && matches!(field("type"), "video" | "photo");
    html! {
        a.preview-card.preview-card--large[large] href=(url) target="_blank" rel="nofollow noopener noreferrer" {
            @if let Some(image) = image {
                img.preview-card__image src=(image) alt=(field("image_description")) loading="lazy";
            }
            span.preview-card__body {
                @if !provider.is_empty() {
                    span.preview-card__provider { (provider) }
                }
                span.preview-card__title { (title) }
                @if !field("description").is_empty() {
                    span.preview-card__desc { (field("description")) }
                }
                @if !field("author_name").is_empty() {
                    span.preview-card__author { "by " (field("author_name")) }
                }
            }
        }
    }
}

/// An attachment's preview dimensions, preferring the `small` (thumbnail) style
/// and falling back to the `original` — used to size the `<img>`/`<video>` so
/// the layout doesn't shift when the media loads.
fn preview_dimensions(item: &Value) -> (Option<i64>, Option<i64>) {
    let meta = item.get("meta");
    let style = |key: &str| meta.and_then(|m| m.get(key));
    let dim = |key: &str| {
        style("small")
            .and_then(|s| s.get(key))
            .or_else(|| style("original").and_then(|s| s.get(key)))
            .and_then(Value::as_i64)
    };
    (dim("width"), dim("height"))
}

/// The media gallery, gated behind a standalone toggle when a blur filter
/// matched or when the viewer's reading preference (or the post's sensitive
/// flag) calls for it. Used only where there is no content warning — a CW
/// already covers the media itself.
fn media_section(status: &Status, ctx: &Ctx, blur: &[String]) -> Markup {
    if status.media().is_empty() {
        return html! {};
    }
    if !blur.is_empty() {
        return blur_gated_gallery(status, ctx, blur);
    }
    let gallery = media_gallery(status, ctx);
    let gated = match ctx.prefs.expand_media {
        MediaDisplay::ShowAll => false,
        MediaDisplay::HideAll => true,
        MediaDisplay::Default => status.sensitive(),
    };
    if gated {
        html! {
            details.status__media-sensitive {
                summary { (ctx.locale.text("status-sensitive-content")) }
                (gallery)
            }
        }
    } else {
        gallery
    }
}

/// The media gallery behind a blur-filter gate ("Filtered: {names}"), the
/// no-JS equivalent of Mastodon blurring only the media. Falls through to the
/// bare gallery when no blur filter matched.
fn blur_gated_gallery(status: &Status, ctx: &Ctx, blur: &[String]) -> Markup {
    if status.media().is_empty() {
        return html! {};
    }
    if blur.is_empty() {
        return media_gallery(status, ctx);
    }
    let mut args = FluentArgs::new();
    args.set("filters", blur.join(", "));
    html! {
        details.status__media-sensitive {
            summary { (ctx.locale.text_with("status-filtered", &args)) }
            (media_gallery(status, ctx))
        }
    }
}

fn media_gallery(status: &Status, ctx: &Ctx) -> Markup {
    standalone_media(status.media(), ctx)
}

/// A media gallery that is not owned by a status card. `Owncast` uses this for
/// the account-level player of an already-running stream discovered by its
/// homepage; all player/HLS/live rendering stays identical to timeline media.
pub(crate) fn standalone_media(media: &[Value], ctx: &Ctx) -> Markup {
    if media.is_empty() {
        return html! {};
    }
    let count = media.len().min(4);
    html! {
        div.status__media data-count=(count) {
            @for item in media {
                @let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                // The preview/thumbnail keeps the timeline light; the full file
                // is only loaded when the viewer opens it. Falls back to the
                // full URL when no preview was generated.
                @let preview = item
                    .get("preview_url")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url);
                @let alt = item.get("description").and_then(Value::as_str).unwrap_or_default();
                @let blurhash = item
                    .get("blurhash")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                // Emit intrinsic dimensions so the browser reserves the box
                // before the image loads (avoids layout shift / CLS).
                @let (width, height) = preview_dimensions(item);
                @match item.get("type").and_then(Value::as_str) {
                    Some("image") => figure.media {
                        // The new-tab link is the no-JS behavior; JS upgrades
                        // the click into the in-page lightbox.
                        a.media__link href=(url) target="_blank" rel="noopener noreferrer" {
                            img src=(preview) alt=(alt) loading="lazy" width=[width] height=[height]
                                data-blurhash=[blurhash];
                        }
                        (media_badges(alt, false))
                    },
                    // A gifv is a soundless clip standing in for a GIF. With
                    // the viewer's auto-play preference on it loops as a
                    // muted autoplay with no player chrome; off (the default)
                    // it renders as a still poster linking to the clip, which
                    // JS upgrades into the lightbox like an image.
                    Some("gifv") => figure.media {
                        @if ctx.prefs.autoplay_gifs {
                            video.media__gifv src=(url) poster=(preview) autoplay muted loop playsinline
                                width=[width] height=[height] title=[Some(alt).filter(|a| !a.is_empty())] {}
                        } @else {
                            a.media__link data-gifv href=(url) target="_blank" rel="noopener noreferrer" {
                                img src=(preview) alt=(alt) loading="lazy" width=[width] height=[height]
                                    data-blurhash=[blurhash];
                            }
                        }
                        (media_badges(alt, true))
                    },
                    Some("video") => {
                        // HLS-native video (PeerTube): `data-hls` is the caching
                        // proxy's master playlist; app.js takes over with hls.js
                        // (quality selector, separated-audio sound) or native
                        // HLS. The stable sparse MP4 remains the real baseline
                        // `src`, so playback also works when an extension or CSP
                        // blocks scripts while the HTML parser still treats
                        // <noscript> as inert. app.js removes this source as soon
                        // as it binds, retaining it only as the final fallback.
                        // Plain (non-HLS) video keeps its `src`: it has no
                        // `data-hls`, so app.js never binds it and `src` is its
                        // only playback path.
                        @let hls_master = item
                            .get("hls")
                            .and_then(|h| h.get("master"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());
                        // A live broadcast is playable only while it is on air.
                        // Off air there is no `url` and no playlist, so the tile
                        // shows the poster and says which of the two it is —
                        // never a player that would fail on the first click.
                        @let live_state = item
                            .get("live")
                            .and_then(|l| l.get("state"))
                            .and_then(Value::as_str);
                        @match live_state {
                            Some("waiting" | "ended") => (offline_live(item, preview, alt, width, height, ctx)),
                            _ => figure.media.media--video {
                                @if let Some(master) = hls_master {
                                    video src=(url) poster=(preview) controls playsinline preload="none"
                                        data-hls=(master) data-src=(url)
                                        data-live=[live_state]
                                        width=[width] height=[height]
                                        title=[Some(alt).filter(|a| !a.is_empty())] {}
                                } @else {
                                    video src=(url) poster=(preview) controls playsinline preload="none"
                                        width=[width] height=[height]
                                        title=[Some(alt).filter(|a| !a.is_empty())] {}
                                }
                                @if live_state == Some("live") {
                                    div.media__badges {
                                        span.media__badge.media__badge--live { (ctx.locale.text("status-live-badge")) }
                                        @if !alt.is_empty() {
                                            span.media__badge title=(alt) { "ALT" }
                                        }
                                    }
                                } @else {
                                    (media_badges(alt, false))
                                }
                            },
                        }
                    },
                    Some("audio") => figure.media.media--audio {
                        audio src=(url) controls preload="metadata"
                            title=[Some(alt).filter(|a| !a.is_empty())] {}
                    },
                    _ => figure.media {
                        a href=(url) target="_blank" rel="noopener noreferrer" {
                            (ctx.locale.text("status-attachment"))
                        }
                    },
                }
            }
        }
    }
}

/// A live broadcast that is not on air: the poster with a caption saying why
/// there is nothing to play.
///
/// Rendered instead of a `<video>` rather than alongside one, because a player
/// with no source is a promise the page cannot keep — the viewer clicks it and
/// nothing happens. A permanent live phrases its wait differently: it is
/// between broadcasts, not yet to have its first.
fn offline_live(
    item: &Value,
    preview: &str,
    alt: &str,
    width: Option<i64>,
    height: Option<i64>,
    ctx: &Ctx,
) -> Markup {
    let live = item.get("live");
    let state = live
        .and_then(|l| l.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("waiting");
    let permanent = live
        .and_then(|l| l.get("permanent"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let caption = match state {
        "ended" if permanent => "status-live-waiting-recurring",
        "ended" => "status-live-ended",
        _ if permanent => "status-live-waiting-recurring",
        _ => "status-live-waiting",
    };
    html! {
        figure.media.media--video.media--live-offline {
            img src=(preview) alt=(alt) loading="lazy" width=[width] height=[height];
            figcaption.media__live-state { (ctx.locale.text(caption)) }
            (media_badges(alt, false))
        }
    }
}

/// The badge row overlaid on a media tile: "GIF" for gifv clips and "ALT"
/// (with the description as its tooltip) when the attachment is described.
fn media_badges(alt: &str, gif: bool) -> Markup {
    if alt.is_empty() && !gif {
        return html! {};
    }
    html! {
        div.media__badges {
            @if gif {
                span.media__badge { "GIF" }
            }
            @if !alt.is_empty() {
                span.media__badge title=(alt) { "ALT" }
            }
        }
    }
}

/// The poll: a results view once the viewer has voted (or it has closed), an
/// otherwise live vote form for signed-in viewers.
pub(crate) fn poll_view(status: &Status, ctx: &Ctx) -> Markup {
    let Some(poll) = status.poll() else {
        return html! {};
    };
    let options = poll.get("options").and_then(Value::as_array);
    let Some(options) = options else {
        return html! {};
    };
    let multiple = poll
        .get("multiple")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expired = poll
        .get("expired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let voted = poll.get("voted").and_then(Value::as_bool).unwrap_or(false);
    let total = poll.get("votes_count").and_then(Value::as_i64).unwrap_or(0);
    let own: Vec<i64> = poll
        .get("own_votes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    // The form is live only for a signed-in viewer who hasn't voted on a
    // poll that is still open and isn't their own.
    let can_vote = ctx.csrf.is_some() && !voted && !expired && !ctx.owns(status);
    let mut vote_args = FluentArgs::new();
    vote_args.set("count", total);
    let footer = html! {
        p.poll__footer {
            (ctx.locale.text_with("status-poll-votes", &vote_args))
            @if expired { " · " (ctx.locale.text("status-poll-closed")) }
        }
    };

    let base = format!("/web/statuses/{}", status.id());
    html! {
        div.poll-region data-poll=(base)
            data-poll-failed=(ctx.locale.text("status-poll-vote-failed"))
            data-poll-network-failed=(ctx.locale.text("status-poll-vote-network-failed")) {
            @if can_vote {
                @let input = if multiple { "checkbox" } else { "radio" };
                form.poll method="post" action=(format!("/web/statuses/{}/vote", status.id())) {
                    input type="hidden" name="csrf" value=(ctx.csrf.unwrap_or_default());
                    input type="hidden" name="return_to" value=(ctx.return_to);
                    @for (index, option) in options.iter().enumerate() {
                        label.poll__choice {
                            input type=(input) name="choices[]" value=(index);
                            span { (emojify_text(option.get("title").and_then(Value::as_str).unwrap_or_default(), status.emojis())) }
                        }
                    }
                    button.poll__vote type="submit" { (ctx.locale.text("status-poll-vote")) }
                }
            } @else {
                div.poll {
                    @for (index, option) in options.iter().enumerate() {
                        @let tally = option.get("votes_count").and_then(Value::as_i64).unwrap_or(0);
                        @let pct = if total > 0 { tally * 100 / total } else { 0 };
                        @let mine = own.contains(&i64::try_from(index).unwrap_or(-1));
                        div class=(if mine { "poll__result is-own" } else { "poll__result" }) {
                            div.poll__bar style=(format!("--pct:{pct}%")) {}
                            span.poll__title { (emojify_text(option.get("title").and_then(Value::as_str).unwrap_or_default(), status.emojis())) }
                            span.poll__pct { (pct) "%" }
                        }
                    }
                }
            }
            (footer)
        }
    }
}

/// The action bar: one row in Mastodon's order — reply / boost /
/// quote / favourite / react / bookmark / shield / "…" — every counter attached
/// to its icon, secondary verbs behind the trailing overflow menu. The shield
/// appears only when the viewer has a target-relevant privileged action. The
/// react link opens the full server-rendered picker without JavaScript; the
/// enhancement turns it into an on-card popup. Existing reaction chips render
/// below in `reactions_row`.
fn action_row(status: &Status, ctx: &Ctx) -> Markup {
    html! {
        footer.status__actions {
            a.action href=(status.permalink_anchored())
                title=(ctx.locale.text("status-action-reply")) {
                (icon("reply")) span.action__count { (status.replies_count()) }
            }
            @match ctx.csrf {
                Some(token) => {
                    @if status.boostable() {
                        (action_form(status, "reblog", status.reblogged(),
                            "boost", Some(status.reblogs_count()), token, ctx.return_to, ctx.locale))
                    } @else {
                        span.action.is-disabled
                            title=(ctx.locale.text("status-action-cannot-boost")) {
                            (icon("boost"))
                        }
                    }
                    @if status.quotable() {
                        a.action.action--quote href=(format!("/compose?quote={}", status.id()))
                            title=(ctx.locale.text("status-action-quote")) {
                            (icon("quote")) span.action__count { (status.quotes_count()) }
                        }
                    } @else {
                        span.action.is-disabled
                            title=(ctx.locale.text("status-action-cannot-quote")) {
                            (icon("quote-off"))
                        }
                    }
                    @if status.group_post() {
                        (vote_cluster(status, token, ctx.return_to, ctx.locale))
                    } @else {
                        (action_form(status, "favourite", status.favourited(),
                            "favourite", Some(status.favourites_count()), token, ctx.return_to,
                            ctx.locale))
                    }
                    (reaction_picker(&format!("/web/statuses/{}", status.id()), token,
                        ctx.return_to, ctx.locale))
                    (action_form(status, "bookmark", status.bookmarked(),
                        "bookmark", None, token, ctx.return_to, ctx.locale))
                }
                None => {
                    // A logged-out visitor's clicks lead to the
                    // remote-interaction interstitial.
                    @let interact = super::interact::interact_href(status.uri());
                    a.action href=(interact)
                        title=(ctx.locale.text("status-action-boost-remote")) {
                        (icon("boost")) span.action__count { (status.reblogs_count()) }
                    }
                    a.action href=(interact)
                        title=(ctx.locale.text("status-action-quote-remote")) {
                        (icon("quote")) span.action__count { (status.quotes_count()) }
                    }
                    @if status.group_post() {
                        a.action href=(interact)
                            title=(ctx.locale.text("status-action-vote-remote")) {
                            (icon("upvote")) span.action__count { (status.score()) }
                        }
                    } @else {
                        a.action href=(interact)
                            title=(ctx.locale.text("status-action-favourite-remote")) {
                            (icon("favourite")) span.action__count { (status.favourites_count()) }
                        }
                    }
                }
            }
            @if let Some(menu) = status_privileged_menu(status, ctx) {
                (menu)
            }
            (overflow_menu(status, ctx))
        }
    }
}

/// A post's privileged-tools menu. Site-level shortcuts target the author of
/// the status actually rendered (boost wrappers are unwrapped before this
/// function is reached); local community moderation keeps targeting the post
/// itself. Existing Remove / Lock / Pin forms live here instead of the general
/// overflow menu so every privileged operation has one predictable home.
fn status_privileged_menu(status: &Status, ctx: &Ctx) -> Option<Markup> {
    let links = AdminTargetLinks::for_account(&status.account(), ctx.admin, ctx.locale);
    let borrow_href = (ctx.admin.manage_custom_emojis
        && status.account().is_remote()
        && !status.emojis().is_empty())
    .then(|| format!("/admin/custom-emojis/borrow/status/{}", status.id()));
    let group_mod = status.group_mod().zip(ctx.csrf);
    if links.is_empty() && borrow_href.is_none() && group_mod.is_none() {
        return None;
    }
    let title = ctx.locale.text("moderation-tools");
    Some(html! {
        details.status__menu.privileged-menu data-status-menu data-privileged-menu {
            summary.action title=(title) aria-label=(title) aria-haspopup="menu" {
                (icon("shield"))
            }
            div.status__menu-pop role="menu" {
                (links.markup())
                @if let Some(href) = &borrow_href {
                    a.status__menu-item href=(href) {
                        (ctx.locale.text("moderation-borrow-emojis"))
                    }
                }
                @if let Some((group_mod, csrf)) = &group_mod {
                    (group_mod_items(
                        status, group_mod, csrf, ctx.return_to, ctx.locale))
                }
            }
        }
    })
}

/// The "…" overflow menu: a `<details>` disclosure, so it opens without
/// JavaScript too — JS only adds outside-click/Escape closing and viewport
/// placement. The engagement-list links mean every menu has working
/// no-JS entries, so the old "hide an all-JS menu" case is gone; copy-link
/// stays the one scripted item.
#[allow(clippy::too_many_lines)] // one flat menu covering every per-post verb
fn overflow_menu(status: &Status, ctx: &Ctx) -> Markup {
    let url = status.url();
    let remote = status.is_remote();
    let permalink = status.permalink();
    html! {
        details.status__menu data-status-menu {
            summary.action title=(ctx.locale.text("status-more-options")) { (icon("more")) }
            div.status__menu-pop role="menu" {
                @if !url.is_empty() {
                    button.status__menu-item.status__menu-item--js type="button"
                        data-copy-link=(url) { (ctx.locale.text("status-copy-link")) }
                }
                @if remote && !url.is_empty() {
                    a.status__menu-item href=(url) target="_blank" rel="noopener noreferrer" {
                        (ctx.locale.text("status-open-original-page"))
                    }
                }
                // The three engagement lists share one horizontal menu row.
                div.status__menu-row {
                    a.status__menu-item href=(format!("{permalink}/reblogs")) {
                        (ctx.locale.text("status-boosts"))
                    }
                    a.status__menu-item href=(format!("{permalink}/quotes")) {
                        (ctx.locale.text("status-quotes"))
                    }
                    a.status__menu-item href=(format!("{permalink}/favourites")) {
                        (ctx.locale.text("status-favourites-short"))
                    }
                }
                @if let Some(csrf) = ctx.csrf {
                    @if ctx.admin.personal_custom_emojis && status.has_borrowable_emojis() {
                        a.status__menu-item href=(format!("/settings/custom-emojis/borrow/status/{}", status.id())) {
                            (ctx.locale.text("custom-emojis-borrow-personal"))
                        }
                    }
                    @if status.muted() {
                        (menu_form(&format!("/web/statuses/{}/unmute", status.id()),
                            &ctx.locale.text("status-unmute-conversation"), false, None, csrf,
                            ctx.return_to))
                    } @else {
                        (menu_form(&format!("/web/statuses/{}/mute", status.id()),
                            &ctx.locale.text("status-mute-conversation"), false, None, csrf,
                            ctx.return_to))
                    }
                    @if ctx.owns(status) {
                        @match status.pinned() {
                            Some(true) => (menu_form(
                                &format!("/web/statuses/{}/unpin", status.id()),
                                &ctx.locale.text("status-unpin-profile"), false, None, csrf,
                                ctx.return_to)),
                            Some(false) => (menu_form(
                                &format!("/web/statuses/{}/pin", status.id()),
                                &ctx.locale.text("status-pin-profile"), false, None, csrf,
                                ctx.return_to)),
                            None => {}
                        }
                        @if let Some(policy) = status.quote_policy_value() {
                            @if status.boostable() {
                                (quote_policy_form(
                                    status, policy, csrf, ctx.return_to, ctx.locale))
                            }
                        }
                        a.status__menu-item
                            href=(format!("/web/statuses/{}/edit", status.id())) {
                            (ctx.locale.text("status-edit"))
                        }
                        (menu_form(&format!("/web/statuses/{}/redraft", status.id()),
                            &ctx.locale.text("status-delete-redraft"), true,
                            Some(&ctx.locale.text("status-redraft-confirm")),
                            csrf, ctx.return_to))
                        (menu_form(&format!("/web/statuses/{}/delete", status.id()),
                            &ctx.locale.text("status-delete"), true,
                            Some(&ctx.locale.text("status-delete-confirm")), csrf, ctx.return_to))
                    } @else {
                        @let account = status.account();
                        (moderation_form(&ModForm {
                            kind: "mute",
                            action: format!("/web/accounts/{}/mute", account.id()),
                            undo_action: format!("/web/accounts/{}/unmute", account.id()),
                            label: localized_arg(ctx.locale, "status-mute-account",
                                "account", account.acct()),
                            undo_label: localized_arg(ctx.locale, "status-unmute-account",
                                "account", account.acct()),
                            account: Some(account.id()),
                            domain: None,
                            confirm: None,
                        }, csrf, ctx.return_to))
                        (moderation_form(&ModForm {
                            kind: "block",
                            action: format!("/web/accounts/{}/block", account.id()),
                            undo_action: format!("/web/accounts/{}/unblock", account.id()),
                            label: localized_arg(ctx.locale, "status-block-account",
                                "account", account.acct()),
                            undo_label: localized_arg(ctx.locale, "status-unblock-account",
                                "account", account.acct()),
                            account: Some(account.id()),
                            domain: None,
                            confirm: Some(localized_arg(ctx.locale,
                                "status-block-account-confirm", "account", account.acct())),
                        }, csrf, ctx.return_to))
                        @let report_query = serde_urlencoded::to_string(
                            [("return_to", ctx.return_to)]).unwrap_or_default();
                        a.status__menu-item.is-danger
                            href=(format!("/web/statuses/{}/report?{report_query}", status.id())) {
                            (localized_arg(ctx.locale, "status-report-account",
                                "account", account.acct()))
                        }
                        @if let Some(domain) = status.remote_domain() {
                            (moderation_form(&ModForm {
                                kind: "domain",
                                action: "/web/domains/block".into(),
                                undo_action: "/web/domains/unblock".into(),
                                label: localized_arg(ctx.locale, "status-block-domain",
                                    "domain", domain),
                                undo_label: localized_arg(ctx.locale, "status-unblock-domain",
                                    "domain", domain),
                                account: None,
                                domain: Some(domain),
                                confirm: Some(localized_arg(ctx.locale,
                                    "status-block-domain-confirm", "domain", domain)),
                            }, csrf, ctx.return_to))
                        }
                    }
                }
            }
        }
    }
}

/// The overflow menu's Moderate section for a group moderator: remove the post
/// from the group, lock/unlock its thread, and (top-level posts only) pin or
/// unpin it — each a single contextual verb reflecting the current state. Flat
/// menu items in the same popup, so they open and submit like every other
/// entry, including on touch (no nested disclosure to fight the outside-tap
/// dismissal).
fn group_mod_items(
    status: &Status,
    group_mod: &GroupMod,
    csrf: &str,
    return_to: &str,
    locale: Locale,
) -> Markup {
    let base = format!("/web/groups/{}/posts/{}", group_mod.group_id, status.id());
    let (lock_action, lock_label) = if group_mod.locked {
        (
            format!("{base}/lock?unlock=1"),
            locale.text("status-group-unlock"),
        )
    } else {
        (format!("{base}/lock"), locale.text("status-group-lock"))
    };
    let (pin_action, pin_label) = if group_mod.pinned {
        (
            format!("{base}/pin?unpin=1"),
            locale.text("status-group-unpin"),
        )
    } else {
        (format!("{base}/pin"), locale.text("status-group-pin"))
    };
    html! {
        p.status__menu-label {
            (icon("shield")) " " (locale.text("status-group-moderate"))
        }
        (menu_form(&format!("{base}/remove"), &locale.text("status-group-remove"), true,
            Some(&locale.text("status-group-remove-confirm")), csrf, return_to))
        (menu_form(&lock_action, &lock_label, false, None, csrf, return_to))
        // Only a top-level submission can be featured; comments have no pin.
        @if status.in_reply_to_id().is_none() {
            (menu_form(&pin_action, &pin_label, false, None, csrf, return_to))
        }
    }
}

/// One overflow-menu verb as a POST form; `confirm` invites the JS
/// confirmation prompt (a no-JS submit just posts).
pub fn menu_form(
    action_path: &str,
    label: &str,
    danger: bool,
    confirm: Option<&str>,
    csrf: &str,
    return_to: &str,
) -> Markup {
    html! {
        form.status__menu-form method="post" action=(action_path) data-confirm=[confirm] {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(return_to);
            button.status__menu-item.is-danger[danger] type="submit" { (label) }
        }
    }
}

/// The owner's "who can quote" control (`PATCH interaction_policy` behind a
/// web form): a select preset to the post's current policy plus an apply
/// button, as one menu row.
fn quote_policy_form(
    status: &Status,
    current: &str,
    csrf: &str,
    return_to: &str,
    locale: Locale,
) -> Markup {
    html! {
        form.status__menu-form.status__menu-policy method="post"
            action=(format!("/web/statuses/{}/quote_policy", status.id())) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(return_to);
            label.status__menu-policy-label {
                span { (locale.text("compose-quote-policy")) }
                select name="policy" {
                    @for &(value, label_id, _, _) in &QUOTE_POLICIES {
                        option value=(value) selected[value == current] {
                            (locale.text(label_id))
                        }
                    }
                }
            }
            button.status__menu-policy-apply type="submit" {
                (locale.text("status-apply"))
            }
        }
    }
}

/// A status-menu moderation verb (account mute/block, user-level domain
/// block) the JS can flip in place. `data-mod` names the kind; the other
/// `data-mod-*` attributes carry the key matching the affected articles
/// (`account` → `data-author-id`/`data-booster-id`, `domain` → the domain
/// attributes) and the undo direction, so a successful background POST dims
/// the sanctioned account's cards and swaps the button to the inverse verb
/// without leaving the page. A no-JS submit still posts and redirects.
struct ModForm<'a> {
    kind: &'a str,
    action: String,
    undo_action: String,
    label: String,
    undo_label: String,
    account: Option<&'a str>,
    domain: Option<&'a str>,
    confirm: Option<String>,
}

fn moderation_form(form: &ModForm, csrf: &str, return_to: &str) -> Markup {
    // Only the confirmed (destructive) direction gets danger styling; the
    // JS drops both together when it flips the form to the undo verb.
    let danger = form.confirm.is_some();
    html! {
        form.status__menu-form method="post" action=(form.action)
            data-confirm=[form.confirm.as_deref()]
            data-mod=(form.kind)
            data-mod-account=[form.account]
            data-mod-domain=[form.domain]
            data-mod-undo-action=(form.undo_action)
            data-mod-undo-label=(form.undo_label) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(return_to);
            @if let Some(domain) = form.domain {
                input type="hidden" name="domain" value=(domain);
            }
            button.status__menu-item.is-danger[danger] type="submit" { (form.label) }
        }
    }
}

/// The group-post vote cluster: upvote toggle, score, downvote
/// toggle — Lemmy's ▲ n ▼ — built from the same flip-to-`un` forms as every
/// other action. The upvote rides the favourite store, so a starred group
/// post reads as upvoted everywhere and vice versa.
fn vote_cluster(status: &Status, csrf: &str, return_to: &str, locale: Locale) -> Markup {
    html! {
        span.status__votes {
            (action_form(status, "upvote", status.favourited(),
                "upvote", None, csrf, return_to, locale))
            span.status__score title=(locale.text("status-action-score")) { (status.score()) }
            (action_form(status, "downvote", status.downvoted(),
                "downvote", None, csrf, return_to, locale))
        }
    }
}

/// One toggle action as a POST form. The path flips to the `un`-prefixed
/// endpoint when the action is already active, so a single button both sets
/// and clears the state with JS disabled.
#[allow(
    clippy::too_many_arguments,
    reason = "one flat status action form carrying its rendering context"
)]
fn action_form(
    status: &Status,
    action: &str,
    active: bool,
    icon_name: &str,
    count: Option<i64>,
    csrf: &str,
    return_to: &str,
    locale: Locale,
) -> Markup {
    let verb = if active {
        format!("un{action}")
    } else {
        action.to_owned()
    };
    let path = format!("/web/statuses/{}/{verb}", status.id());
    let class = if active {
        "action action__btn is-active"
    } else {
        "action action__btn"
    };
    let action_message = |active| match (action, active) {
        ("reblog", false) => "status-action-boost",
        ("reblog", true) => "status-action-unboost",
        ("favourite", false) => "status-action-favourite",
        ("favourite", true) => "status-action-unfavourite",
        ("bookmark", false) => "status-action-bookmark",
        ("bookmark", true) => "status-action-unbookmark",
        ("upvote", false) => "status-action-upvote",
        ("upvote", true) => "status-action-unupvote",
        ("downvote", false) => "status-action-downvote",
        ("downvote", true) => "status-action-undownvote",
        _ => "status-more-options",
    };
    let message = action_message(active);
    html! {
        form.action-form method="post" action=(path) data-action=(action) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(return_to);
            button type="submit" class=(class) aria-pressed=(active)
                title=(locale.text(message))
                data-inactive-title=(locale.text(action_message(false)))
                data-active-title=(locale.text(action_message(true))) {
                (icon(icon_name))
                @if let Some(n) = count { span.action__count { (n) } }
            }
        }
    }
}

/// The Pleroma emoji-reaction chips under the action bar: one chip
/// per reacted emoji with its count, `me`-highlighted, toggling as a plain
/// POST form. The "add a reaction" picker lives in the action bar; an
/// unreacted post renders the container empty (`:empty` collapses it) so the
/// script has an anchor to swap when the first reaction arrives.
pub(crate) fn reactions_row(status: &Status, ctx: &Ctx) -> Markup {
    let base = format!("/web/statuses/{}", status.id());
    reaction_chips_row(
        &base,
        status.reactions(),
        ctx.csrf,
        ctx.return_to,
        ctx.locale,
    )
}

/// The chips row shared by statuses and announcements, keyed by the web verb
/// base path (`/web/statuses/{id}` / `/web/announcements/{id}`): the script
/// toggles at `{base}/(un)react/{name}` and re-fetches `{base}/reactions` to
/// swap the row in place.
pub(crate) fn reaction_chips_row(
    base: &str,
    reactions: &[Value],
    csrf: Option<&str>,
    return_to: &str,
    locale: Locale,
) -> Markup {
    html! {
        div.status__reactions data-reactions=(base) {
            @for reaction in reactions {
                (reaction_chip(base, reaction, csrf, return_to, locale))
            }
        }
    }
}

/// One reaction chip. Signed-in viewers get a toggle form (react/unreact by
/// whether they already reacted); anonymous viewers get an inert chip. A
/// remote custom emoji (qualified `shortcode@host` name) toggles too — the
/// server joins the existing reaction, Pleroma-style.
fn reaction_chip(
    base: &str,
    reaction: &Value,
    csrf: Option<&str>,
    return_to: &str,
    locale: Locale,
) -> Markup {
    let field = |key: &str| reaction.get(key).and_then(Value::as_str).unwrap_or("");
    let name = field("name");
    let count = reaction.get("count").and_then(Value::as_i64).unwrap_or(0);
    let me = reaction.get("me").and_then(Value::as_bool).unwrap_or(false);
    let url = Some(field("url")).filter(|s| !s.is_empty());
    let face = html! {
        @match url {
            Some(url) => img.reaction__emoji src=(url) alt=(format!(":{name}:")) loading="lazy";,
            None => span.reaction__emoji { (name) },
        }
        span.reaction__count { (count) }
    };
    if let Some(csrf) = csrf {
        let verb = if me { "unreact" } else { "react" };
        html! {
            form.reaction-form method="post"
                action=(format!("{base}/{verb}/{name}")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value=(return_to);
                button.reaction.is-active[me] type="submit" aria-pressed=(me)
                    data-reaction-name=(name)
                    title=(locale.text(if me {
                        "status-action-remove-reaction"
                    } else {
                        "status-action-react"
                    })) {
                    (face)
                }
            }
        }
    } else {
        html! {
            span.reaction.is-active[me] data-reaction-name=(name) title=(name) { (face) }
        }
    }
}

/// The action-bar link to the full emoji picker. Without JavaScript it opens a
/// categorized server-rendered page; the enhancement intercepts it and opens
/// the same catalog in the composer popup shell. Neither the full Unicode data
/// nor the instance custom-emoji listing is present in an ordinary timeline:
/// both are fetched only on the first popup open. The hidden template form
/// carries the CSRF/return fields for enhanced submissions.
pub(crate) fn reaction_picker(base: &str, csrf: &str, return_to: &str, locale: Locale) -> Markup {
    let add_reaction = locale.text("status-action-add-reaction");
    let search_emoji = locale.text("status-search-emoji");
    let query = serde_urlencoded::to_string([("return_to", return_to)]).unwrap_or_default();
    let picker_href = format!("{base}/reaction?{query}");
    html! {
        div.reaction-picker data-reaction-picker data-react-base=(base)
            data-unicode-catalog=(super::assets::EMOJI_CATALOG_PATH) {
            a.action.action--react href=(picker_href) data-emoji-trigger
                aria-haspopup="true" aria-expanded="false" title=(add_reaction) {
                (icon("emoji"))
                span.visually-hidden { (locale.text("status-action-add-reaction")) }
            }
            div.compose__emoji-pop data-emoji-pop hidden {
                input.compose__emoji-search type="text" data-emoji-search
                    placeholder=(search_emoji) aria-label=(locale.text("status-search-emoji"))
                    autocomplete="off";
                div.compose__emoji-list data-emoji-list {}
            }
            form method="post" hidden data-react-form {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value=(return_to);
            }
        }
    }
}

/// The viewer's posting defaults, surfaced as the composer's initial state so
/// the profile preferences are actually honoured when writing a post.
pub struct ComposeDefaults<'a> {
    pub visibility: &'a str,
    pub sensitive: bool,
    pub language: &'a str,
    /// The languages offered by the composer's selector — the user's enabled
    /// set from `/settings/languages`, or the full inventory when unset.
    pub languages: &'a [&'static Language],
    /// `public` | `followers` | `nobody` — the user's default quote policy.
    pub quote_policy: &'a str,
    /// `text/plain` | `text/markdown` | `text/html` — the user's default
    /// composer format (P4).
    pub content_type: &'a str,
    /// The viewer's preference time zone — the zone the Schedule field's
    /// wall-clock value is interpreted in.
    pub time_zone: &'a str,
}

/// State carried back into the composer when it re-renders — the
/// delete-and-redraft flow (`text`/`spoiler_text`), and the server-side
/// preview, which echoes every field so nothing the user entered is lost and
/// shows the rendered post below. All-default = a fresh composer.
#[derive(Default)]
pub struct ComposePrefill<'a> {
    pub invitation: Option<Markup>,
    pub kind: &'a str,
    pub title: &'a str,
    pub event: EventCompose<'a>,
    pub text: &'a str,
    pub spoiler_text: &'a str,
    /// Submitted poll option texts (empty = no poll being composed).
    pub poll_options: &'a [String],
    pub poll_expires_in: Option<i64>,
    pub poll_multiple: bool,
    /// The Schedule field's `datetime-local` value, echoed across a
    /// preview or a failed submit.
    pub scheduled_at: &'a str,
    /// Already-uploaded attachments, as `MediaAttachment` entities — rendered
    /// as removable "Keep" cards (the no-JS media survives a preview as ids).
    pub media: &'a [Value],
    /// The rendered preview card (a `Preview` submit produced it); `None` on a
    /// fresh composer or a plain error re-render.
    pub preview: Option<Markup>,
    /// An error to surface above the composer (validation failed on submit).
    pub error: Option<&'a str>,
}

/// Event inputs retained across type changes, previews and validation errors.
#[derive(Default)]
pub struct EventCompose<'a> {
    pub start: &'a str,
    pub end: &'a str,
    pub timezone: &'a str,
    pub join_mode: &'a str,
    pub external_url: &'a str,
    pub capacity: &'a str,
    pub status: &'a str,
    pub location: &'a str,
    pub street: &'a str,
    pub locality: &'a str,
    pub region: &'a str,
    pub country: &'a str,
    pub postal_code: &'a str,
    pub online: bool,
}

/// Turns the shared composer into a group submission: the target group,
/// plus the Title and Link fields a forum-style post carries. When present the
/// composer posts to the group (a hidden `group_id`), locks visibility to
/// public (groups only relay public posts) and drops the visibility selector.
pub struct GroupCompose<'a> {
    /// The group account id, submitted as the hidden `group_id`. The
    /// "Posting to …" note is rendered by the page around the form.
    pub id: &'a str,
    /// Prefilled Title / Link values (echoed back on a preview or error).
    pub title: &'a str,
    pub external_url: &'a str,
}

/// Locks the shared composer onto a direct thread: the visibility selector
/// is dropped and a hidden `visibility=direct` field posts instead (the
/// server clamps direct-parent replies regardless — this keeps the form
/// honest), and the composer leads with who will receive the reply plus the
/// private-mention caveats. Participants are ready-made handles ("@user",
/// "@user@host"): the parent's audience minus the viewer. The audience is
/// inherited server-side, so it is displayed rather than prefilled as
/// mention text — deleting a handle from the body would not remove anyone.
pub struct DirectCompose {
    pub participants: Vec<String>,
}

/// The instance posting limits the composer renders to (media slots, poll
/// rows, character counter) — the live `instance_settings` values, so every
/// slot the form shows is actually accepted by `post_status`.
#[derive(Clone, Copy)]
pub struct ComposeLimits {
    pub max_characters: i32,
    /// The cap that applies instead when the long-form kind is ticked.
    pub max_characters_long_form: i32,
    pub max_media_attachments: i32,
    pub poll_max_options: i32,
}

impl ComposeLimits {
    /// A crude browser-side stop for the no-JS form: the weighted count can
    /// only be checked server-side (URLs weigh a fixed 23), so the hard
    /// `maxlength` stays a loose 10× of the real limit — same spirit as the
    /// old hardcoded 500-limit/5000-maxlength pair.
    ///
    /// One textarea serves both kinds, so it has to admit the larger of the two:
    /// a `maxlength` sized for ordinary posts would silently truncate a long-form
    /// draft in the browser, before anything server-side could say so.
    fn textarea_maxlength(self) -> i64 {
        i64::from(self.max_characters)
            .saturating_mul(10)
            .max(i64::from(self.max_characters_long_form))
    }
}

/// The five post visibility levels: value, menu label, glyph name, and the
/// one-line description shown in the composer's visibility menu.
const VISIBILITY_LEVELS: [(&str, &str, &str, &str); 5] = [
    (
        "public",
        "visibility-public",
        "vis-public",
        "visibility-public-help",
    ),
    (
        "unlisted",
        "visibility-unlisted",
        "vis-unlisted",
        "visibility-unlisted-help",
    ),
    (
        "private",
        "visibility-private",
        "vis-private",
        "visibility-private-help",
    ),
    (
        "direct",
        "visibility-direct",
        "vis-direct",
        "visibility-direct-help",
    ),
    (
        "local",
        "visibility-local",
        "vis-local",
        "visibility-local-help",
    ),
];

/// The three post text formats (P4, Pleroma's `content_type`): value, menu
/// label, glyph, and description — what the composer's format menu offers,
/// mirroring the instance's advertised `post_formats`.
const POST_FORMATS: [(&str, &str, &str, &str); 3] = [
    (
        "text/plain",
        "format-plain",
        "fmt-plain",
        "format-plain-help",
    ),
    (
        "text/markdown",
        "format-markdown",
        "fmt-markdown",
        "format-markdown-help",
    ),
    ("text/html", "format-html", "fmt-html", "format-html-help"),
];

/// The three quote policies ("who can quote"): value, menu label, glyph, and
/// description. A non-distributable post ignores it server-side (downgraded to
/// nobody).
const QUOTE_POLICIES: [(&str, &str, &str, &str); 3] = [
    (
        "public",
        "quote-policy-anyone",
        "quote-any",
        "quote-policy-anyone-help",
    ),
    (
        "followers",
        "quote-policy-followers",
        "quote-followers",
        "quote-policy-followers-help",
    ),
    (
        "nobody",
        "quote-policy-only-me",
        "quote-none",
        "quote-policy-only-me-help",
    ),
];

/// An icon-triggered composer menu (visibility, quote policy). Three layers are
/// rendered: a native `<select>` (the submitted value and the no-JS fallback),
/// an icon-only trigger button, and a listbox popup whose options each carry an
/// icon, label and one-line description. CSS shows the native select without JS
/// and the trigger+popup under the `.js` root; `bindComposeMenus` wires the
/// popup to write the chosen value back into the select and refresh the trigger.
fn compose_menu(
    name: &str,
    label: &str,
    selected: &str,
    options: &[(&str, &str, &str, &str)],
    locale: Locale,
) -> Markup {
    let current_glyph = options
        .iter()
        .find(|(value, ..)| *value == selected)
        .or_else(|| options.first())
        .map_or("", |(_, _, glyph, _)| *glyph);
    html! {
        div.compose__menu data-compose-menu {
            label.compose__menu-native {
                span.visually-hidden { (label) }
                select name=(name) {
                    @for &(value, label_id, _, _) in options {
                        option value=(value) selected[value == selected] { (locale.text(label_id)) }
                    }
                }
            }
            button.compose__menu-trigger type="button" data-menu-trigger
                aria-haspopup="listbox" aria-expanded="false" title=(label) {
                span.compose__menu-icon data-menu-icon { (icon(current_glyph)) }
                span.compose__menu-caret { (icon("chevron")) }
                span.visually-hidden { (label) }
            }
            div.compose__menu-pop role="listbox" aria-label=(label) data-menu-pop hidden {
                @for &(value, label_id, glyph, desc_id) in options {
                    button.compose__menu-option type="button" role="option"
                        data-value=(value)
                        aria-selected=(if value == selected { "true" } else { "false" }) {
                        span.compose__menu-option-icon { (icon(glyph)) }
                        span.compose__menu-option-text {
                            span.compose__menu-option-label { (locale.text(label_id)) }
                            span.compose__menu-option-desc { (locale.text(desc_id)) }
                        }
                        span.compose__menu-option-check { (icon("check")) }
                    }
                }
            }
        }
    }
}

/// The `<option>`s for a language `<select>`: every offered language in
/// proper-name form, `selected` preset. A selected code missing from `options`
/// — a language the user disabled, or an unknown code stored through the API —
/// is prepended, so submitting the form untouched never changes the stored
/// value.
fn language_options(options: &[&'static Language], selected: &str) -> Markup {
    let missing = !options.iter().any(|language| language.code == selected);
    html! {
        @if missing {
            @match languages::find(selected) {
                Some(language) => option value=(selected) selected { (language.label()) },
                None => option value=(selected) selected { (selected) },
            }
        }
        @for language in options {
            option value=(language.code) selected[language.code == selected] {
                (language.label())
            }
        }
    }
}

/// A language picker: a labelled pill over a native `<select>`,
/// shared between the composer and the settings forms. Without JS the native
/// select is the control; under `.js` the pill opens a popup with a search
/// box over the option list, which `bindComposeCombo` builds from the
/// select. The native select stays the submitted value and the no-JS fallback.
pub fn language_combo(
    name: &str,
    label: &str,
    options: &[&'static Language],
    selected: &str,
    locale: Locale,
) -> Markup {
    let current_label =
        languages::find(selected).map_or_else(|| selected.to_string(), Language::label);
    html! {
        div.compose__combo data-compose-combo
            data-empty-label=(locale.text("compose-no-languages")) {
            label.compose__combo-native {
                span.visually-hidden { (label) }
                select name=(name) {
                    (language_options(options, selected))
                }
            }
            button.compose__combo-trigger type="button" data-combo-trigger
                aria-haspopup="listbox" aria-expanded="false" title=(label) {
                span.compose__combo-label data-combo-label { (current_label) }
                span.compose__combo-caret { (icon("chevron")) }
            }
            div.compose__combo-pop data-combo-pop hidden {
                input.compose__combo-search type="text" data-combo-search
                    placeholder=(locale.text("compose-search-languages"))
                    aria-label=(locale.text("compose-search-languages"))
                    autocomplete="off";
                ul.compose__combo-list role="listbox" aria-label=(label) data-combo-list {}
            }
        }
    }
}

/// [`language_combo`] with a leading choice that submits an empty value —
/// for preferences that fall back to another setting when unset (the
/// translate-to language follows the default posting language).
pub fn language_combo_optional(
    name: &str,
    label: &str,
    options: &[&'static Language],
    selected: Option<&str>,
    none_label: &str,
) -> Markup {
    let current_label = match selected {
        Some(code) => languages::find(code).map_or_else(|| code.to_string(), Language::label),
        None => none_label.to_owned(),
    };
    html! {
        div.compose__combo data-compose-combo {
            label.compose__combo-native {
                span.visually-hidden { (label) }
                select name=(name) {
                    option value="" selected[selected.is_none()] { (none_label) }
                    @match selected {
                        Some(code) => (language_options(options, code)),
                        None => @for language in options {
                            option value=(language.code) { (language.label()) }
                        },
                    }
                }
            }
            button.compose__combo-trigger type="button" data-combo-trigger
                aria-haspopup="listbox" aria-expanded="false" title=(label) {
                span.compose__combo-label data-combo-label { (current_label) }
                span.compose__combo-caret { (icon("chevron")) }
            }
            div.compose__combo-pop data-combo-pop hidden {
                input.compose__combo-search type="text" data-combo-search
                    placeholder="Search languages" aria-label="Search languages"
                    autocomplete="off";
                ul.compose__combo-list role="listbox" aria-label=(label) data-combo-list {}
            }
        }
    }
}

/// The custom-emoji picker: a toolbar button opening a searchable,
/// category-grouped grid of the instance's picker emoji that inserts
/// `:shortcode:` at the caret. Inserting into a textarea is inherently a
/// JavaScript act, so there is no no-JS variant — the trigger shares the
/// `.compose__tool` class that only shows under the `.js` root, and without
/// JS shortcodes are simply typed by hand. `bindComposeEmoji` fills the popup
/// from `GET /api/v1/custom_emojis` on first open.
fn compose_emoji_picker(locale: Locale) -> Markup {
    html! {
        div.compose__emoji data-compose-emoji {
            button.compose__tool type="button" data-emoji-trigger
                aria-haspopup="true" aria-expanded="false"
                title=(locale.text("compose-insert-emoji")) {
                (icon("emoji"))
                span.visually-hidden { (locale.text("compose-insert-emoji")) }
            }
            div.compose__emoji-pop data-emoji-pop hidden {
                input.compose__emoji-search type="text" data-emoji-search
                    placeholder=(locale.text("compose-search-emoji"))
                    aria-label=(locale.text("compose-search-emoji"))
                    autocomplete="off";
                div.compose__emoji-list data-emoji-list {}
            }
        }
    }
}

/// A toolbar icon button that toggles a collapsible composer section
/// (`section` names its `data-compose-section`). Hidden without JavaScript —
/// the section it controls stays visible in that case — so it only appears once
/// the toggle behaviour is wired up.
fn compose_toggle(section: &str, glyph: &str, label: &str) -> Markup {
    html! {
        button.compose__tool type="button"
            data-compose-toggle=(section) title=(label) aria-expanded="false" {
            (icon(glyph))
            span.visually-hidden { (label) }
        }
    }
}

/// The full compose editor — the single composer used everywhere a post is
/// written (the `/compose` page and inline replies in a thread): content
/// warning, per-attachment media with alt text, a sensitive toggle, language,
/// visibility, quote/reply context and a poll builder.
///
/// Laid out in the familiar Mastodon shape: the post text on top, the
/// content-warning / media / poll groups around it, and a footer toolbar with
/// icon buttons, the visibility/quote/language selectors, a live character
/// counter and the publish button. Works without JavaScript — the groups are
/// then always visible; JS collapses them behind the toolbar's toggle buttons,
/// auto-grows the textarea and drives the weighted character counter.
/// One already-uploaded attachment as a removable card: a thumbnail, a "Keep"
/// checkbox carrying its `media_id`, and an editable alt-text field. Unchecking
/// Keep drops it on the next submit. Shared by the edit composer and the
/// new-post composer's preview, so an attachment survives a no-JS preview
/// as an id and can still be removed afterward.
fn attachment_keep_row(item: &Value, locale: Locale) -> Markup {
    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
    let preview = item
        .get("preview_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let alt = item
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    html! {
        div.edit-media__row {
            @match preview {
                Some(preview) => {
                    img.edit-media__thumb src=(preview) alt="" loading="lazy";
                }
                None => a.edit-media__thumb.edit-media__thumb--file
                    href=(url) target="_blank" rel="noopener noreferrer" {
                    (icon("upload"))
                }
            }
            div.edit-media__fields {
                label.compose__inline {
                    input type="checkbox" name="media_keep[]" value=(id) checked;
                    span { (locale.text("compose-keep")) }
                }
                label.compose__field {
                    span.visually-hidden { (locale.text("compose-alt-text")) }
                    input type="text" name=(format!("media_alt_{id}"))
                        value=(alt) maxlength="1500"
                        placeholder=(locale.text("compose-alt-description"));
                }
            }
        }
    }
}

fn event_compose_dates(
    event: &EventCompose,
    active: bool,
    default_zone: &str,
    locale: Locale,
) -> Markup {
    let zone = if event.timezone.is_empty() {
        default_zone
    } else {
        event.timezone
    };
    html! {
        fieldset.compose__type-fields.compose__event-dates data-compose-event-fields hidden[!active] disabled[!active] {
            label.compose__field {
                span.compose__legend { (locale.text("compose-event-start")) }
                input type="datetime-local" name="event_start" value=(event.start) required;
            }
            label.compose__field {
                span.compose__legend { (locale.text("compose-event-end")) }
                input type="datetime-local" name="event_end" value=(event.end);
            }
            label.compose__field {
                span.compose__legend { (locale.text("compose-event-venue-zone")) }
                select name="event_timezone" data-compose-timezone {
                    @for (name, label) in super::clock::event_zone_options(event.start) {
                        option value=(name) selected[name == zone] { (label) }
                    }
                }
            }
        }
    }
}

fn event_compose_details(event: &EventCompose, active: bool, locale: Locale) -> Markup {
    html! {
        fieldset.compose__type-fields.compose__event-body data-compose-event-fields hidden[!active] disabled[!active] {
            label.compose__inline {
                span { (locale.text("compose-event-join-mode")) }
                select name="event_join_mode" {
                    option value="free" selected[event.join_mode == "free" || event.join_mode.is_empty()] { (locale.text("compose-event-join-free")) }
                    option value="restricted" selected[event.join_mode == "restricted"] {
                        (locale.text("compose-event-join-restricted"))
                    }
                    option value="invite" selected[event.join_mode == "invite"] { (locale.text("compose-event-join-invite")) }
                    option value="external" selected[event.join_mode == "external"] { (locale.text("compose-event-join-external")) }
                }
            }
            fieldset.compose__type-fields data-compose-external-fields {
              label.compose__field {
                span.compose__legend {
                    (locale.text("compose-event-external-url"))
                }
                input type="url" name="event_external_url" value=(event.external_url) autocomplete="off";
              }
            }
            label.compose__field {
                span.compose__legend { (locale.text("compose-event-capacity")) }
                input type="number" name="event_capacity" value=(event.capacity) min="1" step="1";
                span.compose__hint { (locale.text("compose-event-capacity-hint")) }
            }
            label.compose__inline {
                input type="checkbox" name="event_online" value="true" checked[event.online];
                span { (locale.text("compose-event-online")) }
            }
            label.compose__field {
                span.compose__legend { (locale.text("compose-event-venue")) }
                input type="text" name="event_location" value=(event.location) autocomplete="off"
                    placeholder=(locale.text("compose-event-venue-placeholder"));
            }
            div.compose__event-address {
                label.compose__field {
                    span.compose__legend { (locale.text("compose-event-street")) }
                    input type="text" name="event_street" value=(event.street) autocomplete="off";
                }
                label.compose__field {
                    span.compose__legend { (locale.text("compose-event-locality")) }
                    input type="text" name="event_locality" value=(event.locality) autocomplete="off";
                }
                label.compose__field {
                    span.compose__legend { (locale.text("compose-event-region")) }
                    input type="text" name="event_region" value=(event.region) autocomplete="off";
                }
                label.compose__field {
                    span.compose__legend { (locale.text("compose-event-postal-code")) }
                    input type="text" name="event_postal_code" value=(event.postal_code) autocomplete="off";
                }
                label.compose__field {
                    span.compose__legend { (locale.text("compose-event-country")) }
                    input type="text" name="event_country" value=(event.country) autocomplete="off";
                }
            }
            label.compose__inline {
                span { (locale.text("compose-event-status")) }
                select name="event_status" {
                    option value="CONFIRMED" selected[event.status == "CONFIRMED" || event.status.is_empty()] {
                        (locale.text("compose-event-status-confirmed"))
                    }
                    option value="TENTATIVE" selected[event.status == "TENTATIVE"] {
                        (locale.text("compose-event-status-tentative"))
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one flat form covering every compose surface
pub fn full_compose_form(
    csrf: &str,
    reply: Option<&str>,
    quote: Option<&str>,
    group: Option<&GroupCompose>,
    direct: Option<&DirectCompose>,
    defaults: &ComposeDefaults,
    limits: ComposeLimits,
    prefill: &ComposePrefill,
    locale: Locale,
) -> Markup {
    let kind = match (reply.is_some(), prefill.kind) {
        (false, "article") => "article",
        (false, "event") => "event",
        _ => "note",
    };
    let placeholder_id = if quote.is_some() {
        "compose-placeholder-quote"
    } else if reply.is_some() {
        "compose-placeholder-reply"
    } else if group.is_some() {
        "compose-placeholder-group"
    } else {
        "compose-placeholder-new"
    };
    let note_placeholder = locale.text(placeholder_id);
    let placeholder = match kind {
        "article" => locale.text("compose-placeholder-article"),
        "event" => locale.text("compose-placeholder-event"),
        _ => note_placeholder.clone(),
    };
    // Attachments already uploaded (a prior preview) take slots away from the
    // no-JS file-input list; the remainder can still be added.
    let attached = i32::try_from(prefill.media.len()).unwrap_or(i32::MAX);
    let media_slots = (limits.max_media_attachments - attached).max(0);
    let poll_expires = prefill.poll_expires_in.unwrap_or(86_400);
    let poll_option = |index: i32| -> &str {
        usize::try_from(index)
            .ok()
            .and_then(|i| prefill.poll_options.get(i))
            .map_or("", String::as_str)
    };
    let numbered = |id: &str, number: i32| {
        let mut args = FluentArgs::new();
        args.set("number", number);
        locale.text_with(id, &args)
    };
    let participants = direct.map(|direct| {
        let mut args = FluentArgs::new();
        args.set("participants", direct.participants.join(", "));
        locale.text_with("compose-private-participants", &args)
    });
    let schedule_help = {
        let mut args = FluentArgs::new();
        args.set(
            "zone",
            ViewerClock::of(Some(defaults.time_zone), locale).label(),
        );
        locale.text_with("compose-schedule-help", &args)
    };
    html! {
        form.compose-form data-compose
            data-i18n-media-add=(locale.text("compose-media-add"))
            data-i18n-media-drop-help=(locale.text("compose-media-drop-help"))
            data-i18n-alt-description=(locale.text("compose-alt-description"))
            data-i18n-alt-named=(locale.text("compose-alt-named-template"))
            data-i18n-remove-named=(locale.text("compose-remove-named-template"))
            data-i18n-add-option=(locale.text("compose-add-option"))
            data-i18n-remove-option=(locale.text("compose-remove-option"))
            data-i18n-poll-choice=(locale.text("compose-poll-choice-template"))
            data-i18n-poll-option=(locale.text("compose-poll-option-template"))
            data-i18n-preview-failed=(locale.text("compose-preview-failed"))
            method="post" action="/web/compose"
            enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(csrf);
            @if let Some(id) = reply { input type="hidden" name="in_reply_to_id" value=(id); }
            @if let Some(id) = quote { input type="hidden" name="quoted_status_id" value=(id); }
            // A group submission carries the target group and posts public (a
            // group only relays public posts); the visibility selector is
            // dropped from the toolbar below.
            @if let Some(group) = group {
                input type="hidden" name="group_id" value=(group.id);
                input type="hidden" name="visibility" value="public";
            }
            // A direct-thread reply is likewise locked (hidden field, not a
            // disabled select — a disabled control submits nothing).
            @if group.is_none() && direct.is_some() {
                input type="hidden" name="visibility" value="direct";
            }

            @if reply.is_none() {
                div.compose__type-row {
                    label.compose__kind {
                        span.visually-hidden { (locale.text("compose-post-type")) }
                        select name="post_kind" data-compose-kind {
                            @for (value, message) in [("note", "compose-kind-note"), ("article", "compose-kind-article"), ("event", "compose-event")] {
                                option value=(value) selected[kind == value] { (locale.text(message)) }
                            }
                        }
                    }
                    details.compose__compatibility {
                        summary { (locale.text("compose-compatibility")) }
                        @for (value, message) in [
                            ("note", if group.is_some() { "compose-compatibility-group" } else { "compose-compatibility-note" }),
                            ("article", "compose-compatibility-article"),
                            ("event", "compose-compatibility-event"),
                        ] {
                            ul data-compose-compatibility=(value) hidden[kind != value] {
                                @for line in locale.text(message).lines().filter(|line| !line.trim().is_empty()) {
                                    li { (line) }
                                }
                            }
                        }
                    }
                }
                noscript {
                    button type="submit" name="op" value="change_kind" formnovalidate {
                        (locale.text("compose-change-type"))
                    }
                }
            } @else {
                input type="hidden" name="post_kind" value="note";
            }

            @if let Some(invitation) = &prefill.invitation { (invitation) }
            div.compose.compose--full.compose--direct[direct.is_some()] {
                // A submit that failed validation re-renders the whole composer with
                // this banner rather than the old bare error page.
                @if let Some(error) = prefill.error {
                    p.compose__error role="alert" { (error) }
                }

                // Who a private mention reaches, up front — the "who's included"
                // line Mastodon's own issue tracker keeps asking for — plus the
                // battle-tested caveats.
                @if let Some(direct) = direct {
                    aside.compose__direct data-compose-direct {
                        p.compose__direct-title {
                            (icon("vis-direct")) " " (locale.text("compose-private-title"))
                        }
                        p.compose__direct-participants {
                            @if direct.participants.is_empty() {
                                (locale.text("compose-private-only-you"))
                            } @else {
                                (participants.as_deref().unwrap_or_default())
                            }
                        }
                        p.compose__direct-hint { (locale.text("compose-private-hint")) }
                    }
                }

                fieldset.compose__type-fields data-compose-title-fields
                    hidden[kind == "note" && group.is_none()]
                    disabled[kind == "note" && group.is_none()] {
                    label.compose__field {
                        span.compose__legend data-compose-title-label
                            data-note=(locale.text("compose-title"))
                            data-article=(locale.text("compose-long-form-title"))
                            data-event=(locale.text("compose-event-title")) {
                            (locale.text(if kind == "event" { "compose-event-title" } else { "compose-title" }))
                        }
                        input type="text" name="title" value=(group.map_or(prefill.title, |g| g.title))
                            maxlength="200" autocomplete="off" data-compose-title
                            required[kind != "note"];
                    }
                }
                @if let Some(group) = group {
                    div.compose__group-fields data-compose-group {
                        label.compose__field {
                            span.compose__legend { (locale.text("compose-link")) }
                            input type="url" name="external_url" value=(group.external_url)
                                autocomplete="off" data-compose-link
                                placeholder=(locale.text("compose-link-placeholder"));
                        }
                    }
                }
                @if reply.is_none() {
                    (event_compose_dates(&prefill.event, kind == "event", defaults.time_zone, locale))
                }

                // Content warning — above the text, like Mastodon. Collapsed behind
                // the toolbar's CW toggle with JS; always shown without it.
                div.compose__section data-compose-section="cw" {
                    label.compose__field {
                        span.compose__legend { (locale.text("compose-content-warning")) }
                        input type="text" name="spoiler_text" data-compose-spoiler
                            value=(prefill.spoiler_text)
                            placeholder=(locale.text("compose-content-warning-placeholder"));
                    }
                }

                // The post body. No `required`: the backend accepts media-only
                // posts (empty text + attachments), so the form must too.
                label.compose__field.compose__text {
                    span.compose__legend.visually-hidden[kind == "note"] data-compose-body-label
                        data-note=(locale.text("compose-post-text"))
                        data-article=(locale.text("compose-article-body"))
                        data-event=(locale.text("compose-event-description")) {
                        (locale.text(match kind { "article" => "compose-article-body", "event" => "compose-event-description", _ => "compose-post-text" }))
                    }
                    textarea name="status" rows=(if kind == "article" { 12 } else { 5 }) data-autogrow
                        maxlength=(limits.textarea_maxlength())
                        data-max-chars=(limits.max_characters)
                        data-max-chars-long-form=(limits.max_characters_long_form)
                        data-placeholder-note=(note_placeholder)
                        data-placeholder-article=(locale.text("compose-placeholder-article"))
                        data-placeholder-event=(locale.text("compose-placeholder-event"))
                        placeholder=(placeholder) { (prefill.text) }
                }

                // Attachments uploaded on a prior preview: always-visible
                // removable cards, not a collapsible section — you can drop any
                // before posting. The no-JS path carries media across a preview as
                // these ids; JS keeps its own file list and never reaches here.
                @if !prefill.media.is_empty() {
                    div.compose__edit-media {
                        span.compose__legend { (locale.text("compose-attachments")) }
                        @for item in prefill.media {
                            (attachment_keep_row(item, locale))
                        }
                    }
                }

                div.compose__section data-compose-section="media"
                    data-media-max=(media_slots) {
                    span.compose__legend { (locale.text("compose-media")) }
                    div.compose__media-body {
                        // The static slots are the no-JS fallback: one file input
                        // per remaining attachment, each with its own alt text. The
                        // alt input precedes its file input so the handler can pair
                        // them (`media_alt[]` applies to the `media[]` part that
                        // follows it); empty slots are skipped on submit. Under the
                        // `.js` root the media manager removes this block and drives a
                        // dynamic list instead, preserving the same submit
                        // order so the backend parsing is unchanged.
                        @if media_slots > 0 {
                            div.compose__media-static data-media-static {
                                @for index in 0..media_slots {
                                    div.compose__media-slot {
                                        label.compose__field {
                                            span.visually-hidden {
                                                (numbered("compose-alt-file", index + 1))
                                            }
                                            input type="text" name="media_alt[]" maxlength="1500"
                                                placeholder=(numbered("compose-alt-file", index + 1));
                                        }
                                        label.compose__field {
                                            span.visually-hidden {
                                                (numbered("compose-file", index + 1))
                                            }
                                            input type="file" name="media[]"
                                                accept="image/*,video/*,audio/*";
                                        }
                                    }
                                }
                            }
                        }
                        label.compose__inline {
                            input type="hidden" name="sensitive" value="false";
                            input type="checkbox" name="sensitive" value="true"
                                checked[defaults.sensitive];
                            span { (locale.text("compose-sensitive")) }
                        }
                    }
                }

                fieldset.compose__section.compose__type-fields data-compose-section="poll"
                    data-compose-note-only hidden[kind != "note"] disabled[kind != "note"]
                    data-poll-max=(limits.poll_max_options) {
                    span.compose__legend { (locale.text("compose-poll")) }
                    div.compose__poll-body {
                        // No-JS fallback: a static option row per allowed choice,
                        // empties dropped on submit. Under `.js` the poll builder
                        // removes this block and drives a dynamic add/remove list
                        // that grows up to `data-poll-max`.
                        div.compose__poll-static data-poll-static {
                            @for index in 0..limits.poll_max_options {
                                label.compose__field {
                                    span.visually-hidden {
                                        (numbered("compose-poll-option", index + 1))
                                    }
                                    input type="text" name="poll_options[]" maxlength="50"
                                        value=(poll_option(index))
                                        placeholder=(numbered("compose-poll-choice", index + 1));
                                }
                            }
                        }
                        div.compose__poll-controls {
                            label.compose__inline {
                                span { (locale.text("compose-poll-duration")) }
                                select name="poll_expires_in" {
                                    option value="3600" selected[poll_expires == 3600] {
                                        (locale.text("compose-poll-hour-one"))
                                    }
                                    option value="21600" selected[poll_expires == 21_600] {
                                        (locale.text("compose-poll-hour-six"))
                                    }
                                    option value="86400" selected[poll_expires == 86_400] {
                                        (locale.text("compose-poll-day-one"))
                                    }
                                    option value="259200" selected[poll_expires == 259_200] {
                                        (locale.text("compose-poll-day-three"))
                                    }
                                    option value="604800" selected[poll_expires == 604_800] {
                                        (locale.text("compose-poll-week-one"))
                                    }
                                }
                            }
                            label.compose__inline {
                                input type="checkbox" name="poll_multiple" value="true"
                                    checked[prefill.poll_multiple];
                                span { (locale.text("compose-poll-multiple")) }
                            }
                        }
                    }
                }

                @if reply.is_none() {
                    (event_compose_details(&prefill.event, kind == "event", locale))
                }

                // Schedule for later: a filled time queues the draft instead
                // of publishing. Group posts can't be scheduled (membership is
                // checked at publish time), so the group composer drops the field.
                @if group.is_none() {
                    fieldset.compose__section.compose__type-fields data-compose-section="schedule"
                        data-compose-schedulable hidden[kind == "event"] disabled[kind == "event"] {
                        span.compose__legend { (locale.text("compose-schedule")) }
                        div.compose__schedule-body {
                            label.compose__inline {
                                span { (locale.text("compose-publish-at")) }
                                input type="datetime-local" name="scheduled_at"
                                    value=(prefill.scheduled_at);
                            }
                            span.compose__hint {
                                (schedule_help) " "
                                a href="/settings/scheduled" {
                                    (locale.text("compose-scheduled-posts"))
                                }
                            }
                        }
                    }
                }

                div.compose__toolbar {
                    div.compose__tools {
                        (compose_toggle("media", "upload", &locale.text("compose-attach-media")))
                        span data-compose-note-only hidden[kind != "note"] {
                            (compose_toggle("poll", "poll", &locale.text("compose-poll")))
                        }
                        @if group.is_none() {
                            span data-compose-schedulable hidden[kind == "event"] {
                                (compose_toggle("schedule", "calendar", &locale.text("compose-schedule-later")))
                            }
                        }
                        (compose_toggle("cw", "alert", &locale.text("compose-content-warning")))
                        (compose_emoji_picker(locale))
                        // Groups only relay public posts and a direct-thread reply
                        // stays direct, so both lock the visibility selector away
                        // (a hidden field is emitted at the top instead).
                        @if group.is_none() && direct.is_none() {
                            (compose_menu("visibility", &locale.text("compose-visibility"),
                                defaults.visibility, &VISIBILITY_LEVELS, locale))
                        }
                        (compose_menu("quote_policy", &locale.text("compose-quote-policy"),
                            defaults.quote_policy, &QUOTE_POLICIES, locale))
                        (compose_menu("content_type", &locale.text("compose-text-format"),
                            defaults.content_type, &POST_FORMATS, locale))
                        (language_combo("language", &locale.text("compose-post-language"),
                            defaults.languages, defaults.language, locale))
                    }
                    div.compose__actions {
                        span.compose__count data-compose-count aria-hidden="true" {}
                        // Preview is first, so an accidental Enter previews (safe)
                        // rather than posts; both carry an explicit `op`.
                        button.compose__preview-btn type="submit" name="op" value="preview"
                            formnovalidate data-compose-preview-btn {
                                (locale.text("compose-preview"))
                            }
                        button.compose__publish type="submit" name="op" value="post" {
                            @if group.is_some() { (locale.text("compose-post-to-group")) }
                            @else if reply.is_some() { (locale.text("compose-reply")) }
                            @else { (locale.text("compose-publish")) }
                        }
                    }
                }
            }
        }
        // The rendered post, shown below the composer after a preview. Always
        // present so the JS enhancement has a stable target to fill in place.
        section.compose__preview data-compose-preview {
            @if let Some(preview) = &prefill.preview {
                h2.compose__preview-heading { (locale.text("compose-preview")) }
                (preview)
            }
        }
    }
}

/// The composer in edit mode: the same shell as [`full_compose_form`]
/// prefilled from the post's raw source, submitting to
/// `POST /web/statuses/{id}/edit`. What an edit cannot change stays out of the
/// form: visibility is a static chip (Mastodon locks it after posting) and no
/// new media or polls can be added — attachments are kept/removed via
/// checkboxes, each with an editable alt text.
pub struct EditComposePrefill<'a> {
    pub text: &'a str,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub content_type: &'a str,
    pub language: &'a str,
    pub quote_policy: &'a str,
    pub media: &'a [Value],
    pub preview: Option<Markup>,
    pub error: Option<&'a str>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one flat edit form mirroring the full composer"
)]
pub fn edit_compose_form(
    csrf: &str,
    status: &Status,
    languages: &[&'static Language],
    limits: ComposeLimits,
    prefill: &EditComposePrefill<'_>,
    locale: Locale,
) -> Markup {
    let visibility_locked = visibility_meta(status.visibility()).map(|(_, fallback)| {
        let label_id = match status.visibility() {
            "public" => "visibility-public",
            "unlisted" => "visibility-unlisted",
            "private" => "visibility-private",
            "direct" => "visibility-direct",
            "local" => "visibility-local",
            _ => return fallback.to_owned(),
        };
        let mut args = FluentArgs::new();
        args.set("visibility", locale.text(label_id));
        locale.text_with("compose-visibility-locked", &args)
    });
    html! {
        form.compose.compose--full.compose--edit data-compose
            data-compose-preview-urlencoded
            data-i18n-preview-failed=(locale.text("compose-preview-failed")) method="post"
            action=(format!("/web/statuses/{}/edit", status.id())) {
            input type="hidden" name="csrf" value=(csrf);

            @if let Some(error) = prefill.error {
                p.compose__error role="alert" { (error) }
            }

            div.compose__section data-compose-section="cw" {
                label.compose__field {
                    span.compose__legend { (locale.text("compose-content-warning")) }
                    input type="text" name="spoiler_text" data-compose-spoiler
                        value=(prefill.spoiler_text)
                        placeholder=(locale.text("compose-content-warning-placeholder"));
                }
            }

            label.compose__field.compose__text {
                span.visually-hidden { (locale.text("compose-post-text")) }
                textarea name="status" rows="5" data-autogrow
                    maxlength=(limits.textarea_maxlength())
                    data-max-chars=(limits.max_characters) { (prefill.text) }
            }

            @if !prefill.media.is_empty() {
                // Not a `compose__section`: those collapse behind toolbar
                // toggles under the `.js` root, and the kept attachments must
                // always stay visible.
                div.compose__edit-media {
                    span.compose__legend { (locale.text("compose-attachments")) }
                    @for item in prefill.media {
                        (attachment_keep_row(item, locale))
                    }
                    label.compose__inline {
                        input type="hidden" name="sensitive" value="false";
                        input type="checkbox" name="sensitive" value="true"
                            checked[prefill.sensitive];
                        span { (locale.text("compose-sensitive")) }
                    }
                }
            }

            div.compose__toolbar {
                div.compose__tools {
                    (compose_toggle("cw", "alert", &locale.text("compose-content-warning")))
                    (compose_emoji_picker(locale))
                    @if let Some((glyph, _)) = visibility_meta(status.visibility()) {
                        span.compose__static-vis title=(visibility_locked.as_deref().unwrap_or_default()) {
                            (icon(glyph))
                        }
                    }
                    @if status.boostable() {
                        (compose_menu("quote_policy", &locale.text("compose-quote-policy"),
                            prefill.quote_policy, &QUOTE_POLICIES, locale))
                    }
                    (compose_menu("content_type", &locale.text("compose-text-format"),
                        prefill.content_type, &POST_FORMATS, locale))
                    (language_combo("language", &locale.text("compose-post-language"),
                        languages, prefill.language, locale))
                }
                div.compose__actions {
                    span.compose__count data-compose-count aria-hidden="true" {}
                    button.compose__preview-btn type="submit" name="op" value="preview"
                        formnovalidate data-compose-preview-btn {
                            (locale.text("compose-preview"))
                        }
                    button.compose__publish type="submit" name="op" value="save" {
                        (locale.text("compose-save-changes"))
                    }
                }
            }
        }
        section.compose__preview data-compose-preview {
            @if let Some(preview) = &prefill.preview {
                h2.compose__preview-heading { (locale.text("compose-preview")) }
                (preview)
            }
        }
    }
}

/// The edit-history page's version list: the API's `StatusEdit`
/// entities rendered newest-first as compact read-only cards. An edit
/// snapshot carries no mentions/tags arrays, so its content gets the emoji
/// pass only — links inside stay as the sanitised HTML shipped them.
pub fn edit_history(versions: &[Value], clock: &ViewerClock, locale: Locale) -> Markup {
    let field = |v: &Value, key: &str| -> String {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    html! {
        div.history {
            @for (index, version) in versions.iter().rev().enumerate() {
                @let emojis = version
                    .get("emojis")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                @let spoiler = field(version, "spoiler_text");
                @let when = field(version, "created_at");
                article.history__version {
                    header.history__head {
                        span.history__label {
                            @if index == 0 { (locale.text("history-most-recent")) }
                            @else if index == versions.len() - 1 {
                                (locale.text("history-original"))
                            }
                            @else { (locale.text("history-edited")) }
                        }
                        (clock.element_absolute_iso(&when))
                    }
                    @if !spoiler.is_empty() {
                        p.history__spoiler { (emojify_text(&spoiler, emojis)) }
                    }
                    div.status__content { (emojify(&field(version, "content"), emojis)) }
                    @let media = version
                        .get("media_attachments")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    @if !media.is_empty() {
                        div.history__media {
                            @for item in media {
                                @let alt = field(item, "description");
                                @let preview = item
                                    .get("preview_url")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty());
                                @match preview {
                                    Some(preview) => {
                                        img.history__thumb src=(preview) alt=(alt) loading="lazy";
                                    }
                                    None => span.history__thumb.history__thumb--file { (icon("upload")) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One entry in a [`tab_strip`].
pub struct Tab<'a> {
    pub href: &'a str,
    pub label: &'a str,
    pub active: bool,
}

impl<'a> Tab<'a> {
    #[must_use]
    pub fn new(href: &'a str, label: &'a str, active: bool) -> Self {
        Self {
            href,
            label,
            active,
        }
    }
}

/// A reusable section selector: a `<details>` disclosure whose summary shows
/// the current section and opens a vertical list of the rest. It replaces the
/// old horizontally-scrolling tab strip, which was awkward on both desktop and
/// touch and could push the page wider than the viewport. One control serves
/// the two-item timeline switch and the twenty-item admin nav alike, never
/// scrolls sideways, and works with no JavaScript (the browser opens/closes the
/// disclosure; `app.js` adds outside-click / Escape closing and viewport-aware
/// placement, reusing the status-menu machinery).
pub fn tab_strip(aria_label: &str, tabs: &[Tab]) -> Markup {
    let current = tabs
        .iter()
        .find(|tab| tab.active)
        .map_or("", |tab| tab.label);
    html! {
        nav.nav-select aria-label=(aria_label) {
            details.nav-select__menu data-nav-select {
                summary.nav-select__current aria-label=(aria_label) {
                    span.nav-select__value { (current) }
                    span.nav-select__caret { (icon("chevron")) }
                }
                div.nav-select__pop {
                    @for tab in tabs {
                        a.nav-select__option.is-active[tab.active] href=(tab.href)
                            aria-current=(if tab.active { "page" } else { "false" }) {
                            span.nav-select__option-label { (tab.label) }
                            span.nav-select__option-check { (icon("check")) }
                        }
                    }
                }
            }
        }
    }
}

/// The one data-table shell, shared by every tabular surface in settings and
/// the admin console. `rows` is the table's own content (`thead`/`tbody`),
/// which differs per page; the chrome around it does not.
///
/// The wrapper is what resolves the width invariant for tables: it is a
/// horizontal scroll container, so a table wider than the column keeps its
/// natural column widths and scrolls, rather than being squeezed until cell
/// text wraps one letter per line (see the `overflow-wrap` note on
/// `.admin-table` in app.css). Local-attachment gradients on the wrapper fade
/// and shadow whichever edge still has content off-screen, so a scrollable
/// table announces itself with no JavaScript.
pub fn data_table(rows: &Markup) -> Markup {
    html! {
        div.table-scroll {
            table.admin-table {
                (rows)
            }
        }
    }
}

/// The pinned posts leading a profile: each is the normal card under
/// a "Pinned" marker line, mirroring the boost-attribution treatment. Renders
/// nothing when the account has no pins.
pub fn pinned_feed(statuses: &[Value], ctx: &Ctx) -> Markup {
    html! {
        @for value in statuses {
            @let status = Status(value);
            @let verdict = filter_verdict(&status, ctx, false);
            @if !verdict.hide {
                article.status data-kind="pinned" {
                    p.status__boost { (icon("pin")) " " (ctx.locale.text("profile-pinned")) }
                    (status_main(&status, ctx, false, &verdict, true))
                }
            }
        }
    }
}

/// Renders a list of status values as a feed, with an empty-state fallback.
pub fn feed(statuses: &[Value], ctx: &Ctx) -> Markup {
    if statuses.is_empty() {
        return html! { p.empty { (ctx.locale.text("page-nothing-here")) } };
    }
    html! {
        // `data-paged` marks the container infinite scroll appends into.
        div.feed data-paged {
            @for value in statuses {
                (status_card(&Status(value), ctx))
            }
        }
    }
}

/// [`feed`] over a thread-grouped card list: the same container and the
/// same cards, with the exchanges the page happens to carry drawn together.
///
/// The container stays `div.feed data-paged` and a group is one of its
/// children, so infinite scroll (which appends the incoming document's
/// `[data-paged]` children wholesale) keeps working without knowing groups
/// exist.
pub fn threaded_feed(cards: &[thread::Card], ctx: &Ctx) -> Markup {
    if cards.is_empty() {
        return html! { p.empty { (ctx.locale.text("page-nothing-here")) } };
    }
    html! {
        div.feed data-paged {
            @for card in cards {
                @match card {
                    thread::Card::Single(value) => (status_card(&Status(value), ctx)),
                    thread::Card::Group(group) => (thread_group(group, ctx)),
                }
            }
        }
    }
}

/// One exchange, parent first. A long one keeps its ends and folds everything
/// between them into a disclosure — a `<details>`, so it opens with JavaScript
/// off like every other disclosure in this UI.
///
/// `role="group"` with a name is the whole accessibility story here: the visual
/// grouping is a rail drawn in CSS, which a screen reader cannot see, and the
/// name is the one thing that says *why* these cards are together.
fn thread_group(group: &thread::Group, ctx: &Ctx) -> Markup {
    let label = ctx.locale.text(match group.kind {
        thread::Kind::Thread => "thread-group-thread",
        thread::Kind::Conversation => "thread-group-conversation",
    });
    let (first, middle, last) = group.shown();
    let folded = group.folded();
    let mut args = FluentArgs::new();
    args.set("count", i64::try_from(folded.len()).unwrap_or(i64::MAX));
    html! {
        div.thread-group.thread-group--conversation[group.kind == thread::Kind::Conversation]
            role="group" aria-label=(label) {
            (status_card(&Status(first), ctx))
            @for value in middle { (status_card(&Status(value), ctx)) }
            @if !folded.is_empty() {
                details.thread-group__folded {
                    summary { (ctx.locale.text_with("thread-group-folded", &args)) }
                    @for value in folded { (status_card(&Status(value), ctx)) }
                }
            }
            (status_card(&Status(last), ctx))
        }
    }
}

/// The profile Media tab as a packed gallery: every attachment of every media
/// post, one square tile each, instead of the posts themselves. A tile links
/// to the containing post (the no-JS path from a picture to its context); JS
/// upgrades image/gifv clicks into the lightbox, whose "View post" link keeps
/// that path. Sensitive posts (and blur/warn filter matches) keep their tiles
/// but blur the thumbnails, honoring the viewer's media-display preference.
pub fn media_wall(statuses: &[Value], ctx: &Ctx) -> Markup {
    let mut tiles: Vec<Markup> = Vec::new();
    for value in statuses {
        let status = Status(value);
        let verdict = filter_verdict(&status, ctx, false);
        if verdict.hide {
            continue;
        }
        let proper = status.reblog().unwrap_or(Status(value));
        let gated = match ctx.prefs.expand_media {
            MediaDisplay::ShowAll => false,
            MediaDisplay::HideAll => true,
            MediaDisplay::Default => proper.sensitive(),
        } || !verdict.blur.is_empty()
            || !verdict.warn.is_empty();
        let permalink = status.permalink_anchored();
        for item in proper.media() {
            tiles.push(media_wall_tile(item, &permalink, gated));
        }
    }
    if tiles.is_empty() {
        return html! { p.empty { (ctx.locale.text("page-nothing-here")) } };
    }
    html! {
        div.media-wall data-paged {
            @for tile in &tiles { (tile) }
        }
    }
}

/// One media-wall tile. The link always targets the post; for images and gifv
/// the media file rides along in `data-media-url` so the lightbox can show it
/// in place. Video/audio/unknown attachments stay plain post links (the
/// lightbox has no player for them) with a kind badge saying so.
fn media_wall_tile(item: &Value, permalink: &str, gated: bool) -> Markup {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
    let preview = item
        .get("preview_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        // Audio has no visual: never fall back to the audio file as an <img>.
        .or_else(|| Some(url).filter(|u| !u.is_empty() && matches!(kind, "image" | "gifv")))
        .filter(|s| !s.is_empty());
    let alt = item
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let blurhash = item
        .get("blurhash")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (width, height) = preview_dimensions(item);
    let in_lightbox = matches!(kind, "image" | "gifv") && !url.is_empty();
    html! {
        a.media-wall__tile.is-sensitive[gated] href=(permalink)
            data-media-url=[in_lightbox.then_some(url)]
            data-gifv[kind == "gifv"] {
            @match preview {
                Some(preview) => {
                    img src=(preview) alt=(alt) loading="lazy" width=[width] height=[height]
                        data-blurhash=[blurhash];
                }
                None => span.media-wall__placeholder { (icon("upload")) }
            }
            @if gated || kind != "image" || !alt.is_empty() {
                div.media__badges {
                    @if gated { span.media__badge { "Sensitive" } }
                    @match kind {
                        "gifv" => { span.media__badge { "GIF" } }
                        "video" => { span.media__badge { "Video" } }
                        "audio" => { span.media__badge { "Audio" } }
                        _ => {}
                    }
                    @if !alt.is_empty() { span.media__badge title=(alt) { "ALT" } }
                }
            }
        }
    }
}

/// A compact account row (avatar, name, handle) linking to the profile — used
/// in search results and in account-only notifications.
pub fn account_card(account: &Account) -> Markup {
    html! {
        a.account-card href=(account.profile_path()) {
            img.account-card__avatar src=(account.avatar()) alt="" width="40" height="40" loading="lazy";
            span.account-card__body {
                span.account-card__name { (account.name_markup()) }
                span.account-card__acct { (account.handle_prefix()) (account.acct()) }
            }
        }
    }
}

/// The human-readable action phrase for a notification kind.
fn notification_label(kind: &str) -> &'static str {
    match kind {
        "follow" => "followed you",
        "follow_request" => "requested to follow you",
        "favourite" => "favourited your post",
        "reblog" => "boosted your post",
        "mention" => "mentioned you",
        "status" => "posted",
        "live" => "is live now",
        "poll" => "ran a poll that ended",
        "update" => "edited a post",
        "quote" => "quoted your post",
        "pleroma:emoji_reaction" => "reacted to your post",
        "admin.sign_up" => "signed up",
        "admin.report" => "filed a report",
        "severed_relationships" => "relationships were severed",
        "moderation_warning" => "received a moderation warning",
        // Event participation (E-track). Without these the row reads "sent you a
        // notification", which tells the reader nothing about what to do next.
        "event.participation" => "responded to your event",
        "event.accepted" => "confirmed your attendance",
        "event.rejected" => "declined your attendance",
        "event.changed" => "changed an event you're attending",
        "event.invite" => "invited you to an event",
        _ => "sent you a notification",
    }
}

/// One notification entity (`entities::render_notifications`): an actor line
/// plus, when the notification carries one, the related status card. Pure
/// account notifications (follow / follow request) show the account card.
pub fn notification_item(note: &Value, ctx: &Ctx) -> Markup {
    let kind = note.get("type").and_then(Value::as_str).unwrap_or_default();
    let actor = Account(&note["account"]);
    let status = note.get("status").filter(|s| !s.is_null());
    // A hide-filter match on the related status drops the whole notification,
    // like Mastodon — an actor line over a vanished card helps no one.
    if let Some(value) = status
        && filter_verdict(&Status(value), ctx, false).hide
    {
        return html! {};
    }
    // A moderation warning has no meaningful actor (the sender is the
    // recipient themselves; the moderator stays hidden): render the strike
    // text and the path to the strikes page instead of an actor line.
    if kind == "moderation_warning" {
        let text = note
            .pointer("/moderation_warning/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return html! {
            article.notification data-kind=(kind) {
                p.notification__label {
                    span.notification__icon { (icon(notification_icon(kind))) }
                    "Your account has received a moderation warning"
                }
                @if !text.is_empty() {
                    p.notification__warning-text { (text) }
                }
                p.notification__warning-link {
                    a href="/settings/strikes" { "Review the action and appeal" }
                }
            }
        };
    }
    // Pleroma emoji reactions carry the reacted emoji; show it inline rather
    // than collapsing to the generic phrase. A custom emoji brings its image
    // as `emoji_url` — render that instead of the literal `:shortcode:`.
    let emoji = note
        .get("emoji")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let emoji_url = note
        .get("emoji_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    html! {
        article.notification data-kind=(kind) {
            p.notification__label {
                span.notification__icon { (icon(notification_icon(kind))) }
                a.notification__actor href=(actor.profile_path()) { (actor.name_markup()) }
                " " (notification_label(kind))
                @if kind == "pleroma:emoji_reaction" && !emoji.is_empty() {
                    " " span.notification__emoji {
                        @if emoji_url.is_empty() { (emoji) }
                        @else { (PreEscaped(emoji_img(emoji.trim_matches(':'), emoji_url))) }
                    }
                }
            }
            @if let Some(status) = status {
                (status_card(&Status(status), ctx))
            } @else {
                (account_card(&actor))
                // An incoming follow request is actionable — point at the
                // queue where accept/reject live.
                @if kind == "follow_request" {
                    p.notification__warning-link {
                        a href="/settings/relationships?rel=requests" {
                            "Review follow requests"
                        }
                    }
                }
            }
        }
    }
}

/// One row of the Private-mentions inbox: the participant set, unread state,
/// and the latest message as an ordinary status card. The card carries its
/// own links, forms and menus, so the row is an article with explicit
/// affordances — one big anchor around it would nest interactive content
/// (invalid HTML) and break the card's disclosures.
pub fn conversation_row(conversation: &Value, ctx: &Ctx) -> Markup {
    let row_id = conversation["id"].as_str().unwrap_or_default();
    let unread = conversation["unread"].as_bool().unwrap_or(false);
    let mut accounts: Vec<Account> = conversation["accounts"]
        .as_array()
        .map(|list| list.iter().map(Account).collect())
        .unwrap_or_default();
    let last_status = conversation
        .get("last_status")
        .filter(|s| !s.is_null())
        .map(Status);
    // The latest sender leads the avatar stack. They may be absent — the
    // viewer's own reply is last — in which case the stored order stands.
    if let Some(status) = &last_status
        && let Some(lead) = accounts
            .iter()
            .position(|a| a.id() == status.account().id())
        && lead > 0
    {
        let account = accounts.remove(lead);
        accounts.insert(0, account);
    }
    // "A", "A and B", "A, B and N more" — the Tusky recipe.
    let extra = accounts.len().saturating_sub(2);
    let names = html! {
        @for (i, account) in accounts.iter().take(2).enumerate() {
            @if i > 0 { @if extra == 0 { " and " } @else { ", " } }
            a.conversation__participant href=(account.profile_path()) { (account.name_markup()) }
        }
        @if extra > 0 { " and " (extra) " more" }
    };
    html! {
        article.conversation.is-unread[unread] data-conversation-id=(row_id) {
            header.conversation__header {
                div.conversation__avatars aria-hidden="true" {
                    @for account in accounts.iter().take(4) {
                        img.conversation__avatar src=(account.avatar()) alt="" loading="lazy";
                    }
                }
                p.conversation__with {
                    @if unread {
                        span.conversation__dot aria-hidden="true" {}
                        span.visually-hidden { "Unread: " }
                    }
                    "With " (names)
                }
                @if let Some(csrf) = ctx.csrf {
                    details.status__menu.conversation__menu data-status-menu {
                        summary.action title="Conversation options" { (icon("more")) }
                        div.status__menu-pop role="menu" {
                            @if unread {
                                (menu_form(&format!("/web/conversations/{row_id}/read"),
                                    "Mark as read", false, None, csrf, ctx.return_to))
                            } @else {
                                (menu_form(&format!("/web/conversations/{row_id}/unread"),
                                    "Mark as unread", false, None, csrf, ctx.return_to))
                            }
                            (menu_form(&format!("/web/conversations/{row_id}/remove"),
                                "Remove from inbox", true,
                                Some("Hide this conversation from your inbox? A new message brings it back."),
                                csrf, ctx.return_to))
                        }
                    }
                }
            }
            @if let Some(status) = &last_status {
                (status_card(status, ctx))
                nav.conversation__actions {
                    a.conversation__open href=(status.permalink_anchored()) { "Open conversation" }
                    a.conversation__reply
                        href=(format!("/compose?reply={}&visibility=direct", status.id())) {
                        "Reply"
                    }
                }
            } @else {
                // Never drop the row: a vanished last message must not orphan
                // the thread (the Tusky/Phanpy mistake).
                p.empty { "The latest message in this conversation was deleted." }
            }
        }
    }
}

/// The icon paired with a notification kind.
fn notification_icon(kind: &str) -> &'static str {
    match kind {
        "favourite" | "pleroma:emoji_reaction" => "favourite",
        "reblog" => "boost",
        "quote" => "quote",
        "follow" | "follow_request" | "admin.sign_up" => "profile",
        "admin.report" | "moderation_warning" | "severed_relationships" => "shield",
        "event.participation"
        | "event.accepted"
        | "event.rejected"
        | "event.changed"
        | "event.invite" => "calendar",
        _ => "bell",
    }
}

/// The search box. Pre-fills `query` so it survives a results page reload.
pub fn search_form(query: &str, locale: super::i18n::Locale) -> Markup {
    html! {
        form.search method="get" action="/search" role="search" {
            label.visually-hidden for="search-q" { (locale.text("page-search")) }
            input #search-q type="search" name="q" value=(query)
                placeholder=(locale.text("page-search-placeholder")) autofocus;
            button type="submit" { (locale.text("page-search")) }
        }
    }
}

/// A list of hashtag results, each linking to its tag timeline.
pub fn hashtag_list(tags: &[String]) -> Markup {
    html! {
        ul.hashtag-list {
            @for name in tags {
                li { a.hashtag href={ "/tags/" (name) } { "#" (name) } }
            }
        }
    }
}

/// A link to session details; rendering never resolves or executes the app.
pub fn webxdc_invitation_card(invitation: Option<&Value>) -> Markup {
    let Some(invitation) = invitation.filter(|v| v.is_object()) else {
        return html! {};
    };
    let name = invitation["name"].as_str().unwrap_or("Shared app");
    let href = invitation["open_url"].as_str().unwrap_or("/webxdc/open");
    html! {
        a.webxdc-invitation href=(href) {
            span.webxdc-invitation__icon aria-hidden="true" { (icon("apps")) }
            span { strong { (name) } span.webxdc-muted { "App invitation · View session" } }
            span.webxdc-invitation__arrow aria-hidden="true" { "→" }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn party() -> Vec<Value> {
        vec![json!({
            "shortcode": "party",
            "url": "https://plamenu.test/media/7.png",
            "static_url": "https://plamenu.test/media/7.png",
            "visible_in_picker": true,
        })]
    }

    const PARTY_IMG: &str = "<img class=\"emoji\" src=\"https://plamenu.test/media/7.png\" \
                             alt=\":party:\" title=\":party:\" draggable=\"false\" loading=\"lazy\">";

    #[test]
    fn emojify_swaps_references_in_text_nodes() {
        assert_eq!(
            emojify("<p>time to :party: hard</p>", &party()).into_string(),
            format!("<p>time to {PARTY_IMG} hard</p>")
        );
    }

    #[test]
    fn emojify_leaves_tags_and_unknown_codes_alone() {
        // A reference inside an attribute value (a URL path, say) is markup,
        // not text — rewriting it would corrupt the tag.
        let html = "<a href=\"https://x.example/:party:\">:party: :other:</a>";
        assert_eq!(
            emojify(html, &party()).into_string(),
            format!("<a href=\"https://x.example/:party:\">{PARTY_IMG} :other:</a>")
        );
        // No emoji on the entity: the markup passes through untouched.
        assert_eq!(emojify(html, &[]).into_string(), html);
    }

    #[test]
    fn emojify_text_escapes_before_swapping() {
        assert_eq!(
            emojify_text("a <b> :party:", &party()).into_string(),
            format!("a &lt;b&gt; {PARTY_IMG}")
        );
    }

    #[test]
    fn hls_video_keeps_a_script_blocker_safe_progressive_source() {
        let value = json!({
            "media_attachments": [{
                "type": "video",
                "url": "https://plamenu.test/media/play/7/video.mp4",
                "preview_url": "https://plamenu.test/media/preview/7",
                "description": "A long video",
                "hls": { "master": "https://plamenu.test/media/hls/7/master.m3u8" },
            }],
        });
        let rendered = media_gallery(&Status(&value), &view_ctx(None, None)).into_string();
        assert!(
            rendered.contains(r#"src="https://plamenu.test/media/play/7/video.mp4""#),
            "baseline video must be playable when script execution is blocked: {rendered}"
        );
        assert!(
            rendered.contains(r#"data-hls="https://plamenu.test/media/hls/7/master.m3u8""#),
            "JS can still upgrade the same element to HLS: {rendered}"
        );
        assert!(!rendered.contains("<noscript>"), "{rendered}");
    }

    fn bob_mention() -> Vec<Value> {
        vec![json!({
            "id": "17",
            "username": "bob",
            "acct": "bob@remote.example",
            "url": "https://remote.example/@bob",
        })]
    }

    fn rust_tag() -> Vec<Value> {
        vec![json!({
            "name": "rust",
            "url": "https://plamenu.test/tags/rust",
        })]
    }

    #[test]
    fn mention_anchors_are_rewritten_in_app() {
        let html = r#"<p><span class="h-card"><a href="https://remote.example/@bob" class="u-url mention">@<span>bob</span></a></span> hi</p>"#;
        assert_eq!(
            rewrite_content_links(html, &bob_mention(), &[], false),
            r#"<p><span class="h-card"><a href="/@bob@remote.example" class="u-url mention">@<span>bob</span></a></span> hi</p>"#
        );
    }

    #[test]
    fn ap_id_mention_anchors_are_rewritten_in_app() {
        // Pleroma links mentions to the AP id (`/users/name`), not the
        // profile page the mention entity carries.
        let html = r#"<a href="https://remote.example/users/bob" class="u-url mention">@bob</a>"#;
        assert_eq!(
            rewrite_content_links(html, &bob_mention(), &[], false),
            r#"<a href="/@bob@remote.example" class="u-url mention">@bob</a>"#
        );
        // But a lookalike path elsewhere on the site stays external.
        let other = r#"<a href="https://remote.example/blog/users/bob">x</a>"#;
        assert_eq!(
            rewrite_content_links(other, &bob_mention(), &[], false),
            r#"<a href="https://remote.example/blog/users/bob" target="_blank" rel="nofollow noopener noreferrer">x</a>"#
        );
    }

    #[test]
    fn hashtag_anchors_are_rewritten_in_app() {
        // Mastodon's remote shape: origin-server href, display casing.
        let mastodon = r#"<a href="https://remote.example/tags/Rust" class="mention hashtag" rel="tag">#<span>Rust</span></a>"#;
        assert_eq!(
            rewrite_content_links(mastodon, &[], &rust_tag(), false),
            r#"<a href="/tags/rust" class="mention hashtag" rel="tag">#<span>Rust</span></a>"#
        );
        // Pleroma uses a singular `/tag/` path.
        let pleroma = r#"<a href="https://pleroma.example/tag/rust">#rust</a>"#;
        assert_eq!(
            rewrite_content_links(pleroma, &[], &rust_tag(), false),
            r#"<a href="/tags/rust">#rust</a>"#
        );
        // A tag the entity doesn't carry stays external.
        let unknown = rewrite_content_links(
            r#"<a href="https://remote.example/tags/other">#other</a>"#,
            &[],
            &rust_tag(),
            false,
        );
        assert!(unknown.contains(r#"href="https://remote.example/tags/other""#));
        assert!(unknown.contains(r#"target="_blank""#));
    }

    #[test]
    fn external_links_open_in_a_new_tab() {
        // Remote sanitised HTML: ammonia sets `rel` but never `target`.
        assert_eq!(
            rewrite_content_links(
                r#"<p><a href="https://news.example/story" rel="nofollow noopener noreferrer">read</a></p>"#,
                &[],
                &[],
                false,
            ),
            r#"<p><a href="https://news.example/story" rel="nofollow noopener noreferrer" target="_blank">read</a></p>"#
        );
        // A bare anchor gains both.
        assert_eq!(
            rewrite_content_links(r#"<a href="https://x.example/y">y</a>"#, &[], &[], false),
            r#"<a href="https://x.example/y" target="_blank" rel="nofollow noopener noreferrer">y</a>"#
        );
        // Locally-composed anchors already carry a target; it isn't doubled.
        let local = rewrite_content_links(
            r#"<a href="https://x.example/y" target="_blank" rel="nofollow noopener">y</a>"#,
            &[],
            &[],
            false,
        );
        assert_eq!(local.matches("target=").count(), 1);
        assert!(local.contains(r#"rel="nofollow noopener""#));
    }

    #[test]
    fn signed_in_external_links_route_through_the_resolver() {
        // With `wrap_external`, a plain external link points at `/web/go` so a
        // click can land on our local copy of a federated actor/post; the
        // original URL rides in the query string and the tab still opens.
        let wrapped = rewrite_content_links(
            r#"<a href="https://lemmy.world/post/123">post</a>"#,
            &[],
            &[],
            true,
        );
        assert!(
            wrapped.contains(r#"href="/web/go?url=https%3A%2F%2Flemmy.world%2Fpost%2F123""#),
            "external link should route through the resolver: {wrapped}"
        );
        assert!(wrapped.contains(r#"target="_blank""#));
        // A mention still wins over the resolver wrap (stays a direct in-app
        // link, no new tab).
        let mention = r#"<a href="https://remote.example/@bob" class="u-url mention">@bob</a>"#;
        let out = rewrite_content_links(mention, &bob_mention(), &[], true);
        assert!(out.contains(r#"href="/@bob@remote.example""#));
        assert!(!out.contains("/web/go"));
        assert!(!out.contains("target="));
        // Non-http(s) links (mailto, fragments) are never wrapped.
        let mail = rewrite_content_links(r#"<a href="mailto:x@y.z">mail</a>"#, &[], &[], true);
        assert!(!mail.contains("/web/go"));
    }

    #[test]
    fn escaped_hrefs_still_match_mentions() {
        let mentions = vec![json!({
            "id": "17",
            "username": "bob",
            "acct": "bob@remote.example",
            "url": "https://remote.example/@bob?x=1&y=2",
        })];
        let html =
            r#"<a href="https://remote.example/@bob?x=1&amp;y=2" class="u-url mention">@bob</a>"#;
        assert_eq!(
            rewrite_content_links(html, &mentions, &[], false),
            r#"<a href="/@bob@remote.example" class="u-url mention">@bob</a>"#
        );
    }

    #[test]
    fn non_anchor_markup_passes_through_the_link_pass() {
        let html = "<p>no links <b>here</b></p>";
        assert_eq!(rewrite_content_links(html, &[], &[], false), html);
        // An anchor with no attributes (nothing to rewrite) is left alone.
        assert_eq!(
            rewrite_content_links("<a>x</a>", &[], &[], false),
            "<a>x</a>"
        );
    }

    // ---- Filter verdicts -------------------------------------------------

    fn filtered_status(action: &str, contexts: &[&str]) -> Value {
        json!({
            "id": "1",
            "created_at": "2026-07-04T12:00:00Z",
            "visibility": "public",
            "content": "<p>verboten wares</p>",
            "account": { "id": "9", "acct": "bob", "display_name": "Bob",
                         "avatar": "/a.png", "url": "https://plamenu.test/@bob" },
            "media_attachments": [],
            "filtered": [{
                "filter": {
                    "id": "5",
                    "title": "Bad words",
                    "context": contexts,
                    "expires_at": null,
                    "filter_action": action,
                },
                "keyword_matches": ["verboten"],
                "status_matches": [],
            }],
        })
    }

    fn view_ctx(filter_context: Option<FilterContext>, viewer_id: Option<&str>) -> Ctx<'_> {
        Ctx {
            csrf: None,
            viewer_id,
            return_to: "/",
            filter_context,
            prefs: ViewPrefs::default(),
            locale: Locale::default(),
            clock: ViewerClock::utc(Locale::default()),
            admin: AdminCapabilities::default(),
        }
    }

    #[test]
    fn warn_filter_collapses_only_in_its_context() {
        let value = filtered_status("warn", &["home"]);
        let status = Status(&value);
        let matching = filter_verdict(&status, &view_ctx(Some(FilterContext::Home), None), false);
        assert!(!matching.hide);
        assert_eq!(matching.warn, ["Bad words"]);

        // The same filter is inert on a page whose context it doesn't cover.
        let other = filter_verdict(&status, &view_ctx(Some(FilterContext::Public), None), false);
        assert!(other.warn.is_empty() && !other.hide);

        // And on surfaces with no filter context at all.
        let none = filter_verdict(&status, &view_ctx(None, None), false);
        assert!(none.warn.is_empty() && !none.hide);
    }

    #[test]
    fn hide_filter_drops_except_where_downgraded() {
        let value = filtered_status("hide", &["thread"]);
        let status = Status(&value);
        let ctx = view_ctx(Some(FilterContext::Thread), None);
        assert!(filter_verdict(&status, &ctx, false).hide);

        // The thread page's focused post downgrades hide to the warn bar.
        let focused = filter_verdict(&status, &ctx, true);
        assert!(!focused.hide);
        assert_eq!(focused.warn, ["Bad words"]);

        // Search results likewise never lose entries without a trace.
        let hide_public = filtered_status("hide", &["public"]);
        let search = filter_verdict(
            &Status(&hide_public),
            &view_ctx(Some(FilterContext::Search), None),
            false,
        );
        assert!(!search.hide);
        assert_eq!(search.warn, ["Bad words"]);
    }

    #[test]
    fn own_posts_are_never_filtered() {
        let value = filtered_status("hide", &["home"]);
        let verdict = filter_verdict(
            &Status(&value),
            &view_ctx(Some(FilterContext::Home), Some("9")),
            false,
        );
        assert!(!verdict.hide && verdict.warn.is_empty());
    }

    #[test]
    fn blur_filter_gates_media_not_the_post() {
        let value = filtered_status("blur", &["home"]);
        let verdict = filter_verdict(
            &Status(&value),
            &view_ctx(Some(FilterContext::Home), None),
            false,
        );
        assert!(!verdict.hide);
        assert!(verdict.warn.is_empty());
        assert_eq!(verdict.blur, ["Bad words"]);
    }

    #[test]
    fn boost_is_judged_by_its_target() {
        let mut wrapper = filtered_status("hide", &["home"]);
        // The booster is someone else; the target is the viewer's own post,
        // so the match is discarded (Mastodon checks the reblogged author).
        wrapper["reblog"] = filtered_status("hide", &["home"]);
        wrapper["reblog"]["account"]["id"] = json!("42");
        let verdict = filter_verdict(
            &Status(&wrapper),
            &view_ctx(Some(FilterContext::Home), Some("42")),
            false,
        );
        assert!(!verdict.hide);
    }

    #[test]
    fn warn_bar_and_hide_render_through_status_card() {
        let warn = filtered_status("warn", &["home"]);
        let ctx = view_ctx(Some(FilterContext::Home), None);
        // The filter title is a Fluent variable, so it renders wrapped in
        // bidi isolation marks; strip them before asserting on the label.
        let card = status_card(&Status(&warn), &ctx)
            .into_string()
            .replace(['\u{2068}', '\u{2069}'], "");
        assert!(card.contains("Filtered: Bad words"), "warn bar: {card}");
        assert!(card.contains("Show anyway"));
        assert!(card.contains("verboten wares"), "body stays reachable");

        let hidden = filtered_status("hide", &["home"]);
        assert_eq!(status_card(&Status(&hidden), &ctx).into_string(), "");
    }

    #[test]
    fn russian_locale_flows_through_shared_status_card_controls() {
        let value = json!({
            "id": "17",
            "created_at": "2026-07-04T12:00:00Z",
            "edited_at": "2026-07-04T13:00:00Z",
            "visibility": "public",
            "url": "https://plamenu.test/@alice/17",
            "content": "<p>Привет</p>",
            "sensitive": true,
            "in_reply_to_id": "12",
            "in_reply_to_account_id": "8",
            "mentions": [{"id": "8", "acct": "bob"}],
            "account": {
                "id": "9",
                "acct": "alice",
                "display_name": "Alice",
                "avatar": "/a.png",
                "url": "https://plamenu.test/@alice",
                "bot": true,
                "locked": true,
            },
            "media_attachments": [{
                "id": "1",
                "type": "image",
                "url": "/media/full.png",
                "preview_url": "/media/preview.png",
                "description": "описание",
            }],
            "poll": {
                "multiple": false,
                "expired": true,
                "voted": true,
                "votes_count": 5,
                "own_votes": [],
                "options": [
                    {"title": "Да", "votes_count": 3},
                    {"title": "Нет", "votes_count": 2},
                ],
            },
            "replies_count": 1,
            "reblogs_count": 2,
            "favourites_count": 3,
            "quote_approval": {
                "automatic": ["public"],
                "manual": [],
                "current_user": "automatic",
            },
        });
        let locale = Locale::negotiate(Some("ru"), None);
        let ctx = Ctx {
            csrf: Some("tok"),
            viewer_id: Some("1"),
            return_to: "/",
            filter_context: None,
            prefs: ViewPrefs::default(),
            locale,
            clock: ViewerClock::utc(Locale::default()),
            admin: AdminCapabilities::default(),
        };
        let card = status_card(&Status(&value), &ctx)
            .into_string()
            .replace(['\u{2068}', '\u{2069}'], "");

        for expected in [
            "Автоматизированный аккаунт",
            "Подписка требует одобрения",
            "изменено",
            "В ответ @bob",
            "Деликатный контент",
            "5 голосов",
            "Завершён",
            "Ответить",
            "Продвинуть",
            "Цитировать",
            "В избранное",
            "В закладки",
            "Другие действия",
            "Копировать ссылку",
        ] {
            assert!(card.contains(expected), "missing {expected:?} in {card}");
        }
        assert!(!card.contains("Sensitive content"));
        assert!(!card.contains("More options"));
    }

    // ---- Contextual privileged-tools menus ------------------------------

    fn account_entity(id: &str, acct: &str, group: bool) -> Value {
        json!({
            "id": id,
            "acct": acct,
            "username": acct.split('@').next().unwrap_or(acct),
            "display_name": "Target",
            "avatar": "/a.png",
            "url": "https://example.test/profile",
            "group": group,
        })
    }

    #[test]
    fn account_privileged_menu_follows_target_and_exact_capabilities() {
        let locale = Locale::default();
        let local_person = account_entity("9", "bob", false);
        assert!(
            account_privileged_menu(
                &Account(&local_person),
                AdminCapabilities::default(),
                None,
                locale,
            )
            .is_none()
        );

        let account_admin = AdminCapabilities {
            manage_users: true,
            ..AdminCapabilities::default()
        };
        let menu = account_privileged_menu(&Account(&local_person), account_admin, None, locale)
            .unwrap()
            .into_string();
        assert!(menu.contains("data-privileged-menu"), "{menu}");
        assert!(menu.contains(r#"href="/admin/accounts/9""#), "{menu}");
        assert!(!menu.contains("/admin/instances/"), "{menu}");

        let remote_person = account_entity("10", "carol@remote.example", false);
        let federation_admin = AdminCapabilities {
            manage_federation: true,
            ..AdminCapabilities::default()
        };
        let menu =
            account_privileged_menu(&Account(&remote_person), federation_admin, None, locale)
                .unwrap()
                .into_string();
        assert!(
            menu.contains(r#"href="/admin/instances/remote.example""#),
            "{menu}"
        );
        assert!(!menu.contains("/admin/accounts/10"), "{menu}");

        let mut emoji_profile = remote_person.clone();
        emoji_profile["emojis"] = json!([{
            "shortcode": "blobcat",
            "url": "/media/proxy/emoji/1",
        }]);
        let emoji_admin = AdminCapabilities {
            manage_custom_emojis: true,
            ..AdminCapabilities::default()
        };
        let menu = account_privileged_menu(&Account(&emoji_profile), emoji_admin, None, locale)
            .unwrap()
            .into_string();
        assert!(
            menu.contains(r#"href="/admin/custom-emojis/borrow/account/10""#),
            "{menu}"
        );
        assert!(!menu.contains("/admin/accounts/10"), "{menu}");

        let local_group = account_entity("11", "hiking", true);
        let group_admin = AdminCapabilities {
            manage_groups: true,
            ..AdminCapabilities::default()
        };
        let menu = account_privileged_menu(
            &Account(&local_group),
            group_admin,
            Some("/groups/11/manage"),
            locale,
        )
        .unwrap()
        .into_string();
        assert!(menu.contains(r#"href="/groups/11/manage""#), "{menu}");
        assert!(menu.contains(r#"href="/admin/groups/11""#), "{menu}");

        // A remote Group has no group-console record; with MANAGE_USERS it
        // deliberately falls back to the known account record.
        let remote_group = account_entity("12", "memes@remote.example", true);
        let menu = account_privileged_menu(&Account(&remote_group), account_admin, None, locale)
            .unwrap()
            .into_string();
        assert!(menu.contains(r#"href="/admin/accounts/12""#), "{menu}");
        assert!(!menu.contains("/admin/groups/12"), "{menu}");
    }

    // ---- Group moderation in the shield menu ---------------------------

    fn group_post_entity() -> Value {
        json!({
            "id": "42",
            "created_at": "2026-07-13T12:00:00Z",
            "visibility": "public",
            "content": "<p>trail report</p>",
            "url": "https://plamenu.test/@bob/42",
            "account": { "id": "9", "acct": "bob", "username": "bob",
                         "display_name": "Bob", "avatar": "/a.png",
                         "url": "https://plamenu.test/@bob" },
            "media_attachments": [],
        })
    }

    fn mod_ctx() -> Ctx<'static> {
        Ctx {
            csrf: Some("tok"),
            viewer_id: Some("1"),
            return_to: "/@hiking",
            filter_context: None,
            prefs: ViewPrefs::default(),
            locale: Locale::default(),
            clock: ViewerClock::utc(Locale::default()),
            admin: AdminCapabilities::default(),
        }
    }

    #[test]
    fn group_vote_controls_expose_synchronizable_state() {
        let mut value = group_post_entity();
        value["group_post"] = json!(true);
        value["favourites_count"] = json!(7);
        value["downvotes_count"] = json!(2);
        value["favourited"] = json!(true);
        let card = status_card(&Status(&value), &mod_ctx()).into_string();

        assert!(card.contains(r#"data-action="upvote""#), "{card}");
        assert!(
            card.contains(r#"action="/web/statuses/42/unupvote""#),
            "{card}"
        );
        assert!(card.contains(r#"data-action="downvote""#), "{card}");
        assert!(
            card.contains(r#"class="status__score" title="Score">5</span>"#),
            "{card}"
        );
        assert!(card.contains(r#"data-inactive-title="Upvote""#), "{card}");
        assert!(
            card.contains(r#"data-active-title="Remove upvote""#),
            "{card}"
        );
    }

    #[test]
    fn group_moderator_gets_contextual_menu_verbs() {
        // A fresh top-level post: remove, lock, and pin all offered, each as a
        // POST to the group action; pin/unpin and lock/unlock are single
        // contextual verbs, not both at once.
        let mut value = group_post_entity();
        inject_group_mod(&mut value, 7, false, false);
        let card = status_card(&Status(&value), &mod_ctx()).into_string();
        assert!(card.contains("data-privileged-menu"), "{card}");
        assert!(card.contains("Moderate"), "{card}");
        assert!(
            card.contains(r#"action="/web/groups/7/posts/42/remove""#),
            "{card}"
        );
        assert!(card.contains("Remove from group"));
        assert!(card.contains(r#"action="/web/groups/7/posts/42/lock""#));
        assert!(card.contains("Lock thread") && !card.contains("Unlock thread"));
        assert!(card.contains(r#"action="/web/groups/7/posts/42/pin""#));
        assert!(card.contains("Pin in group") && !card.contains("Unpin from group"));

        // Pinned + locked flips both to their inverse (single contextual verb).
        let mut pinned = group_post_entity();
        inject_group_mod(&mut pinned, 7, true, true);
        let card = status_card(&Status(&pinned), &mod_ctx()).into_string();
        assert!(
            card.contains(r#"action="/web/groups/7/posts/42/pin?unpin=1""#),
            "{card}"
        );
        assert!(card.contains("Unpin from group") && !card.contains("Pin in group"));
        assert!(card.contains(r#"action="/web/groups/7/posts/42/lock?unlock=1""#));
        assert!(card.contains("Unlock thread") && !card.contains("Lock thread"));
    }

    #[test]
    fn group_comment_has_no_pin() {
        // A reply (comment) can be removed or its thread locked, but not pinned.
        let mut reply = group_post_entity();
        reply["in_reply_to_id"] = json!("40");
        inject_group_mod(&mut reply, 7, false, false);
        let card = status_card(&Status(&reply), &mod_ctx()).into_string();
        assert!(card.contains("Remove from group"), "{card}");
        assert!(
            !card.contains("/posts/42/pin"),
            "no pin for a comment: {card}"
        );
    }

    #[test]
    fn group_mod_context_rides_the_boosted_object() {
        // A group announce carries the moderation context on the *reblogged*
        // post — what the card unwraps to and the menu acts on.
        let mut boost = json!({
            "id": "100",
            "created_at": "2026-07-13T12:00:00Z",
            "visibility": "public",
            "content": "",
            "media_attachments": [],
            "account": { "id": "7", "acct": "hiking", "username": "hiking",
                         "display_name": "Hiking", "avatar": "/g.png",
                         "url": "https://plamenu.test/@hiking", "group": true },
            "reblog": group_post_entity(),
        });
        inject_group_mod(&mut boost, 7, true, false);
        assert_eq!(displayed_status_id(&boost), Some(42));
        assert!(boost["reblog"].get("_group_mod").is_some());
        assert!(boost.get("_group_mod").is_none(), "not on the wrapper");
        // Rendered, the unwrapped card shows the contextual (pinned) verbs.
        let card = status_card(&Status(&boost), &mod_ctx()).into_string();
        assert!(
            card.contains(r#"action="/web/groups/7/posts/42/remove""#),
            "{card}"
        );
        assert!(card.contains("Unpin from group"));

        // Site-level shortcuts follow that same displayed object: the
        // original author, never the Group Announce wrapper.
        let mut ctx = mod_ctx();
        ctx.admin.manage_users = true;
        let card = status_card(&Status(&boost), &ctx).into_string();
        assert!(card.contains(r#"href="/admin/accounts/9""#), "{card}");
        assert!(!card.contains("/admin/groups/7"), "{card}");
    }

    #[test]
    fn no_group_mod_menu_without_context() {
        // An ordinary post (no injected context) shows no Moderate section.
        let value = group_post_entity();
        let card = status_card(&Status(&value), &mod_ctx()).into_string();
        assert!(!card.contains("Remove from group"), "{card}");
        assert!(!card.contains("status__menu-label"), "{card}");
    }
}
