//! Per-account Atom syndication feed — interop with the non-ActivityPub
//! web (RSS/Atom readers, `IndieWeb`). Served at `/@name.atom`, autodiscovered
//! from the profile page's `rel=alternate` link. Public, anonymous and
//! read-only: it carries the same public post set a logged-out visitor sees on
//! the profile page, excluding replies and boosts — Mastodon's per-account RSS
//! shape (`without_reblogs.without_replies`), rendered as Atom 1.0 (RFC 4287).

use std::fmt::Write as _;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use plamenu_db::status;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::i18n::Locale;
use super::pages::resolve_handle;
use crate::entities::{account_json, render_statuses};
use crate::error::ApiError;
use crate::filters::plain_text;
use crate::state::AppState;

/// How many recent posts the feed carries — Mastodon's RSS `PAGE_SIZE`.
const FEED_LIMIT: i64 = 20;
/// Longest derived entry title before it is elided; keeps a title one line.
const TITLE_MAX_CHARS: usize = 120;

/// `GET /@name.atom` — a local account's Atom 1.0 feed. The `.atom` suffix is
/// stripped by [`super::pages::profile`] before we get here, so `handle` is the
/// bare `@name` (or `@name@domain`) segment. `locale` shapes the one derived
/// string (the media-only entry-title fallback); feed readers rarely send
/// `Accept-Language`, so it usually stays English.
pub async fn account_atom(
    state: &AppState,
    handle: &str,
    locale: Locale,
) -> Result<Response, ApiError> {
    let Some(account) = resolve_handle(state, handle).await? else {
        return Err(ApiError::NotFound);
    };
    // Feeds are a local-account affordance; a remote profile has its own on its
    // origin, and we don't syndicate mirrors.
    if !account.has_local_account_on(&state.config.domain) {
        return Err(ApiError::NotFound);
    }
    if account.suspended() {
        return Err(ApiError::Forbidden("This account is suspended".into()));
    }
    let domain = &state.config.domain;
    // Anonymous public view (viewer = None): top-level own posts only.
    let filter = status::AccountStatusesFilter {
        exclude_replies: true,
        exclude_reblogs: true,
        only_media: false,
        media_through_reblog: false,
        tagged: None,
        max_id: None,
        since_id: None,
    };
    // A syndication feed reads in publish order (the default); for a local
    // author — the only kind we syndicate — it coincides with ingest order.
    let statuses = status::by_account(
        &state.pool,
        account.id,
        None,
        &filter,
        plamenu_db::user::TimelineOrder::default(),
        FEED_LIMIT,
    )
    .await?;
    let entries = render_statuses(&state.pool, domain, &statuses, None).await?;
    let account_value = account_json(&state.pool, domain, &account, None).await?;
    let xml = build_feed(domain, &account_value, &entries, locale);
    Ok(([(header::CONTENT_TYPE, plamenu_ap::ATOM_XML_UTF8)], xml).into_response())
}

/// A string field off a rendered entity, empty when absent/null.
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A rendered status' modification time for Atom `<updated>`: the edit time
/// when it has one, otherwise its creation time.
fn entry_updated(entry: &Value) -> &str {
    let edited = field(entry, "edited_at");
    if edited.is_empty() {
        field(entry, "created_at")
    } else {
        edited
    }
}

/// Atom requires every entry to carry a title, but a microblog post has none —
/// so derive one the way a feed reader would want it: the content warning if
/// there is one, else a one-line snippet of the post text, else a stand-in for
/// a media-only post.
fn entry_title(entry: &Value, author_name: &str, locale: Locale) -> String {
    let spoiler = field(entry, "spoiler_text").trim();
    if !spoiler.is_empty() {
        return elide(spoiler);
    }
    let text = plain_text(field(entry, "content"));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        let mut args = FluentArgs::new();
        args.set("author", author_name);
        locale.plain_with("feed-post-by", &args)
    } else {
        elide(&text)
    }
}

/// Truncates to [`TITLE_MAX_CHARS`] on a char boundary, appending an ellipsis.
fn elide(text: &str) -> String {
    if text.chars().count() <= TITLE_MAX_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(TITLE_MAX_CHARS).collect();
    out.push('…');
    out
}

/// Escapes text for inclusion in XML character data or an attribute value, and
/// drops the control characters XML 1.0 forbids so a stray byte in
/// remote-ingested content can never yield an unparseable feed.
fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Renders the whole Atom document. `entries` are newest-first, as
/// [`status::by_account`] returns them.
fn build_feed(domain: &str, account: &Value, entries: &[Value], locale: Locale) -> String {
    let name = field(account, "display_name");
    let username = field(account, "username");
    let title = if name.is_empty() {
        format!("@{username}")
    } else {
        name.to_owned()
    };
    let profile_url = field(account, "url");
    let actor_uri = field(account, "uri");
    let avatar = field(account, "avatar");
    let bio = plain_text(field(account, "note"));
    let bio = bio.split_whitespace().collect::<Vec<_>>().join(" ");
    let feed_url = plamenu_ap::urls::account_atom_url(domain, username);
    // Atom requires a feed-level `<updated>`; use the newest post, falling back
    // to now for an empty feed.
    let updated = entries.first().map_or_else(
        || {
            OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default()
        },
        |entry| entry_updated(entry).to_owned(),
    );

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    let _ = writeln!(xml, "  <id>{}</id>", xml_escape(actor_uri));
    let _ = writeln!(xml, "  <title>{}</title>", xml_escape(&title));
    if !bio.is_empty() {
        let _ = writeln!(xml, "  <subtitle>{}</subtitle>", xml_escape(&bio));
    }
    let _ = writeln!(xml, "  <updated>{}</updated>", xml_escape(&updated));
    let _ = writeln!(
        xml,
        "  <link rel=\"self\" type=\"application/atom+xml\" href=\"{}\"/>",
        xml_escape(&feed_url)
    );
    let _ = writeln!(
        xml,
        "  <link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>",
        xml_escape(profile_url)
    );
    xml.push_str("  <author>\n");
    let _ = writeln!(xml, "    <name>{}</name>", xml_escape(&title));
    let _ = writeln!(xml, "    <uri>{}</uri>", xml_escape(profile_url));
    xml.push_str("  </author>\n");
    if !avatar.is_empty() {
        let _ = writeln!(xml, "  <icon>{}</icon>", xml_escape(avatar));
    }
    let _ = writeln!(
        xml,
        "  <generator uri=\"https://{domain}/\">Plamenu</generator>"
    );
    for entry in entries {
        write_entry(&mut xml, entry, &title, locale);
    }
    xml.push_str("</feed>\n");
    xml
}

/// One `<entry>`. The rendered `content` is already-sanitized HTML; embedded as
/// escaped text inside `<content type="html">` a reader un-escapes and renders
/// it (so the feed document itself stays well-formed regardless of the HTML).
fn write_entry(xml: &mut String, entry: &Value, author_name: &str, locale: Locale) {
    let uri = field(entry, "uri");
    let url = field(entry, "url");
    let created = field(entry, "created_at");
    let spoiler = field(entry, "spoiler_text").trim();
    xml.push_str("  <entry>\n");
    let _ = writeln!(xml, "    <id>{}</id>", xml_escape(uri));
    let _ = writeln!(
        xml,
        "    <title>{}</title>",
        xml_escape(&entry_title(entry, author_name, locale))
    );
    let _ = writeln!(
        xml,
        "    <link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>",
        xml_escape(url)
    );
    let _ = writeln!(xml, "    <published>{}</published>", xml_escape(created));
    let _ = writeln!(
        xml,
        "    <updated>{}</updated>",
        xml_escape(entry_updated(entry))
    );
    // Hashtags become Atom categories, the way Mastodon's RSS emits them.
    if let Some(tags) = entry.get("tags").and_then(Value::as_array) {
        for tag in tags {
            let term = field(tag, "name");
            if !term.is_empty() {
                let _ = writeln!(xml, "    <category term=\"{}\"/>", xml_escape(term));
            }
        }
    }
    // A content warning rides as the entry summary, so a reader can gate the
    // body behind it exactly as the timeline does.
    if !spoiler.is_empty() {
        let _ = writeln!(
            xml,
            "    <summary type=\"text\">{}</summary>",
            xml_escape(spoiler)
        );
    }
    let _ = writeln!(
        xml,
        "    <content type=\"html\">{}</content>",
        xml_escape(field(entry, "content"))
    );
    xml.push_str("  </entry>\n");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Locale, elide, entry_title, xml_escape};

    #[test]
    fn xml_escape_encodes_markup_and_drops_control_bytes() {
        assert_eq!(
            xml_escape("<p>a & b \"c\" 'd'</p>"),
            "&lt;p&gt;a &amp; b &quot;c&quot; &apos;d&apos;&lt;/p&gt;"
        );
        // Tab/newline survive; other C0 controls are dropped, not emitted raw.
        assert_eq!(xml_escape("a\tb\nc\u{0}d\u{7}"), "a\tb\ncd");
    }

    #[test]
    fn elide_truncates_on_a_char_boundary() {
        let long = "é".repeat(200);
        let out = elide(&long);
        assert_eq!(out.chars().count(), 121); // 120 + the ellipsis
        assert!(out.ends_with('…'));
        assert_eq!(elide("short"), "short");
    }

    #[test]
    fn entry_title_prefers_the_content_warning_then_the_text() {
        let locale = Locale::default();
        let cw = json!({ "spoiler_text": "spoilers ahead", "content": "<p>body</p>" });
        assert_eq!(entry_title(&cw, "Alice", locale), "spoilers ahead");

        let plain = json!({ "spoiler_text": "", "content": "<p>just text</p>" });
        assert_eq!(entry_title(&plain, "Alice", locale), "just text");

        // A media-only post (empty rendered text) falls back to the author.
        let media = json!({ "spoiler_text": "", "content": "" });
        assert_eq!(entry_title(&media, "Alice", locale), "Post by Alice");
    }
}
