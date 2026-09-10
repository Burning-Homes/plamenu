//! Compose pipeline for locally-authored statuses: tokenizes `@mentions`
//! and `#hashtags` out of plain text, resolves mentions (local, known-remote,
//! or freshly webfingered), and renders Mastodon-compatible HTML.
//!
//! P4 adds Pleroma's rich-text posting: a status can be authored as
//! `text/markdown` or `text/html` instead of plain text. Those formats are
//! rendered/sanitized to HTML first, then the mention/hashtag/URL pass runs
//! over the HTML's text runs (never inside `a`/`code`/`pre`), so rich posts
//! get the same anchors, mention rows and tag records as plain ones.

use plamenu_ap::acct::Acct;
use plamenu_ap::text::{
    escape_html, inline_html_to_paragraphs, sanitize_remote_html, shortened_link_anchor,
};
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::account::{self, Account};
use serde_json::Value;

use crate::AppState;
use crate::error::ApiError;
use crate::remote::resolve_remote_account;

/// The format a status' source text is authored in — Pleroma's
/// `content_type` parameter. Unsupported values (`BBCode`, MFM) fall back to
/// plain text, like Pleroma's `get_content_type`; only [`Self::ADVERTISED`]
/// is offered to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostFormat {
    #[default]
    Plain,
    Markdown,
    Html,
}

impl PostFormat {
    /// The media types advertised as `pleroma.metadata.post_formats` /
    /// nodeinfo `postFormats`.
    pub const ADVERTISED: [&'static str; 3] = ["text/plain", "text/markdown", "text/html"];

    #[must_use]
    pub fn from_media_type(value: &str) -> Self {
        match value {
            "text/markdown" => Self::Markdown,
            "text/html" => Self::Html,
            _ => Self::Plain,
        }
    }

    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Plain => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Html => "text/html",
        }
    }
}

pub struct Composed {
    pub html: String,
    /// Resolved mentioned accounts (deduplicated).
    pub mentions: Vec<Account>,
    /// Normalized hashtag names, without `#` (deduplicated).
    pub hashtags: Vec<String>,
    /// `Mention`/`Hashtag` tag objects for the outgoing Note.
    pub tag_json: Vec<Value>,
}

enum Segment<'a> {
    Text(&'a str),
    Mention {
        username: &'a str,
        domain: Option<&'a str>,
    },
    Hashtag(&'a str),
    Url(&'a str),
}

fn is_username_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_domain_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-'
}

fn is_hashtag_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Length of the URL starting at `text[start..]`, if one starts there:
/// an `http(s)://` scheme, characters up to whitespace/quote/angle bracket,
/// then trailing punctuation (and unbalanced `)`) trimmed off. `None` when
/// the remainder is not a plausible URL (the authority needs a dot).
fn url_span(text: &str, start: usize) -> Option<usize> {
    let rest = &text.as_bytes()[start..];
    let scheme_len = if rest.len() > 8 && rest[..8].eq_ignore_ascii_case(b"https://") {
        8
    } else if rest.len() > 7 && rest[..7].eq_ignore_ascii_case(b"http://") {
        7
    } else {
        return None;
    };
    let mut end = scheme_len;
    while end < rest.len() {
        let b = rest[end];
        if b.is_ascii_whitespace()
            || b.is_ascii_control()
            || matches!(b, b'<' | b'>' | b'"' | b'\'' | b'`' | b'\\')
        {
            break;
        }
        end += 1;
    }
    // Trailing punctuation belongs to the sentence, not the URL; a `)` only
    // counts while it has no matching `(` inside the URL.
    loop {
        let candidate = &rest[scheme_len..end];
        let trimmed = match rest.get(end - 1) {
            Some(b'.' | b',' | b';' | b':' | b'!' | b'?' | b'*' | b'~' | b']' | b'}') => true,
            Some(b')') => {
                let (opens, closes) = candidate.iter().fold((0u32, 0u32), |(o, c), &b| match b {
                    b'(' => (o + 1, c),
                    b')' => (o, c + 1),
                    _ => (o, c),
                });
                opens < closes
            }
            _ => false,
        };
        if trimmed && end > scheme_len {
            end -= 1;
        } else {
            break;
        }
    }
    // The authority must look like a host: non-empty, with a dot before
    // the path (`https://localhost/x` or a bare scheme are not linkified).
    let after_scheme = &rest[scheme_len..end];
    let authority_end = after_scheme
        .iter()
        .position(|&b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(after_scheme.len());
    if authority_end == 0 || !after_scheme[..authority_end].contains(&b'.') {
        return None;
    }
    Some(end)
}

/// What a URL counts as toward the character limit, whatever its real
/// length — Mastodon's `StatusLengthValidator::URL_PLACEHOLDER` (23 × `x`).
const URL_PLACEHOLDER: &str = "xxxxxxxxxxxxxxxxxxxxxxx";

/// The status length the character limit is checked against, counted like
/// Mastodon's `StatusLengthValidator`: spoiler text and post text combined,
/// every URL a fixed 23 characters, a remote mention's domain free
/// (`@user@remote.example` counts as `@user`), and the result measured in
/// grapheme clusters rather than chars or bytes.
pub(crate) fn countable_length(spoiler_text: &str, text: &str) -> usize {
    use unicode_segmentation::UnicodeSegmentation;

    let mut combined = String::with_capacity(spoiler_text.len() + text.len());
    combined.push_str(spoiler_text);
    for segment in tokenize(text) {
        match segment {
            Segment::Text(literal) => combined.push_str(literal),
            Segment::Url(_) => combined.push_str(URL_PLACEHOLDER),
            Segment::Mention { username, .. } => {
                combined.push('@');
                combined.push_str(username);
            }
            Segment::Hashtag(name) => {
                combined.push('#');
                combined.push_str(name);
            }
        }
    }
    combined.graphemes(true).count()
}

/// The URLs appearing in a plain-text status, in order — what the link
/// crawler scans for local statuses (Mastodon parses the raw text too).
pub(crate) fn urls_in(text: &str) -> Vec<&str> {
    tokenize(text)
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Url(url) => Some(url),
            _ => None,
        })
        .collect()
}

/// Splits text into literal segments and mention/hashtag tokens. Tokens only
/// count when preceded by start-of-text or a non-word character.
fn tokenize(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut segments = Vec::new();
    let mut literal_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        // URLs first: an `@` or `#` inside a URL is part of it, not a token.
        if at_boundary
            && (bytes[i] | 0x20) == b'h'
            && let Some(len) = url_span(text, i)
        {
            if literal_start < i {
                segments.push(Segment::Text(&text[literal_start..i]));
            }
            segments.push(Segment::Url(&text[i..i + len]));
            i += len;
            literal_start = i;
            continue;
        }
        if at_boundary && bytes[i] == b'@' {
            let name_start = i + 1;
            let mut j = name_start;
            while j < bytes.len() && is_username_byte(bytes[j]) {
                j += 1;
            }
            if j > name_start && j - name_start <= 30 {
                let username = &text[name_start..j];
                // Optional @domain part.
                let (domain, end) = if j < bytes.len() && bytes[j] == b'@' {
                    let domain_start = j + 1;
                    let mut k = domain_start;
                    while k < bytes.len() && is_domain_byte(bytes[k]) {
                        k += 1;
                    }
                    // A domain needs at least one dot to be plausible.
                    if k > domain_start && text[domain_start..k].contains('.') {
                        (Some(text[domain_start..k].trim_end_matches('.')), k)
                    } else {
                        (None, j)
                    }
                } else {
                    (None, j)
                };
                if literal_start < i {
                    segments.push(Segment::Text(&text[literal_start..i]));
                }
                segments.push(Segment::Mention { username, domain });
                let consumed = domain.map_or(end, |d| j + 1 + d.len());
                i = consumed;
                literal_start = i;
                continue;
            }
        }
        if at_boundary && bytes[i] == b'#' {
            let name_start = i + 1;
            let mut j = name_start;
            while j < bytes.len() && is_hashtag_byte(bytes[j]) {
                j += 1;
            }
            let name = &text[name_start..j];
            // Tags must contain a letter (a bare `#2026` is not a tag).
            if !name.is_empty() && name.bytes().any(|b| b.is_ascii_alphabetic()) {
                if literal_start < i {
                    segments.push(Segment::Text(&text[literal_start..i]));
                }
                segments.push(Segment::Hashtag(name));
                i = j;
                literal_start = i;
                continue;
            }
        }
        i += 1;
    }
    if literal_start < text.len() {
        segments.push(Segment::Text(&text[literal_start..]));
    }
    segments
}

/// Resolves a mention to an account: local user, already-known remote, or —
/// when `fetch_remote` — a fresh webfinger + actor fetch. `None` leaves the
/// token as plain text. `fetch_remote` is `false` for re-rendering stored
/// text (announcements), matching Mastodon's `Account.from_text`, which only
/// matches existing accounts and never reaches the network.
async fn resolve_mention(
    state: &AppState,
    username: &str,
    domain: Option<&str>,
    fetch_remote: bool,
) -> Result<Option<Account>, ApiError> {
    if domain.is_none_or(|domain| state.config.is_local_domain(domain)) {
        let account = account::find_local_account_by_username(&state.pool, username).await?;
        let Some(account) = account else {
            return Ok(None);
        };
        if !crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &account,
        )
        .await?
        {
            return Ok(None);
        }
        return Ok(Some(account));
    }
    let domain = domain.expect("checked above");
    let Ok(acct) = Acct::new(username, domain) else {
        return Ok(None);
    };
    // A `@name@host` mention targets a person-like actor, never a Group.
    if !fetch_remote {
        return account::find_remote_person_by_acct(&state.pool, acct.username(), acct.domain())
            .await
            .map_err(Into::into);
    }
    // Known remote or best-effort fresh resolution; failures degrade to
    // plain text.
    resolve_remote_account(state, &acct, account::ActorClass::PersonLike)
        .await
        .map_err(Into::into)
}

fn mention_anchor(url: &str, username: &str) -> String {
    format!(
        r#"<span class="h-card"><a href="{}" class="u-url mention">@<span>{}</span></a></span>"#,
        escape_html(url),
        escape_html(username),
    )
}

fn hashtag_anchor(domain: &str, name: &str) -> String {
    format!(
        r#"<a href="https://{domain}/tags/{}" class="mention hashtag" rel="tag">#<span>{}</span></a>"#,
        escape_html(&name.to_lowercase()),
        escape_html(name),
    )
}

/// Tokenizes, resolves and renders a status text, webfingering unknown remote
/// mentions (the compose path for a freshly authored status).
pub async fn compose(
    state: &AppState,
    text: &str,
    format: PostFormat,
) -> Result<Composed, ApiError> {
    compose_with(state, text, format, true).await
}

/// Like [`compose`] but resolves mentions against known accounts only, never
/// the network — for re-rendering stored text such as an announcement's, the
/// way Mastodon's `Account.from_text` works.
pub async fn compose_local(state: &AppState, text: &str) -> Result<Composed, ApiError> {
    compose_with(state, text, PostFormat::Plain, false).await
}

/// Renders a draft the way [`compose`] would but *without touching the
/// network* — mentions resolve against known accounts only, so an unknown
/// remote handle stays literal rather than triggering a webfinger fetch that
/// would persist a remote account row. Used by the no-persist status preview
/// A client calls it on every keystroke, so it must be a pure read.
/// Unlike [`compose_local`] it honors the requested [`PostFormat`], since the
/// whole point is to show the exact rich-text rendering the post will get.
pub async fn compose_preview(
    state: &AppState,
    text: &str,
    format: PostFormat,
) -> Result<Composed, ApiError> {
    compose_with(state, text, format, false).await
}

/// A piece of the pre-rendered HTML the linkify pass walks: markup passed
/// through verbatim, or a text run (entity-decoded back to raw text) that
/// mention/hashtag/URL tokens are rendered into.
enum Chunk {
    Html(String),
    Text(String),
}

/// Renders Markdown to sanitized HTML (`CommonMark` + strikethrough — tables
/// and footnotes stay off because the sanitizer's Mastodon allowlist would
/// mangle their markup anyway). Shared with ingest: remote peers (`PeerTube`
/// posts and comments) declare `mediaType: text/markdown` content.
/// `newline_breaks` renders every single newline as a line break
/// (markdown-it's `breaks: true`, what `PeerTube`'s own UI does with its
/// newline-heavy descriptions); local composing keeps `CommonMark` semantics.
pub(crate) fn markdown_to_sanitized_html(text: &str, newline_breaks: bool) -> String {
    let parser = pulldown_cmark::Parser::new_ext(
        text,
        pulldown_cmark::Options::ENABLE_STRIKETHROUGH,
    )
    .map(|event| match event {
        pulldown_cmark::Event::SoftBreak if newline_breaks => pulldown_cmark::Event::HardBreak,
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    sanitize_remote_html(&html)
}

/// Linkifies bare URLs and `#hashtags` in already-rendered *remote* HTML,
/// without resolving mentions against the network. Remote markdown posts
/// (`PeerTube` descriptions and comments) arrive with plain-text links and
/// tags; this gives them the same anchors local content gets from
/// [`compose_with`] — bare URLs become links and hashtags route to this
/// instance's tag page (Mastodon's remote-hashtag behaviour). Reuses the exact
/// [`html_chunks`]/[`tokenize`] pass, so `<a>`/`code`/`pre` are left verbatim
/// and escaping matches the local path. Mentions are kept literal here: the
/// object's `tag` metadata drives federation addressing, not a body re-scan.
pub(crate) fn linkify_remote_html(html: &str, local_domain: &str) -> String {
    let mut out = String::with_capacity(html.len() + 32);
    for chunk in &html_chunks(html) {
        let run = match chunk {
            Chunk::Html(markup) => {
                out.push_str(markup);
                continue;
            }
            Chunk::Text(run) => run,
        };
        for segment in tokenize(run) {
            match segment {
                Segment::Text(literal) => out.push_str(&escape_html(literal)),
                Segment::Url(url) => out.push_str(&shortened_link_anchor(url)),
                Segment::Hashtag(name) => out.push_str(&hashtag_anchor(local_domain, name)),
                Segment::Mention { username, domain } => {
                    out.push('@');
                    out.push_str(&escape_html(username));
                    if let Some(d) = domain {
                        out.push('@');
                        out.push_str(&escape_html(d));
                    }
                }
            }
        }
    }
    out
}

/// The tag's element name (`</p>` → `p`), lowercased byte-wise — sanitizer
/// output is already lowercase and ASCII.
fn tag_name(tag: &str) -> &str {
    let inner = tag.trim_start_matches('<').trim_start_matches('/');
    let end = inner
        .bytes()
        .position(|b| !b.is_ascii_alphanumeric())
        .unwrap_or(inner.len());
    &inner[..end]
}

/// Decodes the entities the sanitizer emits in text runs back to raw text so
/// the tokenizer sees what the author typed (it re-escapes literals when
/// rendering). Unknown entities keep their `&` verbatim.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Entities are short and ASCII; a far-away `;` means a bare `&`.
        let Some(semi) = rest.as_bytes().iter().take(34).position(|&b| b == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            hex if hex.starts_with("#x") || hex.starts_with("#X") => {
                u32::from_str_radix(&hex[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            dec if dec.starts_with('#') => dec[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &rest[semi + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Splits sanitized HTML into verbatim markup and linkifiable text runs. Text
/// inside `a` (already a link), `code` and `pre` (literal by convention) is
/// kept verbatim. The input comes out of ammonia, so tags are well-formed and
/// `<`/`>` never appear in text runs.
fn html_chunks(html: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut skip_depth = 0usize;
    let push_text = |chunks: &mut Vec<Chunk>, run: &str, skip_depth: usize| {
        if run.is_empty() {
            return;
        }
        if skip_depth == 0 {
            chunks.push(Chunk::Text(decode_entities(run)));
        } else {
            chunks.push(Chunk::Html(run.to_owned()));
        }
    };
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut chunks, rest, skip_depth);
            break;
        };
        push_text(&mut chunks, &rest[..lt], skip_depth);
        let Some(gt) = rest[lt..].find('>') else {
            // Cannot come out of the sanitizer; keep the remainder verbatim.
            chunks.push(Chunk::Html(rest[lt..].to_owned()));
            break;
        };
        let tag = &rest[lt..=lt + gt];
        if matches!(tag_name(tag), "a" | "code" | "pre") {
            if tag.starts_with("</") {
                skip_depth = skip_depth.saturating_sub(1);
            } else {
                skip_depth += 1;
            }
        }
        chunks.push(Chunk::Html(tag.to_owned()));
        rest = &rest[lt + gt + 1..];
    }
    chunks
}

async fn compose_with(
    state: &AppState,
    text: &str,
    format: PostFormat,
    fetch_remote: bool,
) -> Result<Composed, ApiError> {
    // Rich formats render to HTML first; the token pass below then only sees
    // its text runs. Plain text is one big run, wrapped into paragraphs at
    // the end (rich formats carry their own block structure).
    let chunks = match format {
        PostFormat::Plain => vec![Chunk::Text(text.to_owned())],
        PostFormat::Markdown => html_chunks(&markdown_to_sanitized_html(text, false)),
        PostFormat::Html => html_chunks(&sanitize_remote_html(text)),
    };

    let mut inline = String::with_capacity(text.len() + 32);
    let mut mentions: Vec<Account> = Vec::new();
    let mut hashtags: Vec<String> = Vec::new();
    let mut tag_json: Vec<Value> = Vec::new();

    for chunk in &chunks {
        let run = match chunk {
            Chunk::Html(markup) => {
                inline.push_str(markup);
                continue;
            }
            Chunk::Text(run) => run,
        };
        for segment in tokenize(run) {
            match segment {
                Segment::Text(literal) => inline.push_str(&escape_html(literal)),
                Segment::Url(url) => inline.push_str(&shortened_link_anchor(url)),
                Segment::Hashtag(name) => {
                    inline.push_str(&hashtag_anchor(&state.config.domain, name));
                    let normalized = name.to_lowercase();
                    if !hashtags.contains(&normalized) {
                        tag_json.push(serde_json::json!({
                            "type": "Hashtag",
                            "href": format!("https://{}/tags/{normalized}", state.config.domain),
                            "name": format!("#{normalized}"),
                        }));
                        hashtags.push(normalized);
                    }
                }
                Segment::Mention { username, domain } => {
                    if let Some(mentioned) =
                        resolve_mention(state, username, domain, fetch_remote).await?
                    {
                        let urls = LocalUserUrls::for_account(
                            &state.config.domain,
                            &mentioned.username,
                            mentioned.uri.as_deref(),
                        );
                        // The `tag` Mention `href` is the AP id (federation
                        // addressing), but the visible anchor links the human
                        // web page, like Mastodon's `TagManager#url_for` vs
                        // `uri_for`.
                        let url = mentioned.uri.clone().unwrap_or_else(|| urls.id.clone());
                        let web_url =
                            crate::entities::account_web_url(&state.config.domain, &mentioned);
                        inline.push_str(&mention_anchor(&web_url, &mentioned.username));
                        if !mentions.iter().any(|m| m.id == mentioned.id) {
                            let acct = if mentioned.has_local_account_on(&state.config.domain) {
                                format!("{}@{}", mentioned.username, state.config.account_domain)
                            } else {
                                format!(
                                    "{}@{}",
                                    mentioned.username,
                                    mentioned.domain.as_deref().unwrap_or_default()
                                )
                            };
                            tag_json.push(serde_json::json!({
                                "type": "Mention",
                                "href": url,
                                "name": format!("@{acct}"),
                            }));
                            mentions.push(mentioned);
                        }
                    } else {
                        // Unresolvable: keep the original token as text.
                        inline.push('@');
                        inline.push_str(&escape_html(username));
                        if let Some(d) = domain {
                            inline.push('@');
                            inline.push_str(&escape_html(d));
                        }
                    }
                }
            }
        }
    }

    Ok(Composed {
        html: match format {
            PostFormat::Plain => inline_html_to_paragraphs(&inline),
            PostFormat::Markdown | PostFormat::Html => inline,
        },
        mentions,
        hashtags,
        tag_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        tokenize(text)
            .iter()
            .map(|s| match s {
                Segment::Text(t) => format!("T({t})"),
                Segment::Mention { username, domain } => {
                    format!("M({username},{})", domain.unwrap_or("-"))
                }
                Segment::Hashtag(name) => format!("H({name})"),
                Segment::Url(url) => format!("U({url})"),
            })
            .collect()
    }

    #[test]
    fn tokenizer_finds_mentions_and_tags_at_boundaries() {
        assert_eq!(
            kinds("hi @bob@remote.example and @alice, #Rust2026!"),
            [
                "T(hi )",
                "M(bob,remote.example)",
                "T( and )",
                "M(alice,-)",
                "T(, )",
                "H(Rust2026)",
                "T(!)",
            ]
        );
        // Mid-word @ and # are not tokens; pure numbers are not tags.
        assert_eq!(
            kinds("mail@example.com costs #42"),
            ["T(mail@example.com costs #42)"]
        );
        assert_eq!(kinds("#ok"), ["H(ok)"]);
    }

    #[test]
    fn tokenizer_finds_urls_and_trims_trailing_punctuation() {
        assert_eq!(
            kinds("see https://example.com/a?b=c#d, ok"),
            ["T(see )", "U(https://example.com/a?b=c#d)", "T(, ok)"]
        );
        // `@` and `#` inside a URL are part of it, not mention/hashtag.
        assert_eq!(
            kinds("https://remote.example/@bob/123"),
            ["U(https://remote.example/@bob/123)"]
        );
        // Balanced parentheses stay, unbalanced ones are sentence
        // punctuation.
        assert_eq!(
            kinds("(https://en.example/x_(y))"),
            ["T(()", "U(https://en.example/x_(y))", "T())"]
        );
        // No dot in the authority: not a URL. HTTP is case-insensitive.
        assert_eq!(kinds("https://localhost/x"), ["T(https://localhost/x)"]);
        assert_eq!(kinds("HTTPS://Example.COM"), ["U(HTTPS://Example.COM)"]);
        assert_eq!(
            kinds("end of sentence https://example.com."),
            ["T(end of sentence )", "U(https://example.com)", "T(.)"]
        );
    }

    #[test]
    fn countable_length_weighs_urls_mentions_and_graphemes() {
        // Plain text counts as-is; spoiler and text combine with no separator.
        assert_eq!(countable_length("", "hello"), 5);
        assert_eq!(countable_length("cw", "hello"), 7);
        // Every URL is a fixed 23, however long (or short) it really is.
        assert_eq!(
            countable_length(
                "",
                "https://example.com/a/very/long/path?with=query&and=more"
            ),
            23
        );
        assert_eq!(countable_length("", "https://a.io"), 23);
        // A remote mention's domain is free: `@bob@remote.example` = `@bob`.
        assert_eq!(countable_length("", "hi @bob@remote.example"), 7);
        assert_eq!(countable_length("", "hi @bob"), 7);
        // Hashtags count in full.
        assert_eq!(countable_length("", "#rust"), 5);
        // Grapheme clusters, not chars or bytes: a ZWJ emoji family and a
        // combining-accent are one column each.
        assert_eq!(
            countable_length("", "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
            1
        );
        assert_eq!(countable_length("", "e\u{301}"), 1);
    }

    #[test]
    fn post_format_parses_and_falls_back_like_pleroma() {
        assert_eq!(
            PostFormat::from_media_type("text/markdown"),
            PostFormat::Markdown
        );
        assert_eq!(PostFormat::from_media_type("text/html"), PostFormat::Html);
        assert_eq!(PostFormat::from_media_type("text/plain"), PostFormat::Plain);
        // Unsupported (bbcode/MFM) and garbage fall back to plain.
        assert_eq!(
            PostFormat::from_media_type("text/bbcode"),
            PostFormat::Plain
        );
        assert_eq!(
            PostFormat::from_media_type("text/x.misskeymarkdown"),
            PostFormat::Plain
        );
        assert_eq!(PostFormat::from_media_type(""), PostFormat::Plain);
        for advertised in PostFormat::ADVERTISED {
            assert_eq!(
                PostFormat::from_media_type(advertised).media_type(),
                advertised
            );
        }
    }

    #[test]
    fn decode_entities_round_trips_sanitizer_output() {
        use super::decode_entities;
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;p&gt;"), "<p>");
        assert_eq!(decode_entities("&quot;x&quot; &apos;y&apos;"), "\"x\" 'y'");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        // Unknown entities and bare ampersands stay verbatim.
        assert_eq!(decode_entities("&unknown; & rest"), "&unknown; & rest");
        assert_eq!(decode_entities("no entities"), "no entities");
    }

    #[test]
    fn html_chunks_split_text_runs_and_skip_links_and_code() {
        use super::{Chunk, html_chunks};
        let shape = |html: &str| -> Vec<String> {
            html_chunks(html)
                .iter()
                .map(|chunk| match chunk {
                    Chunk::Html(markup) => format!("H({markup})"),
                    Chunk::Text(run) => format!("T({run})"),
                })
                .collect()
        };
        assert_eq!(
            shape("<p>hi <em>you</em></p>"),
            [
                "H(<p>)", "T(hi )", "H(<em>)", "T(you)", "H(</em>)", "H(</p>)"
            ]
        );
        // Text inside anchors and code stays verbatim (no re-linkify).
        assert_eq!(
            shape(r#"<a href="https://x.example/">@not_a_mention</a>"#),
            [
                r#"H(<a href="https://x.example/">)"#,
                "H(@not_a_mention)",
                "H(</a>)"
            ]
        );
        assert_eq!(
            shape("<pre><code>#tag</code></pre> #real"),
            [
                "H(<pre>)",
                "H(<code>)",
                "H(#tag)",
                "H(</code>)",
                "H(</pre>)",
                "T( #real)"
            ]
        );
        // Entities in text runs are decoded for the tokenizer.
        assert_eq!(shape("<p>a &amp; b</p>"), ["H(<p>)", "T(a & b)", "H(</p>)"]);
    }

    #[test]
    fn linkify_remote_html_wraps_urls_and_hashtags_only() {
        use super::linkify_remote_html;
        let out = linkify_remote_html(
            "<p>See https://example.com/x and #Linux news</p>",
            "plamenu.test",
        );
        // Bare URL becomes an anchor (Mastodon-shaped, target=_blank).
        assert!(
            out.contains(r#"<a href="https://example.com/x" target="_blank""#),
            "url linkified: {out}"
        );
        // Hashtag routes to *this* instance's tag page (remote-hashtag rule),
        // lowercased in the href, original case in the visible text.
        assert!(
            out.contains(r#"href="https://plamenu.test/tags/linux""#)
                && out.contains("#<span>Linux</span>"),
            "hashtag linkified to local tag page: {out}"
        );
        // Mentions stay literal here (no network resolution at render time).
        let mention = linkify_remote_html("<p>ping @bob@remote.example ok</p>", "plamenu.test");
        assert!(
            mention.contains("@bob@remote.example") && !mention.contains("class=\"u-url mention\""),
            "mention left as text: {mention}"
        );
        // Anchors and code the markdown renderer already produced are not
        // re-scanned (reuses html_chunks' skip rules).
        let preformatted = linkify_remote_html(
            r"<pre><code>see #nope and https://skip.example</code></pre>",
            "plamenu.test",
        );
        assert!(
            !preformatted.contains("<a ") && preformatted.contains("#nope"),
            "code blocks untouched: {preformatted}"
        );
    }

    #[test]
    fn markdown_renders_sanitized_html() {
        use super::markdown_to_sanitized_html;
        assert_eq!(
            markdown_to_sanitized_html("**bold** and _em_", false),
            "<p><strong>bold</strong> and <em>em</em></p>\n"
        );
        assert_eq!(
            markdown_to_sanitized_html("~~gone~~", false),
            "<p><del>gone</del></p>\n"
        );
        // Raw HTML inside markdown is sanitized like remote content.
        let script = markdown_to_sanitized_html("hi <script>alert(1)</script>", false);
        assert!(!script.contains("<script"), "{script}");
        // Links keep their href and gain the forced rel.
        let link = markdown_to_sanitized_html("[text](https://example.com/x)", false);
        assert!(
            link.contains(
                r#"<a href="https://example.com/x" rel="nofollow noopener noreferrer">text</a>"#
            ),
            "{link}"
        );
    }

    #[test]
    fn urls_in_lists_urls_in_order() {
        assert_eq!(
            urls_in("a https://one.example/x then http://two.example."),
            ["https://one.example/x", "http://two.example"]
        );
        assert!(urls_in("no links here @bob #tag").is_empty());
    }
}
