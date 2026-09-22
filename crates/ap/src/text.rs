//! Plain-text to HTML conversion for locally-authored content, and
//! sanitization of remote HTML.

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::Builder;

/// Sanitizer for remote status content: roughly Mastodon's allowlist —
/// inline markup, links, lists, block quotes and code, nothing else.
/// `class` survives on `a`/`span` so mention/hashtag markup keeps rendering.
static REMOTE_HTML: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder
        .tags(HashSet::from([
            "p",
            "br",
            "span",
            "a",
            "abbr",
            "del",
            "s",
            "pre",
            "blockquote",
            "code",
            "b",
            "strong",
            "u",
            "i",
            "em",
            "ul",
            "ol",
            "li",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
        ]))
        .add_tag_attributes("a", ["class"])
        .add_tag_attributes("span", ["class"])
        .add_tag_attributes("ol", ["start"])
        .url_schemes(HashSet::from(["http", "https"]))
        .link_rel(Some("nofollow noopener noreferrer"));
    builder
});

/// The remote-content sanitizer used while resolving inline Article images.
///
/// This is deliberately separate from [`REMOTE_HTML`]: callers must replace
/// every surviving remote `src` with a same-origin media URL before storing
/// the result. Keeping the ordinary sanitizer image-free prevents an
/// accidental origin request anywhere that does not perform that rewrite.
static REMOTE_HTML_WITH_IMAGES: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder
        .tags(HashSet::from([
            "p",
            "br",
            "span",
            "a",
            "abbr",
            "del",
            "s",
            "pre",
            "blockquote",
            "code",
            "b",
            "strong",
            "u",
            "i",
            "em",
            "ul",
            "ol",
            "li",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "img",
        ]))
        .add_tag_attributes("a", ["class"])
        .add_tag_attributes("span", ["class"])
        .add_tag_attributes("ol", ["start"])
        .add_tag_attributes("img", ["src", "alt"])
        .url_schemes(HashSet::from(["http", "https"]))
        .link_rel(Some("nofollow noopener noreferrer"));
    builder
});

/// Sanitizer for preview-card embed HTML, mirroring Mastodon's
/// `MASTODON_OEMBED` config: only the media-embedding elements survive, and
/// every iframe is forced into a sandbox.
static OEMBED_HTML: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder
        .tags(HashSet::from(["audio", "iframe", "source", "video"]))
        .tag_attributes(std::collections::HashMap::from([
            ("audio", HashSet::from(["controls"])),
            (
                "iframe",
                HashSet::from([
                    "allowfullscreen",
                    "frameborder",
                    "height",
                    "scrolling",
                    "src",
                    "width",
                ]),
            ),
            ("source", HashSet::from(["src", "type"])),
            (
                "video",
                HashSet::from(["controls", "height", "loop", "width"]),
            ),
        ]))
        .url_schemes(HashSet::from(["http", "https"]))
        .set_tag_attribute_value(
            "iframe",
            "sandbox",
            "allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox \
             allow-forms",
        );
    builder
});

/// Sanitizes oEmbed/player HTML before it is stored on a preview card.
#[must_use]
pub fn sanitize_oembed_html(html: &str) -> String {
    OEMBED_HTML.clean(html).to_string()
}

/// Stripper for remote plain-text-ish fields (display names): no markup at
/// all, entities decoded and re-escaped safely.
static REMOTE_TEXT: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder.tags(HashSet::new());
    builder
});

/// Sanitizes HTML received from other servers before storing/serving it.
#[must_use]
pub fn sanitize_remote_html(html: &str) -> String {
    REMOTE_HTML.clean(html).to_string()
}

/// Sanitizes remote HTML while temporarily retaining image sources and alt
/// text. The result is safe markup except for privacy: its image URLs still
/// name the remote origin. A caller must rewrite or remove every `<img>` before
/// storing or serving it.
#[must_use]
pub fn sanitize_remote_html_with_images(html: &str) -> String {
    REMOTE_HTML_WITH_IMAGES.clean(html).to_string()
}

/// Reduces remote rich text to safe inline text (for display names).
#[must_use]
pub fn sanitize_remote_text(text: &str) -> String {
    REMOTE_TEXT.clean(text).to_string()
}

/// Reduces remote rich text to *plain* text: markup stripped and entities
/// decoded, so what is stored is the literal characters (`&`, not `&amp;`).
/// For fields stored and served as plain text (status titles, event
/// locations) — [`sanitize_remote_text`] output stays HTML-encoded and is
/// meant for HTML embedding.
#[must_use]
pub fn sanitize_remote_plain(text: &str) -> String {
    decode_entities(&sanitize_remote_text(text))
}

/// Decodes the HTML entities a sanitizer's text output can carry (named
/// basics plus numeric references).
#[must_use]
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(';').filter(|&e| e <= 32) else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix(['x', 'X']).map_or_else(
                        || n.parse::<u32>().ok(),
                        |h| u32::from_str_radix(h, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &rest[start + end + 1..];
        } else {
            out.push('&');
            rest = &rest[start + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// Escapes plain text and wraps it Mastodon-style: paragraphs from blank
/// lines, `<br />` for single newlines.
#[must_use]
pub fn plain_to_html(text: &str) -> String {
    inline_html_to_paragraphs(&escape_html(text))
}

/// Wraps already-escaped inline HTML (which may still contain raw `\n`
/// characters) into paragraphs: blank lines split `<p>`s, single newlines
/// become `<br />`. The compose pipeline uses this after rendering mention
/// and hashtag anchors.
#[must_use]
pub fn inline_html_to_paragraphs(inline: &str) -> String {
    let mut html = String::with_capacity(inline.len() + 16);
    for (i, paragraph) in inline
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .enumerate()
    {
        if i > 0 {
            html.push_str("</p><p>");
        }
        for (j, line) in paragraph.trim().lines().enumerate() {
            if j > 0 {
                html.push_str("<br />");
            }
            html.push_str(line);
        }
    }
    if html.is_empty() {
        String::new()
    } else {
        format!("<p>{html}</p>")
    }
}

/// Escapes text for safe embedding in HTML content or attribute values.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    escape_into(&mut out, text);
    out
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

/// Mastodon's `TextFormatter.shortened_link`: the scheme (and any `www.`) is
/// hidden in an `invisible` span, the next 30 characters are displayed, and a
/// longer remainder is hidden again with the visible part classed `ellipsis`.
/// Shared by the compose pipeline and the outgoing quote fallback so both emit
/// the same anchor shape.
#[must_use]
pub fn shortened_link_anchor(url: &str) -> String {
    let scheme_len = url.find("://").map_or(0, |i| i + 3);
    let prefix_len = if url[scheme_len..].starts_with("www.") {
        scheme_len + 4
    } else {
        scheme_len
    };
    let rest = &url[prefix_len..];
    let mut display_end = rest
        .char_indices()
        .nth(30)
        .map_or(rest.len(), |(offset, _)| offset);
    let mut cutoff = display_end < rest.len();
    // A one-character remainder is shorter than the ellipsis it would hide
    // behind; show it instead (Mastodon's quirk).
    if cutoff && rest[display_end..].chars().count() == 1 {
        display_end = rest.len();
        cutoff = false;
    }
    format!(
        r#"<a href="{href}" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">{prefix}</span><span class="{class}">{display}</span><span class="invisible">{suffix}</span></a>"#,
        href = escape_html(url),
        prefix = escape_html(&url[..prefix_len]),
        class = if cutoff { "ellipsis" } else { "" },
        display = escape_html(&rest[..display_end]),
        suffix = escape_html(&rest[display_end..]),
    )
}

/// Removes the compatibility link from the rendered content of an accepted
/// quote. The original stored content stays untouched: pending, rejected and
/// revoked quotes still need this link when there is no native quote card.
///
/// Deployed senders put the fallback in several places: Mastodon prepends a
/// `p.quote-inline`, Pleroma appends a `span.quote-inline`, Mitra appends an
/// unclassed `RE:` paragraph, and Misskey-family content can put the `RE:` link
/// after a final `<br>`. Marker elements are safe to remove directly. The
/// unmarked forms are removed only at a content edge and only when their anchor
/// names this quote's exact target, so an author's ordinary `RE:` text or link
/// is never mistaken for protocol scaffolding.
#[must_use]
pub fn strip_quote_fallback(html: &str, quoted_links: &[impl AsRef<str>]) -> String {
    let mut html = html.to_string();
    strip_marked_quote_elements(&mut html);
    strip_edge_quote_paragraphs(&mut html, quoted_links);
    strip_trailing_quote_line(&mut html, quoted_links);
    html
}

fn strip_marked_quote_elements(html: &mut String) {
    let mut search_from = 0;
    while let Some(marker_rel) = html[search_from..].find("quote-inline") {
        let marker = search_from + marker_rel;
        let Some(open_start) = html[..marker].rfind('<') else {
            break;
        };
        if html[open_start..marker].contains('>') {
            search_from = marker + "quote-inline".len();
            continue;
        }
        let Some(open_gt_rel) = html[open_start..].find('>') else {
            break;
        };
        let open_gt = open_start + open_gt_rel;
        let open_tag = &html[open_start..=open_gt];
        let Some(tag) = ["p", "span"].into_iter().find(|tag| {
            open_tag.get(1..).is_some_and(|rest| {
                rest.starts_with(tag) && rest[tag.len()..].starts_with([' ', '>'])
            })
        }) else {
            search_from = marker + "quote-inline".len();
            continue;
        };
        if !class_has_token(open_tag, "quote-inline") {
            search_from = marker + "quote-inline".len();
            continue;
        }
        let close = format!("</{tag}>");
        let body_start = open_gt + 1;
        let Some(close_rel) = html[body_start..].find(&close) else {
            break;
        };
        let close_end = body_start + close_rel + close.len();
        html.replace_range(open_start..close_end, "");
        search_from = open_start;
    }
}

fn class_has_token(tag: &str, token: &str) -> bool {
    let Some(class) = tag.find("class=") else {
        return false;
    };
    let rest = &tag[class + "class=".len()..];
    let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) else {
        return false;
    };
    let value = &rest[quote.len_utf8()..];
    let Some(end) = value.find(quote) else {
        return false;
    };
    value[..end]
        .split_ascii_whitespace()
        .any(|word| word == token)
}

fn quote_fallback_body(body: &str, quoted_links: &[impl AsRef<str>]) -> bool {
    let linked = quoted_links.iter().any(|quoted_link| {
        let quoted_link = quoted_link.as_ref();
        if quoted_link.is_empty() {
            return false;
        }
        let escaped = escape_html(quoted_link);
        [
            format!(r#"href="{quoted_link}""#),
            format!(r"href='{quoted_link}'"),
            format!(r#"href="{escaped}""#),
            format!(r"href='{escaped}'"),
        ]
        .iter()
        .any(|needle| body.contains(needle))
    });
    if !linked {
        return false;
    }
    let plain = sanitize_remote_plain(body);
    let plain = plain.trim_start_matches(char::is_whitespace);
    plain.starts_with("RE:") || plain.starts_with("RT:")
}

fn edge_paragraph(html: &str, leading: bool) -> Option<(usize, usize, usize, usize)> {
    let edge = if leading {
        html.len() - html.trim_start().len()
    } else {
        html.trim_end().len()
    };
    let open = if leading {
        (html[edge..].starts_with("<p") && html[edge + 2..].starts_with([' ', '>']))
            .then_some(edge)?
    } else {
        html[..edge].rfind("<p")?
    };
    let open_gt = open + html[open..].find('>')?;
    let close = open_gt + 1 + html[open_gt + 1..].find("</p>")?;
    let end = close + "</p>".len();
    if (leading && open != edge) || (!leading && end != edge) {
        return None;
    }
    Some((open, open_gt + 1, close, end))
}

fn strip_edge_quote_paragraphs(html: &mut String, quoted_links: &[impl AsRef<str>]) {
    for leading in [true, false] {
        let Some((open, body_start, body_end, end)) = edge_paragraph(html, leading) else {
            continue;
        };
        if quote_fallback_body(&html[body_start..body_end], quoted_links) {
            html.replace_range(open..end, "");
        }
    }
}

fn strip_trailing_quote_line(html: &mut String, quoted_links: &[impl AsRef<str>]) {
    let trimmed_end = html.trim_end().len();
    let Some(close) = html[..trimmed_end].strip_suffix("</p>").map(str::len) else {
        return;
    };
    let Some(last_br) = html[..close].rfind("<br") else {
        return;
    };
    let Some(line_start) = html[last_br..close].find('>').map(|end| last_br + end + 1) else {
        return;
    };
    if !quote_fallback_body(&html[line_start..close], quoted_links) {
        return;
    }
    // Remove every immediately-adjacent `<br>` that belongs to the fallback,
    // not merely the last one (Pleroma-style templates commonly use two).
    let mut remove_from = last_br;
    while let Some(previous) = html[..remove_from].rfind("<br") {
        let Some(previous_gt) = html[previous..remove_from].find('>') else {
            break;
        };
        if html[previous + previous_gt + 1..remove_from]
            .trim()
            .is_empty()
        {
            remove_from = previous;
        } else {
            break;
        }
    }
    html.replace_range(remove_from..close, "");
}

#[cfg(test)]
mod tests {
    use super::plain_to_html;

    #[test]
    fn escapes_and_wraps() {
        assert_eq!(plain_to_html("hello"), "<p>hello</p>");
        assert_eq!(
            plain_to_html("a < b & c > \"d\""),
            "<p>a &lt; b &amp; c &gt; &quot;d&quot;</p>"
        );
    }

    #[test]
    fn newlines_become_breaks_and_paragraphs() {
        assert_eq!(plain_to_html("one\ntwo"), "<p>one<br />two</p>");
        assert_eq!(plain_to_html("one\n\ntwo"), "<p>one</p><p>two</p>");
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(plain_to_html(""), "");
        assert_eq!(plain_to_html("\n\n  \n"), "");
    }

    #[test]
    fn script_injection_is_neutralized() {
        let html = plain_to_html("<script>alert('x')</script>");
        assert!(!html.contains('<') || !html.contains("<script"));
        assert_eq!(
            html,
            "<p>&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;</p>"
        );
    }

    #[test]
    fn remote_html_sanitizer_strips_dangerous_markup() {
        use super::sanitize_remote_html;
        // Scripts, event handlers, javascript: URLs and images are removed.
        assert_eq!(
            sanitize_remote_html("<p>hi<script>alert(1)</script></p>"),
            "<p>hi</p>"
        );
        assert_eq!(
            sanitize_remote_html(r#"<p onclick="evil()">x</p>"#),
            "<p>x</p>"
        );
        let js_link = sanitize_remote_html(r#"<a href="javascript:evil()">x</a>"#);
        assert!(!js_link.contains("javascript:"), "{js_link}");
        assert_eq!(
            sanitize_remote_html(r#"<img src="https://evil.example/track.png">"#),
            ""
        );
        // Unknown tags are unwrapped, not kept.
        assert_eq!(
            sanitize_remote_html("<style>p{}</style><iframe></iframe><p>ok</p>"),
            "<p>ok</p>"
        );
    }

    #[test]
    fn remote_html_sanitizer_keeps_mastodon_markup() {
        use super::sanitize_remote_html;
        let mention = r#"<p><span class="h-card"><a href="https://remote.example/@bob" class="u-url mention">@<span>bob</span></a></span> hello</p>"#;
        let cleaned = sanitize_remote_html(mention);
        assert!(cleaned.contains(r#"class="u-url mention""#), "{cleaned}");
        assert!(cleaned.contains(r#"rel="nofollow noopener noreferrer""#));
        assert!(cleaned.contains("hello"));
    }

    #[test]
    fn image_aware_remote_sanitizer_keeps_only_safe_image_fields() {
        use super::sanitize_remote_html_with_images;
        let cleaned = sanitize_remote_html_with_images(
            r#"<p><img src="https://media.example/a.png" alt="A &amp; B" onload="evil()"><img src="javascript:evil()" alt="bad"></p>"#,
        );
        assert!(cleaned.contains(r#"src="https://media.example/a.png""#));
        assert!(cleaned.contains(r#"alt="A &amp; B""#));
        assert!(!cleaned.contains("onload"));
        assert!(!cleaned.contains("javascript:"));
    }

    #[test]
    fn oembed_sanitizer_keeps_embeds_and_sandboxes_iframes() {
        use super::sanitize_oembed_html;
        let cleaned = sanitize_oembed_html(
            r#"<iframe src="https://player.example/v/1" width="640" height="360" frameborder="0" allowfullscreen="true" onload="evil()"></iframe><script>x()</script>"#,
        );
        assert!(
            cleaned.contains(r#"src="https://player.example/v/1""#),
            "{cleaned}"
        );
        assert!(cleaned.contains(r#"width="640""#));
        assert!(cleaned.contains("sandbox=\"allow-scripts"), "{cleaned}");
        assert!(!cleaned.contains("onload"));
        assert!(!cleaned.contains("script>"));
        // javascript: src is dropped with its iframe attribute.
        let js = sanitize_oembed_html(r#"<iframe src="javascript:evil()"></iframe>"#);
        assert!(!js.contains("javascript:"), "{js}");
    }

    #[test]
    fn remote_text_sanitizer_strips_all_markup() {
        use super::sanitize_remote_text;
        assert_eq!(sanitize_remote_text("<b>Bold</b> name"), "Bold name");
        assert_eq!(sanitize_remote_text("<script>x</script>safe"), "safe");
    }

    #[test]
    fn shortened_link_anchor_matches_mastodon_shape() {
        use super::shortened_link_anchor;
        // Short URL: scheme hidden, no ellipsis, empty trailing span.
        assert_eq!(
            shortened_link_anchor("https://example.com/about"),
            r#"<a href="https://example.com/about" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">example.com/about</span><span class="invisible"></span></a>"#
        );
        // `www.` joins the hidden prefix.
        assert_eq!(
            shortened_link_anchor("https://www.example.com/x"),
            r#"<a href="https://www.example.com/x" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://www.</span><span class="">example.com/x</span><span class="invisible"></span></a>"#
        );
        // Long URLs cut the display at 30 chars and hide the rest.
        let long = shortened_link_anchor("https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(
            long.contains(r#"<span class="ellipsis">example.com/aaaaaaaaaaaaaaaaaa</span><span class="invisible">aaaaaaaaaaaaaaaa</span>"#),
            "{long}"
        );
        // ...except when only one character would be hidden.
        let one_over = shortened_link_anchor("https://example.com/aaaaaaaaaaaaaaaaaaa");
        assert!(
            one_over.contains(r#"<span class="">example.com/aaaaaaaaaaaaaaaaaaa</span>"#),
            "{one_over}"
        );
    }

    #[test]
    fn strip_quote_fallback_handles_deployed_positions() {
        use super::strip_quote_fallback;
        let uri = "https://a.test/x";
        let links = [uri];
        // Mastodon prepends the fallback ahead of the real content.
        let mastodon = r#"<p class="quote-inline">RE: <a href="https://a.test/x" class="u-url">a.test/x</a></p><p>look at this</p>"#;
        assert_eq!(
            strip_quote_fallback(mastodon, &links),
            "<p>look at this</p>"
        );
        // Pleroma appends a marked span inside the author's last paragraph.
        let pleroma = r#"<p>look<span class="foo quote-inline"><br/><br/><bdi>RT:</bdi> <a href="https://a.test/x">https://a.test/x</a></span></p>"#;
        assert_eq!(strip_quote_fallback(pleroma, &links), "<p>look</p>");
        // Mitra deliberately leaves its trailing paragraph unclassed.
        let mitra = r#"<p>look</p><p>RE: <a href="https://a.test/x">https://a.test/x</a></p>"#;
        assert_eq!(strip_quote_fallback(mitra, &links), "<p>look</p>");
        // Misskey-family HTML can append the fallback as the final line.
        let sharkey = r#"<p>look<br>RE: <a href="https://a.test/x">https://a.test/x</a></p>"#;
        assert_eq!(strip_quote_fallback(sharkey, &links), "<p>look</p>");
        // Mastodon-compatible servers can put the target's human web URL in
        // the fallback while the structural quote uses its ActivityPub id.
        let canonical =
            r#"<p>RE: <a href="https://a.test/@alice/1">a.test/@alice/1</a></p><p>look</p>"#;
        assert_eq!(
            strip_quote_fallback(canonical, &[uri, "https://a.test/@alice/1"]),
            "<p>look</p>"
        );
        // Content that merely mentions the phrase is left intact.
        let plain = "<p>my quote-inline thoughts</p>";
        assert_eq!(strip_quote_fallback(plain, &links), plain);
        // A protocol-looking link to some other post is the author's content.
        let other = r#"<p>RE: <a href="https://a.test/other">other</a></p><p>hi</p>"#;
        assert_eq!(strip_quote_fallback(other, &links), other);
    }
}
