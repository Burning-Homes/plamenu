//! Link preview cards — Mastodon's `FetchLinkCardService` and
//! `LinkCrawlWorker`: find the first eligible link of a status, fetch the
//! page (oEmbed first, then `OpenGraph`), store the card and attach it.
//!
//! Crawling is queued in Postgres (`link_crawl_jobs`) and best-effort with a
//! single attempt, like Mastodon's `retry: 0` worker. Cards are keyed by URL
//! and shared across statuses; a card older than two weeks is re-fetched.
//!
//! Differences from Mastodon, by design: card images stay remote URLs (no
//! local proxying or blurhash, like the rest of remote media), `http://`
//! links are not crawled (the outbound guard is https-only), and JSON-LD
//! structured data is not parsed (`OpenGraph` covers the same pages).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use plamenu_ap::text::{escape_html, sanitize_oembed_html};
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::preview_card::{self, NewPreviewCard, PreviewCard};
use plamenu_db::status::Status;
use plamenu_db::{media, mention, preview_card_trend, quote, status};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinHandle;
use url::Url;

use crate::AppState;
use crate::error::ApiError;

/// Cards older than this are re-fetched (Mastodon's two weeks).
const REFRESH_AFTER: time::Duration = time::Duration::days(14);

/// Mastodon's URL length limit (safe for unique indexes).
const URL_CHARACTER_LIMIT: usize = 2692;

/// Defensive caps on stored text fields — a hostile page is up to 1 MiB of
/// attacker-chosen metadata; clients display a couple of lines anyway.
const TEXT_LIMIT: usize = 1000;
const HTML_LIMIT: usize = 10_000;

const BATCH_SIZE: i64 = 20;
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

// ---------------------------------------------------------------------------
// HTML scanning (html5ever tokenizer — no tree construction needed)

/// A `<link>` element's relevant attributes (lowercased `rel`/`type`).
struct LinkTag {
    rel: String,
    kind: String,
    href: String,
}

/// An `<a>` element's relevant attributes, in document order.
struct AnchorTag {
    href: String,
    rel: String,
    class: String,
}

/// Everything the card extractor reads out of a page (or a status' HTML).
#[derive(Default)]
struct PageScan {
    /// `<meta property|name content>` pairs, keys lowercased.
    metas: Vec<(String, String)>,
    links: Vec<LinkTag>,
    anchors: Vec<AnchorTag>,
    /// `<title>` text (the first one wins).
    title: Option<String>,
    /// The `<html lang>` attribute.
    root_lang: Option<String>,
    /// `<img src>` values, in document order — used to recover images that
    /// live only inline in a status' body (Lemmy/PieFed markdown posts), which
    /// the HTML sanitizer strips.
    imgs: Vec<String>,
}

impl PageScan {
    /// The first non-empty `<meta>` content for `key` (`property` or `name`).
    fn meta(&self, key: &str) -> Option<&str> {
        self.metas
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }

    /// The first `<link>` href whose `rel` list contains `rel`.
    fn link_rel(&self, rel: &str) -> Option<&str> {
        self.links
            .iter()
            .find(|l| l.rel.split_ascii_whitespace().any(|r| r == rel))
            .map(|l| l.href.as_str())
    }

    /// Hrefs of every `<a>`/`<link>` element whose `rel` list contains `me` —
    /// the back-links a profile uses to prove ownership (rel="me" verification).
    fn rel_me_hrefs(&self) -> Vec<&str> {
        let has_me = |rel: &str| rel.split_ascii_whitespace().any(|r| r == "me");
        self.anchors
            .iter()
            .filter(|a| has_me(&a.rel))
            .map(|a| a.href.as_str())
            .chain(
                self.links
                    .iter()
                    .filter(|l| has_me(&l.rel))
                    .map(|l| l.href.as_str()),
            )
            .collect()
    }

    /// The oEmbed JSON endpoint advertised by the page, if any.
    fn oembed_href(&self) -> Option<&str> {
        self.links
            .iter()
            .find(|l| l.kind == "application/json+oembed")
            .map(|l| l.href.as_str())
    }
}

#[derive(Default)]
struct ScanSink {
    scan: RefCell<PageScan>,
    in_title: Cell<bool>,
    title_done: Cell<bool>,
}

fn attr<'t>(tag: &'t Tag, name: &str) -> Option<&'t str> {
    tag.attrs
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| &*a.value)
}

impl ScanSink {
    fn process_start_tag(&self, tag: &Tag) -> TokenSinkResult<()> {
        let mut scan = self.scan.borrow_mut();
        match &*tag.name {
            "meta" => {
                let key = attr(tag, "property").or_else(|| attr(tag, "name"));
                if let (Some(key), Some(content)) = (key, attr(tag, "content")) {
                    scan.metas
                        .push((key.to_ascii_lowercase(), content.to_owned()));
                }
            }
            "link" => {
                if let Some(href) = attr(tag, "href") {
                    scan.links.push(LinkTag {
                        rel: attr(tag, "rel").unwrap_or("").to_ascii_lowercase(),
                        kind: attr(tag, "type").unwrap_or("").to_ascii_lowercase(),
                        href: href.to_owned(),
                    });
                }
            }
            "a" => {
                if let Some(href) = attr(tag, "href") {
                    scan.anchors.push(AnchorTag {
                        href: href.to_owned(),
                        rel: attr(tag, "rel").unwrap_or("").to_ascii_lowercase(),
                        class: attr(tag, "class").unwrap_or("").to_owned(),
                    });
                }
            }
            "html" => {
                if scan.root_lang.is_none()
                    && let Some(lang) = attr(tag, "lang")
                {
                    scan.root_lang = Some(lang.to_owned());
                }
            }
            "img" => {
                if let Some(src) = attr(tag, "src") {
                    scan.imgs.push(src.to_owned());
                }
            }
            // Raw-content elements: the tokenizer must not read their bodies
            // as markup (a `<meta>` inside a script string is not a tag).
            "title" => {
                if !self.title_done.get() {
                    self.in_title.set(true);
                    if scan.title.is_none() {
                        scan.title = Some(String::new());
                    }
                }
                return TokenSinkResult::RawData(RawKind::Rcdata);
            }
            "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
            "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                return TokenSinkResult::RawData(RawKind::Rawtext);
            }
            "plaintext" => return TokenSinkResult::Plaintext,
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

impl TokenSink for ScanSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) => match tag.kind {
                TagKind::StartTag => self.process_start_tag(&tag),
                TagKind::EndTag => {
                    if &*tag.name == "title" && self.in_title.get() {
                        self.in_title.set(false);
                        self.title_done.set(true);
                    }
                    TokenSinkResult::Continue
                }
            },
            Token::CharacterTokens(text) => {
                if self.in_title.get()
                    && let Some(title) = &mut self.scan.borrow_mut().title
                {
                    title.push_str(&text);
                }
                TokenSinkResult::Continue
            }
            _ => TokenSinkResult::Continue,
        }
    }
}

/// Tokenizes an HTML document into the bits the card extractor needs.
fn scan_html(html: &str) -> PageScan {
    let tokenizer = Tokenizer::new(ScanSink::default(), TokenizerOpts::default());
    let input = html5ever::buffer_queue::BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.scan.into_inner()
}

/// The `src` of every `<img>` in `html`, in document order. Used to recover
/// images carried only inline in a remote status' body (Lemmy/PieFed markdown
/// posts declare no `attachment`), which the sanitizer would otherwise drop.
pub(crate) fn inline_image_srcs(html: &str) -> Vec<String> {
    scan_html(html).imgs
}

/// The absolute `rel="me"` back-links a page declares, resolved against
/// `base` — the input to rel="me" profile-link verification.
pub(crate) fn rel_me_backlinks(html: &str, base: &Url) -> Vec<String> {
    scan_html(html)
        .rel_me_hrefs()
        .into_iter()
        .filter_map(|href| resolve_url(base, href))
        .collect()
}

// ---------------------------------------------------------------------------
// Card extraction

/// A card's fields as extracted from a fetched page, pre-storage.
#[derive(Debug, Default)]
struct CardDraft {
    url: String,
    title: String,
    description: String,
    kind: String,
    author_name: String,
    author_url: String,
    provider_name: String,
    provider_url: String,
    html: String,
    width: i32,
    height: i32,
    image_url: Option<String>,
    image_description: String,
    embed_url: String,
    /// MIME hint for an audio stream discovered in Open Graph metadata. The
    /// media worker verifies it from the response when the file is cached.
    audio_content_type: Option<String>,
    language: Option<String>,
    published_at: Option<OffsetDateTime>,
}

impl CardDraft {
    /// A card is worth keeping when it has a title, embed HTML, or a directly
    /// playable media URL. The last case covers sparse podcast metadata.
    fn is_renderable(&self) -> bool {
        !(self.title.trim().is_empty() && self.html.is_empty() && self.embed_url.is_empty())
    }
}

fn truncated(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((offset, _)) => value[..offset].to_owned(),
        None => value.to_owned(),
    }
}

/// Resolves `candidate` (possibly relative) against `base` into an absolute
/// http(s) URL, rejecting the junk values pages actually serve.
fn resolve_url(base: &Url, candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == "null" || candidate == "undefined" {
        return None;
    }
    let joined = base.join(candidate).ok()?;
    (matches!(joined.scheme(), "http" | "https") && joined.host_str().is_some())
        .then(|| joined.to_string())
}

/// A plausible language tag's primary subtag (`en_US` → `en`), or nothing.
fn normalized_language(raw: &str) -> Option<String> {
    let primary = raw.split(['_', '-']).next().unwrap_or("");
    let lowered = primary.to_ascii_lowercase();
    crate::actions::is_language_code(&lowered).then_some(lowered)
}

/// An i32 out of an oEmbed dimension, which providers serve as number or
/// string (`"100%"` and friends become 0).
fn oembed_dimension(embed: &Value, key: &str) -> i32 {
    let value = match embed.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    i32::try_from(value).unwrap_or(0).max(0)
}

/// Builds a card from `OpenGraph` (and plain HTML) metadata, Mastodon's
/// `LinkDetailsExtractor` minus JSON-LD structured data.
fn opengraph_draft(scan: &PageScan, final_url: &Url) -> CardDraft {
    let audio = scan
        .meta("og:audio:secure_url")
        .or_else(|| scan.meta("og:audio"))
        .and_then(|v| resolve_url(final_url, v));
    let audio_content_type = audio.as_ref().map(|url| {
        scan.meta("og:audio:type")
            .and_then(normalized_audio_content_type)
            .unwrap_or_else(|| audio_content_type_from_url(url).to_owned())
    });
    let player = scan
        .meta("twitter:player")
        .and_then(|v| resolve_url(final_url, v));
    let width = scan
        .meta("twitter:player:width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0i32)
        .max(0);
    let height = scan
        .meta("twitter:player:height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0i32)
        .max(0);
    let (kind, html) = match (&audio, &player) {
        (Some(_), _) => ("audio", String::new()),
        (None, Some(src)) => (
            "video",
            format!(
                r#"<iframe src="{}" width="{width}" height="{height}" allowfullscreen="true" allowtransparency="true" scrolling="no" frameborder="0"></iframe>"#,
                escape_html(src),
            ),
        ),
        (None, None) => ("link", String::new()),
    };
    // The canonical URL replaces the fetched one, but only on the same
    // origin — a page may not claim to canonically be another site.
    let canonical = scan
        .link_rel("canonical")
        .or_else(|| scan.meta("og:url"))
        .and_then(|v| resolve_url(final_url, v))
        .filter(|v| Url::parse(v).is_ok_and(|parsed| parsed.host_str() == final_url.host_str()))
        .unwrap_or_else(|| final_url.to_string());
    let title = scan
        .meta("og:title")
        .or(scan.title.as_deref())
        .unwrap_or("")
        .trim()
        .to_owned();
    let description = scan
        .meta("og:description")
        .or_else(|| scan.meta("description"))
        .unwrap_or("")
        .to_owned();
    let provider_url = scan.meta("og:site").and_then(|v| {
        let absolute = if v.starts_with("http://") || v.starts_with("https://") {
            v.to_owned()
        } else {
            format!("http://{v}")
        };
        resolve_url(final_url, &absolute)
    });
    let published_at = scan
        .meta("article:published_time")
        .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok());
    let language = scan
        .meta("og:locale")
        .or(scan.root_lang.as_deref())
        .and_then(normalized_language);
    CardDraft {
        url: canonical,
        title: truncated(&title, TEXT_LIMIT),
        description: truncated(&description, TEXT_LIMIT),
        kind: kind.to_owned(),
        author_name: truncated(
            scan.meta("og:author")
                .or_else(|| scan.meta("og:author:username"))
                .unwrap_or(""),
            TEXT_LIMIT,
        ),
        author_url: String::new(),
        provider_name: truncated(scan.meta("og:site_name").unwrap_or(""), TEXT_LIMIT),
        provider_url: provider_url.unwrap_or_default(),
        html,
        width,
        height,
        image_url: scan
            .meta("og:image")
            .and_then(|v| resolve_url(final_url, v)),
        image_description: truncated(scan.meta("og:image:alt").unwrap_or(""), TEXT_LIMIT),
        embed_url: audio.unwrap_or_else(|| {
            scan.meta("twitter:player:stream")
                .and_then(|v| resolve_url(final_url, v))
                .unwrap_or_default()
        }),
        audio_content_type,
        language,
        published_at,
    }
}

/// A conservative audio MIME hint for cards reused from storage. Castopod's
/// Open Graph URL ends in `.mp3`; other common podcast containers are covered,
/// and an extensionless stream is provisionally MPEG until the cache worker
/// probes the actual response.
fn audio_content_type_from_url(url: &str) -> &'static str {
    let path = Url::parse(url).ok().map(|url| url.path().to_owned());
    let extension = path
        .as_deref()
        .and_then(|path| std::path::Path::new(path).extension())
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    match extension.to_ascii_lowercase().as_str() {
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "m4a" | "mp4" => "audio/mp4",
        "oga" | "ogg" | "opus" => "audio/ogg",
        "wav" | "wave" => "audio/wav",
        "webm" => "audio/webm",
        _ => "audio/mpeg",
    }
}

/// Normalizes a declared Open Graph audio MIME while rejecting non-audio or
/// unreasonably large values. Parameters do not affect the media kind and the
/// cache worker detects the authoritative type after download.
fn normalized_audio_content_type(value: &str) -> Option<String> {
    let base = value.split(';').next()?.trim();
    (base.len() <= 100
        && base.len() > "audio/".len()
        && base
            .get(.."audio/".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("audio/")))
    .then(|| base.to_ascii_lowercase())
}

/// Builds a card from a fetched oEmbed document, Mastodon's
/// `attempt_oembed`. `None` falls back to `OpenGraph` (`rich` embeds rely on
/// script tags and are rejected outright).
fn oembed_card_draft(embed: &Value, endpoint: &Url, page_url: &Url) -> Option<CardDraft> {
    let kind = embed.get("type").and_then(Value::as_str)?;
    let text = |key: &str| {
        truncated(
            embed.get(key).and_then(Value::as_str).unwrap_or(""),
            TEXT_LIMIT,
        )
    };
    let resolved = |key: &str| {
        embed
            .get(key)
            .and_then(Value::as_str)
            .and_then(|v| resolve_url(endpoint, v))
    };
    let mut draft = CardDraft {
        url: page_url.to_string(),
        title: text("title"),
        kind: kind.to_owned(),
        author_name: text("author_name"),
        author_url: resolved("author_url").unwrap_or_default(),
        provider_name: text("provider_name"),
        provider_url: resolved("provider_url").unwrap_or_default(),
        ..CardDraft::default()
    };
    match kind {
        "link" => {
            draft.image_url = resolved("thumbnail_url");
        }
        "photo" => {
            let image = resolved("url")?;
            draft.embed_url.clone_from(&image);
            draft.image_url = Some(image);
            draft.width = oembed_dimension(embed, "width");
            draft.height = oembed_dimension(embed, "height");
        }
        "video" => {
            draft.width = oembed_dimension(embed, "width");
            draft.height = oembed_dimension(embed, "height");
            draft.html = truncated(
                &sanitize_oembed_html(embed.get("html").and_then(Value::as_str).unwrap_or("")),
                HTML_LIMIT,
            );
            draft.image_url = resolved("thumbnail_url");
        }
        // `rich` embeds rely on <script>, which is a no-no (Mastodon too).
        _ => return None,
    }
    Some(draft)
}

// ---------------------------------------------------------------------------
// Link selection

/// Whether a link is worth crawling: http(s) — in practice https, the
/// outbound guard rejects plain http — under the length limit, and not one
/// of our own pages.
fn is_crawlable(domain: &str, url: &str) -> bool {
    if url.chars().count() > URL_CHARACTER_LIMIT {
        return false;
    }
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|host| !host.eq_ignore_ascii_case(domain))
}

/// The host of a URL, lowercased, for a `reject_media` domain-policy check.
#[must_use]
pub fn url_host(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// Whether an anchor of a remote status is a hashtag/mention rather than a
/// real link (Mastodon's `skip_link?`): `rel="tag"`, microformat/hashtag
/// classes, or the profile URL of a mentioned account. The class check is
/// wider than Mastodon's (`mention`/`hashtag` too) because our sanitizer
/// rewrites `rel` on stored remote content, so `rel="tag"` rarely survives.
fn is_tag_or_mention_anchor(anchor: &AnchorTag, mention_urls: &[String]) -> bool {
    anchor.rel.split_ascii_whitespace().any(|r| r == "tag")
        || anchor
            .class
            .split_ascii_whitespace()
            .any(|c| matches!(c, "u-url" | "h-card" | "mention" | "hashtag"))
        || mention_urls.contains(&anchor.href)
}

/// Whether a status could possibly carry something [`eligible_url`] would
/// return — checked before enqueuing a crawl, so the queue stops carrying the
/// ~75% of posts that have no link in them at all.
///
/// A *necessary* condition, deliberately weaker than the crawler's own rule:
/// [`is_crawlable`] accepts nothing but `https`, so a body with no `https://`
/// anywhere and no main link cannot yield a URL, whatever the rest of
/// `eligible_url` goes on to decide. Anything this lets through is still
/// judged by the crawler exactly as before, which is what keeps a cheap
/// pre-filter from silently losing cards.
///
/// Case-insensitive because `Url::parse` lowercases a scheme, so `HTTPS://…`
/// is crawlable and a plain `contains("https://")` would drop it.
#[must_use]
pub fn may_carry_link(item: &Status) -> bool {
    body_may_carry_link(&item.content, item.external_url.as_deref())
}

/// Drops an edited status' current card and queues a fresh crawl — Mastodon's
/// `reset_preview_card!`, shared by the local and remote edit paths so the two
/// cannot drift on when a card is re-fetched.
///
/// The new body is passed in rather than read off the row: at this point the
/// stored status still holds the content being replaced.
pub async fn reset_for_edit(
    state: &AppState,
    status_id: i64,
    content: &str,
    external_url: Option<&str>,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    reset_for_edit_conn(&mut conn, status_id, content, external_url).await
}

/// Reconciles preview state on the editing transaction's connection.
pub async fn reset_for_edit_conn(
    conn: &mut plamenu_db::PgConnection,
    status_id: i64,
    content: &str,
    external_url: Option<&str>,
) -> Result<(), ApiError> {
    media::delete_preview_media_for_status(&mut *conn, status_id).await?;
    preview_card::detach_all(&mut *conn, status_id).await?;
    if body_may_carry_link(content, external_url) {
        preview_card::enqueue_crawl(&mut *conn, status_id).await?;
    }
    Ok(())
}

/// [`may_carry_link`] over a body that is not stored yet — the edit paths,
/// where the status row still holds the *previous* content and the answer has
/// to be about the replacement.
#[must_use]
fn body_may_carry_link(content: &str, external_url: Option<&str>) -> bool {
    external_url.is_some()
        || content
            .as_bytes()
            .windows(8)
            .any(|window| window.eq_ignore_ascii_case(b"https://"))
}

/// The first eligible link of a status: scanned out of the raw source text
/// for local statuses, out of the stored HTML's anchors for remote ones —
/// exactly Mastodon's `parse_urls`.
async fn eligible_url(
    state: &AppState,
    item: &Status,
    batch: Option<&CrawlBatch>,
) -> Result<Option<String>, ApiError> {
    let domain = &state.config.domain;
    if item.uri.is_none() {
        let source = status::source_of(&state.pool, item.id)
            .await?
            .unwrap_or_default();
        // Rich-text posts (P4) can carry `[text](url)` links the raw-text
        // scan would miss; their rendered HTML is scanned like a remote
        // status' instead (our own mention/hashtag anchors carry the same
        // marker classes, so the filter below skips them).
        if source.content_type != "text/plain" {
            return Ok(scan_html(&item.content)
                .anchors
                .into_iter()
                .filter(|anchor| !is_tag_or_mention_anchor(anchor, &[]))
                .map(|anchor| anchor.href)
                .find(|candidate| is_crawlable(domain, candidate)));
        }
        return Ok(crate::compose::urls_in(&source.text)
            .into_iter()
            .find(|candidate| is_crawlable(domain, candidate))
            .map(str::to_owned));
    }
    // A converted link post (Lemmy `Page`, `Article`) carries its MAIN link —
    // the post's topic — in the `external_url` column, read from the first
    // `Link` attachment at ingest. That, not whatever URL happens to sit in the
    // body text (a crosspost backlink, an aside), is what the card should
    // describe — matching how Mastodon cards a converted Page/Article. Fall
    // back to scanning the body anchors only when there is no main link.
    if let Some(link) = item.external_url.as_deref()
        && is_crawlable(domain, link)
    {
        return Ok(Some(link.to_owned()));
    }
    let mentioned = match batch {
        Some(batch) => batch.mentions.get(&item.id).cloned().unwrap_or_default(),
        None => mention::for_statuses(&state.pool, &[item.id], false)
            .await?
            .remove(&item.id)
            .unwrap_or_default(),
    };
    let mention_urls: Vec<String> = mentioned
        .iter()
        .map(|account| {
            account.uri.clone().unwrap_or_else(|| {
                LocalUserUrls::for_account(domain, &account.username, account.uri.as_deref()).id
            })
        })
        .collect();
    Ok(scan_html(&item.content)
        .anchors
        .into_iter()
        .filter(|anchor| !is_tag_or_mention_anchor(anchor, &mention_urls))
        .map(|anchor| anchor.href)
        .find(|candidate| is_crawlable(domain, candidate)))
}

// ---------------------------------------------------------------------------
// The crawl itself

fn is_html(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/html")
}

/// Fetches `original_url` and stores a card for it, oEmbed before
/// `OpenGraph`.
/// Network and parse failures are quietly `None` — crawling is best-effort.
async fn fetch_card(
    state: &AppState,
    original_url: &str,
) -> Result<Option<(PreviewCard, Option<String>)>, ApiError> {
    let page = match state.federation.fetch_page(original_url, "text/html").await {
        Ok(page) => page,
        Err(error) => {
            tracing::debug!(error = %crate::error::ErrorChain(&error), url = original_url, "link preview fetch failed");
            return Ok(None);
        }
    };
    if !is_html(&page.content_type) {
        return Ok(None);
    }
    let Ok(final_url) = Url::parse(&page.final_url) else {
        return Ok(None);
    };
    let scan = scan_html(&page.body);
    let mut draft = None;
    if let Some(href) = scan.oembed_href()
        && let Some(endpoint) = resolve_url(&final_url, href)
        && let Ok(endpoint_url) = Url::parse(&endpoint)
        && let Ok(embed_page) = state
            .federation
            .fetch_page(&endpoint, "application/json")
            .await
        && let Ok(embed) = serde_json::from_str::<Value>(&embed_page.body)
    {
        draft = oembed_card_draft(&embed, &endpoint_url, &final_url);
    }
    let draft = draft.unwrap_or_else(|| opengraph_draft(&scan, &final_url));
    if !draft.is_renderable() || draft.url.chars().count() > URL_CHARACTER_LIMIT {
        return Ok(None);
    }
    // `fediverse:creator` author attribution (Mastodon's `attempt_opengraph`):
    // the named account becomes the card's verified author when its
    // attribution domains authorize the page's domain.
    let author_account_id = match scan.meta("fediverse:creator") {
        Some(handle) => resolve_card_author(state, handle, &draft.url).await?,
        None => None,
    };
    let audio_content_type = draft.audio_content_type.clone();
    let card = preview_card::upsert(
        &state.pool,
        NewPreviewCard {
            url: &draft.url,
            title: &draft.title,
            description: &draft.description,
            kind: &draft.kind,
            author_name: &draft.author_name,
            author_url: &draft.author_url,
            provider_name: &draft.provider_name,
            provider_url: &draft.provider_url,
            html: &draft.html,
            width: draft.width,
            height: draft.height,
            image_url: draft.image_url.as_deref(),
            image_description: &draft.image_description,
            embed_url: &draft.embed_url,
            language: draft.language.as_deref(),
            published_at: draft.published_at,
            author_account_id,
        },
    )
    .await?;
    Ok(Some((card, audio_content_type)))
}

/// Attaches an ordinary card, or promotes Open Graph audio into the established
/// remote-media pipeline. Audio is on-demand because podcast episodes are
/// long-form: playback streams through a bounded sparse cache within
/// `remote_video_max_mb`, while the viewer's `?d=1` preference remains the
/// last-resort origin fallback.
async fn attach_card_or_audio(
    state: &AppState,
    item: &Status,
    card: &PreviewCard,
    original_url: &str,
    audio_content_type: Option<&str>,
) -> Result<(), ApiError> {
    if card.kind == "audio" && !card.embed_url.is_empty() {
        let content_type =
            audio_content_type.unwrap_or_else(|| audio_content_type_from_url(&card.embed_url));
        media::create_remote(
            &state.pool,
            media::NewRemoteMedia {
                account_id: item.account_id,
                status_id: item.id,
                remote_url: &card.embed_url,
                preview_card_id: Some(card.id),
                content_type,
                description: (!card.title.is_empty()).then_some(card.title.as_str()),
                thumbnail_remote_url: card.image_url.as_deref(),
                download_on_demand: true,
                ..Default::default()
            },
        )
        .await?;
    } else {
        preview_card::attach(&state.pool, item.id, card.id, original_url).await?;
    }
    Ok(())
}

/// Resolves a page's `fediverse:creator` handle and verifies the attribution:
/// the account must list the card's domain among its `attributionDomains`
/// (with Mastodon's parent-suffix matching), or the domain's provider must be
/// trendable (Mastodon's temporary grace for known publishers). `None` on
/// any failure — attribution is best-effort.
async fn resolve_card_author(
    state: &AppState,
    handle: &str,
    card_url: &str,
) -> Result<Option<i64>, ApiError> {
    let Some(card_domain) = Url::parse(card_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
    else {
        return Ok(None);
    };
    let handle = handle.trim().trim_start_matches('@');
    let account = if let Ok(acct) = handle.parse::<plamenu_ap::acct::Acct>() {
        if state.config.is_local_domain(acct.domain()) {
            plamenu_db::account::find_local_account_by_username(&state.pool, acct.username())
                .await?
        } else {
            // A link-preview author (`fediverse:creator`) is a person-like actor.
            crate::remote::resolve_remote_account(
                state,
                &acct,
                plamenu_db::account::ActorClass::PersonLike,
            )
            .await?
        }
    } else if !handle.is_empty() && !handle.contains('@') {
        // A bare `@user` names an account on this server, like Mastodon's
        // `ResolveAccountService` with no domain.
        plamenu_db::account::find_local_account_by_username(&state.pool, handle).await?
    } else {
        None
    };
    let Some(account) = account else {
        return Ok(None);
    };
    if !crate::instance_policy::public_account_visible(&state.pool, &state.config.domain, &account)
        .await?
    {
        return Ok(None);
    }
    let domains = plamenu_db::account::attribution_domains(&state.pool, account.id).await?;
    if can_be_attributed_from(&domains, &card_domain) {
        return Ok(Some(account.id));
    }
    let trendable = plamenu_db::preview_card_provider::trendable_domains(&state.pool).await?;
    Ok(trendable.contains(&card_domain).then_some(account.id))
}

/// Mastodon's `can_be_attributed_from?`: the stored domains match the card
/// domain itself or any parent suffix (`blog.example.com` is authorized by a
/// stored `example.com`).
fn can_be_attributed_from(domains: &[String], card_domain: &str) -> bool {
    let mut suffix = card_domain;
    loop {
        if domains.iter().any(|d| d == suffix) {
            return true;
        }
        match suffix.split_once('.') {
            Some((_, rest)) if !rest.is_empty() => suffix = rest,
            _ => return false,
        }
    }
}

/// The shared per-job context of one claimed crawl batch (N+1 decisions):
/// the skip checks (`crawl_status` used to issue them once per job) loaded as
/// one query each across the batch. The per-URL work — the fresh-card lookup,
/// the fetch, attach, and trend registration — stays per job; so does the
/// local-status `source_of` read. The context is point-in-time for the claim:
/// should the queue ever hold two jobs for one status, the second re-crawls
/// against a stale skip set, which converges because `attach` is idempotent.
struct CrawlBatch {
    statuses: HashMap<i64, Status>,
    with_card: HashSet<i64>,
    with_media: HashSet<i64>,
    with_quote: HashSet<i64>,
    mentions: HashMap<i64, Vec<plamenu_db::account::Account>>,
}

impl CrawlBatch {
    async fn load(state: &AppState, status_ids: &[i64]) -> Result<Self, ApiError> {
        Ok(Self {
            statuses: status::find_by_ids(&state.pool, status_ids)
                .await?
                .into_iter()
                .map(|item| (item.id, item))
                .collect(),
            with_card: preview_card::for_statuses(&state.pool, status_ids)
                .await?
                .into_keys()
                .collect(),
            with_media: media::for_statuses(&state.pool, status_ids)
                .await?
                .into_keys()
                .collect(),
            with_quote: quote::for_statuses(&state.pool, status_ids)
                .await?
                .into_keys()
                .collect(),
            mentions: mention::for_statuses(&state.pool, status_ids, false).await?,
        })
    }
}

/// Crawls one status for a link preview: skipped when the status already has
/// a card, attachments or a quote (Mastodon's rules); a fresh stored card
/// for the same URL is reused without fetching. This is the per-job path the
/// worker falls back to when its batch load fails.
pub async fn crawl_status(state: &AppState, status_id: i64) -> Result<(), ApiError> {
    crawl_status_inner(state, status_id, None).await
}

async fn crawl_status_inner(
    state: &AppState,
    status_id: i64,
    batch: Option<&CrawlBatch>,
) -> Result<(), ApiError> {
    let item = match batch {
        Some(batch) => batch.statuses.get(&status_id).cloned(),
        None => status::find_by_id(&state.pool, status_id).await?,
    };
    let Some(item) = item else {
        return Ok(()); // deleted since being queued
    };
    let skip = match batch {
        Some(batch) => {
            item.reblog_of_id.is_some()
                || batch.with_card.contains(&item.id)
                || batch.with_media.contains(&item.id)
                || batch.with_quote.contains(&item.id)
        }
        None => {
            item.reblog_of_id.is_some()
                || preview_card::exists_for_status(&state.pool, item.id).await?
                || !media::for_statuses(&state.pool, &[item.id])
                    .await?
                    .is_empty()
                || quote::for_statuses(&state.pool, &[item.id])
                    .await?
                    .contains_key(&item.id)
        }
    };
    if skip {
        return Ok(());
    }
    let Some(original_url) = eligible_url(state, &item, batch).await? else {
        return Ok(());
    };
    let today = OffsetDateTime::now_utc().date();
    if let Some(existing) = preview_card::find_by_url(&state.pool, &original_url).await?
        && OffsetDateTime::now_utc() - existing.updated_at < REFRESH_AFTER
    {
        attach_card_or_audio(state, &item, &existing, &original_url, None).await?;
        // Register link-trend usage (Mastodon's `Trends.links.register`).
        preview_card_trend::record_use(&state.pool, existing.id, item.id, today).await?;
        return Ok(());
    }
    if let Some((card, audio_content_type)) = fetch_card(state, &original_url).await? {
        attach_card_or_audio(
            state,
            &item,
            &card,
            &original_url,
            audio_content_type.as_deref(),
        )
        .await?;
        preview_card_trend::record_use(&state.pool, card.id, item.id, today).await?;
        tracing::debug!(status = item.id, url = %original_url, "link preview attached");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The worker

/// Claims and crawls one batch of due jobs; returns how many were claimed
/// (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match preview_card::claim_due_crawls(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim link crawl jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    // One context load for the whole claim; a failed load falls back to the
    // per-job path so one poisoned batch read cannot sink its neighbours.
    let status_ids: Vec<i64> = jobs.iter().map(|job| job.status_id).collect();
    let batch = match CrawlBatch::load(state, &status_ids).await {
        Ok(batch) => Some(batch),
        Err(error) => {
            tracing::warn!(
                error = %error.chain(),
                "crawl batch context load failed; falling back to per-job reads"
            );
            None
        }
    };
    for job in jobs {
        // The claim leased the job rather than deleting it, so a crash before
        // the crawl finishes lets the lease expire and it runs again. Crawling
        // is single-attempt best-effort, so complete the job
        // whatever the outcome — dropping one reclaimed past the cap.
        if job.exhausted() {
            tracing::warn!(
                status = job.status_id,
                attempts = job.attempts,
                "dropping link crawl after too many crash reclaims"
            );
        } else if let Err(error) = crawl_status_inner(state, job.status_id, batch.as_ref()).await {
            tracing::warn!(error = %error.chain(), status = job.status_id, "link preview crawl failed");
        }
        if let Err(error) = preview_card::complete_crawl(&state.pool, job.id).await {
            tracing::error!(%error, status = job.status_id, "failed to complete link crawl job");
        }
    }
    claimed
}

/// Runs the crawl loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("link preview worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scan_reads_metadata_and_skips_script_bodies() {
        let scan = scan_html(concat!(
            "<!doctype html><html lang=\"en-US\"><head>",
            "<title>Page &amp; Title</title>",
            r#"<meta property="OG:Description" content="A &quot;desc&quot;">"#,
            r#"<meta name="description" content="fallback">"#,
            r#"<link rel="Canonical" href="/canonical">"#,
            r#"<link rel="alternate" type="application/json+oembed" href="/oembed.json">"#,
            r#"<script>var html = '<meta property="og:title" content="not real">';</script>"#,
            "</head><body>",
            r#"<a href="https://elsewhere.example/x" rel="nofollow">x</a>"#,
            "</body></html>",
        ));
        assert_eq!(scan.title.as_deref(), Some("Page & Title"));
        assert_eq!(scan.meta("og:description"), Some(r#"A "desc""#));
        assert_eq!(scan.meta("description"), Some("fallback"));
        assert_eq!(scan.link_rel("canonical"), Some("/canonical"));
        assert_eq!(scan.oembed_href(), Some("/oembed.json"));
        // The og:title inside the script string must not have been parsed.
        assert_eq!(scan.meta("og:title"), None);
        assert_eq!(scan.root_lang.as_deref(), Some("en-US"));
        assert_eq!(scan.anchors.len(), 1);
        assert_eq!(scan.anchors[0].href, "https://elsewhere.example/x");
    }

    #[test]
    fn opengraph_draft_extracts_mastodon_fields() {
        let final_url = Url::parse("https://news.example/articles/42?utm=x").unwrap();
        let scan = scan_html(concat!(
            "<html><head><title>html title</title>",
            r#"<meta property="og:title" content="OG Title">"#,
            r#"<meta property="og:description" content="OG description">"#,
            r#"<meta property="og:image" content="/cover.jpg">"#,
            r#"<meta property="og:image:alt" content="a cover">"#,
            r#"<meta property="og:site_name" content="News">"#,
            r#"<meta property="og:locale" content="en_US">"#,
            r#"<meta property="article:published_time" content="2026-05-01T10:00:00Z">"#,
            r#"<link rel="canonical" href="https://news.example/articles/42">"#,
            "</head></html>",
        ));
        let draft = opengraph_draft(&scan, &final_url);
        assert_eq!(draft.kind, "link");
        assert_eq!(draft.url, "https://news.example/articles/42");
        assert_eq!(draft.title, "OG Title");
        assert_eq!(draft.description, "OG description");
        assert_eq!(
            draft.image_url.as_deref(),
            Some("https://news.example/cover.jpg")
        );
        assert_eq!(draft.image_description, "a cover");
        assert_eq!(draft.provider_name, "News");
        assert_eq!(draft.language.as_deref(), Some("en"));
        assert!(draft.published_at.is_some());
        assert!(draft.is_renderable());
    }

    #[test]
    fn opengraph_draft_rejects_cross_origin_canonical_and_builds_players() {
        let final_url = Url::parse("https://video.example/v/9").unwrap();
        let scan = scan_html(concat!(
            "<html><head><title>Clip</title>",
            r#"<link rel="canonical" href="https://evil.example/steal">"#,
            r#"<meta name="twitter:player" content="https://video.example/embed/9">"#,
            r#"<meta name="twitter:player:width" content="640">"#,
            r#"<meta name="twitter:player:height" content="360">"#,
            "</head></html>",
        ));
        let draft = opengraph_draft(&scan, &final_url);
        assert_eq!(draft.url, "https://video.example/v/9");
        assert_eq!(draft.kind, "video");
        assert_eq!(draft.width, 640);
        assert_eq!(draft.height, 360);
        assert!(
            draft
                .html
                .contains(r#"src="https://video.example/embed/9""#),
            "{}",
            draft.html
        );
    }

    #[test]
    fn opengraph_audio_wins_over_an_iframe_player() {
        let final_url = Url::parse("https://pod.example/@show/episodes/one").unwrap();
        let scan = scan_html(concat!(
            "<html><head><title>Episode one</title>",
            r#"<meta name="twitter:player" content="/episodes/one/embed/light">"#,
            r#"<meta property="og:audio" content="https://cdn.example/one.mp3?source=og">"#,
            r#"<meta property="og:audio:type" content="audio/mpeg">"#,
            "</head></html>",
        ));
        let draft = opengraph_draft(&scan, &final_url);
        assert_eq!(draft.kind, "audio");
        assert_eq!(draft.embed_url, "https://cdn.example/one.mp3?source=og");
        assert_eq!(draft.audio_content_type.as_deref(), Some("audio/mpeg"));
        assert!(
            draft.html.is_empty(),
            "the remote iframe must not be embedded"
        );
    }

    #[test]
    fn untitled_pages_are_not_renderable() {
        let final_url = Url::parse("https://blank.example/").unwrap();
        let draft = opengraph_draft(&scan_html("<html><head></head></html>"), &final_url);
        assert!(!draft.is_renderable());
        // Whitespace-only titles do not count either.
        let draft = opengraph_draft(
            &scan_html("<html><head><title>  \n </title></head></html>"),
            &final_url,
        );
        assert!(!draft.is_renderable());
    }

    #[test]
    fn oembed_drafts_map_like_mastodon() {
        let endpoint = Url::parse("https://photos.example/oembed?url=x").unwrap();
        let page = Url::parse("https://photos.example/p/1").unwrap();
        let photo = oembed_card_draft(
            &json!({
                "type": "photo",
                "title": "A photo",
                "author_name": "Ann",
                "author_url": "/ann",
                "url": "https://cdn.photos.example/p1.jpg",
                "width": 1024,
                "height": "768",
            }),
            &endpoint,
            &page,
        )
        .unwrap();
        assert_eq!(photo.kind, "photo");
        assert_eq!(photo.url, "https://photos.example/p/1");
        assert_eq!(photo.embed_url, "https://cdn.photos.example/p1.jpg");
        assert_eq!(
            photo.image_url.as_deref(),
            Some("https://cdn.photos.example/p1.jpg")
        );
        assert_eq!(photo.width, 1024);
        assert_eq!(photo.height, 768);
        // Relative author URL resolves against the oEmbed endpoint.
        assert_eq!(photo.author_url, "https://photos.example/ann");

        let video = oembed_card_draft(
            &json!({
                "type": "video",
                "title": "A video",
                "html": "<iframe src=\"https://tube.example/e/1\"></iframe><script>x()</script>",
                "width": 640,
                "height": 360,
            }),
            &endpoint,
            &page,
        )
        .unwrap();
        assert_eq!(video.kind, "video");
        assert!(video.html.contains("iframe"), "{}", video.html);
        assert!(video.html.contains("sandbox="), "{}", video.html);
        assert!(!video.html.contains("<script"), "{}", video.html);
        assert!(!video.html.contains("x()"), "{}", video.html);

        // A photo without a url, and a rich embed, fall back to OpenGraph.
        assert!(oembed_card_draft(&json!({"type": "photo"}), &endpoint, &page).is_none());
        assert!(
            oembed_card_draft(
                &json!({"type": "rich", "html": "<script></script>"}),
                &endpoint,
                &page
            )
            .is_none()
        );
    }

    #[test]
    fn crawlable_urls_are_https_and_not_local() {
        assert!(is_crawlable("plamenu.test", "https://example.com/a"));
        assert!(!is_crawlable(
            "plamenu.test",
            "https://plamenu.test/users/alice"
        ));
        assert!(!is_crawlable("plamenu.test", "http://example.com/a"));
        assert!(!is_crawlable("plamenu.test", "not a url"));
        let long = format!("https://example.com/{}", "a".repeat(URL_CHARACTER_LIMIT));
        assert!(!is_crawlable("plamenu.test", &long));
    }

    #[test]
    fn tag_and_mention_anchors_are_skipped() {
        let tag = AnchorTag {
            href: "https://remote.example/tags/x".into(),
            rel: "tag nofollow".into(),
            class: String::new(),
        };
        let mention = AnchorTag {
            href: "https://remote.example/@bob".into(),
            rel: String::new(),
            class: "u-url mention".into(),
        };
        let mentioned_profile = AnchorTag {
            href: "https://plamenu.test/users/alice".into(),
            rel: String::new(),
            class: String::new(),
        };
        let plain = AnchorTag {
            href: "https://news.example/a".into(),
            rel: "nofollow noopener".into(),
            class: String::new(),
        };
        // A sanitized hashtag anchor: `rel="tag"` was rewritten, only the
        // class is left.
        let sanitized_hashtag = AnchorTag {
            href: "https://remote.example/tags/x".into(),
            rel: "nofollow noopener noreferrer".into(),
            class: "mention hashtag".into(),
        };
        let mentions = vec!["https://plamenu.test/users/alice".to_owned()];
        assert!(is_tag_or_mention_anchor(&tag, &mentions));
        assert!(is_tag_or_mention_anchor(&mention, &mentions));
        assert!(is_tag_or_mention_anchor(&sanitized_hashtag, &mentions));
        assert!(is_tag_or_mention_anchor(&mentioned_profile, &mentions));
        assert!(!is_tag_or_mention_anchor(&plain, &mentions));
    }

    #[test]
    fn rel_me_backlinks_collects_anchors_and_links_absolute() {
        let base = Url::parse("https://alice.example/").unwrap();
        let backlinks = rel_me_backlinks(
            concat!(
                "<html><head>",
                r#"<link rel="me" href="https://social.example/@alice">"#,
                "</head><body>",
                // Relative + multi-token rel, resolved against base.
                r#"<a rel="nofollow me" href="/profile">me</a>"#,
                // rel without "me" is ignored.
                r#"<a rel="nofollow" href="https://other.example/x">x</a>"#,
                "</body></html>",
            ),
            &base,
        );
        assert_eq!(
            backlinks,
            vec![
                "https://alice.example/profile".to_owned(),
                "https://social.example/@alice".to_owned(),
            ]
        );
    }

    #[test]
    fn html_content_types_only() {
        assert!(is_html("text/html"));
        assert!(is_html("Text/HTML; charset=utf-8"));
        assert!(!is_html("application/json"));
        assert!(!is_html(""));
    }
}
