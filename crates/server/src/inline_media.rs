//! Inline media identities and safe, already-sanitised audio/video fragments.
use std::{collections::HashMap, sync::LazyLock};

use plamenu_db::media::Media;
use regex::Regex;

pub(crate) static PLAYERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<(?:video|audio)\b[^>]*>.*?</(?:video|audio)>").unwrap());
static SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<(?:video|audio|source)\b[^>]*\bsrc="([^"]*)"[^>]*>"#).unwrap());
pub(crate) static SOURCE_TAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<source\b[^>]*>").unwrap());
static RESIZED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-[1-9][0-9]*x[1-9][0-9]*$").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?([a-z][a-z0-9]*)\b[^>]*>").unwrap());
pub(crate) static INLINE_IDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<(?:img|video|audio)\b[^>]*\bdata-media-id="([0-9]+)""#).unwrap()
});
pub(crate) static LINKED_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<a\s[^>]*>\s*(<img\s[^>]*class="status__inline-image"[^>]*>)\s*</a>"#)
        .unwrap()
});

/// Compile media patterns before entering deeply nested federation futures.
/// Lazy compilation inside the first gateway Create can exhaust a worker stack.
pub(crate) fn initialize() {
    for expression in [
        &PLAYERS,
        &SOURCE,
        &SOURCE_TAGS,
        &RESIZED,
        &TAG,
        &INLINE_IDS,
        &LINKED_IMAGE,
    ] {
        let _ = LazyLock::force(expression);
    }
}

pub(crate) fn player_source(player: &str) -> Option<String> {
    SOURCE
        .captures(player)
        .map(|m| plamenu_ap::text::decode_entities(&m[1]))
}

pub(crate) fn players(html: &str) -> Vec<(String, &'static str)> {
    if !html
        .as_bytes()
        .windows(6)
        .any(|tag| tag.eq_ignore_ascii_case(b"<video") || tag.eq_ignore_ascii_case(b"<audio"))
    {
        return Vec::new();
    }
    // Do not interpret tag-looking script strings as attachments.
    let cleaned = plamenu_ap::text::sanitize_remote_html_with_images(html);
    PLAYERS
        .find_iter(&cleaned)
        .filter_map(|m| {
            let kind = if m.as_str().starts_with("<audio") {
                "audio/mpeg"
            } else {
                "video/mp4"
            };
            player_source(m.as_str()).map(|url| (url, kind))
        })
        .collect()
}

/// `WordPress` publishes a resized attachment beside its inline full-size image.
/// Only its uploads directory and conventional generated suffixes qualify.
/// Origin, directory, extension and query remain part of the identity.
fn wordpress_image_identity(raw: &str) -> Option<String> {
    if !raw.contains("/wp-content/uploads/") {
        return None;
    }
    let mut url = url::Url::parse(raw).ok()?;
    let path = url.path();
    if !path.contains("/wp-content/uploads/") {
        return None;
    }
    let (stem, extension) = path.rsplit_once('.')?;
    if !matches!(extension, "png" | "jpg" | "jpeg" | "webp" | "avif" | "gif") {
        return None;
    }
    let stem = RESIZED.replace(stem, "");
    let stem = stem
        .strip_suffix("-scaled")
        .or_else(|| stem.strip_suffix("-rotated"))
        .unwrap_or(&stem);
    let canonical = format!("{stem}.{extension}");
    url.set_path(&canonical);
    Some(url.into())
}

/// Additive IDs, never origin URLs: clients retain privacy-preserving media URLs.
pub(crate) fn alias_ids(files: &[Media]) -> HashMap<i64, Vec<String>> {
    let mut groups: HashMap<String, Vec<i64>> = HashMap::new();
    for file in files {
        if file.kind_or_derived() == "image"
            && let Some(key) = file
                .remote_url
                .as_deref()
                .and_then(wordpress_image_identity)
        {
            groups.entry(key).or_default().push(file.id);
        }
    }
    groups
        .into_values()
        .filter(|ids| ids.len() > 1)
        .flat_map(|ids| {
            let strings: Vec<_> = ids.iter().map(ToString::to_string).collect();
            ids.into_iter().map(move |id| (id, strings.clone()))
        })
        .collect()
}

/// A full player contains flow content. Close/reopen surrounding phrasing
/// elements so inserting it into `<p>before <video>…</video> after</p>` does
/// not make the HTML parser detach controls or lose the following text.
pub(crate) fn render_players(html: &str, mut render: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    for player in PLAYERS.find_iter(html) {
        let Some(replacement) = render(player.as_str()) else {
            continue;
        };
        out.push_str(&html[cursor..player.start()]);
        let mut stack: Vec<(&str, &str)> = Vec::new();
        for tag in TAG.captures_iter(&html[..player.start()]) {
            let raw = tag.get(0).unwrap().as_str();
            let name = tag.get(1).unwrap().as_str();
            if raw.starts_with("</") {
                if let Some(at) = stack.iter().rposition(|(open, _)| *open == name) {
                    stack.truncate(at);
                }
            } else if !matches!(name, "br" | "img" | "source" | "hr" | "input" | "track") {
                stack.push((name, raw));
            }
        }
        let inline: Vec<_> = stack
            .iter()
            .rev()
            .take_while(|(name, _)| {
                matches!(
                    *name,
                    "p" | "span"
                        | "a"
                        | "abbr"
                        | "b"
                        | "code"
                        | "del"
                        | "em"
                        | "i"
                        | "s"
                        | "strong"
                        | "u"
                )
            })
            .collect();
        for (name, _) in &inline {
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        out.push_str(&replacement);
        for (_, raw) in inline.iter().rev() {
            out.push_str(raw);
        }
        cursor = player.end();
    }
    out.push_str(&html[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wordpress_variants_share_identity_without_merging_other_files_or_origins() {
        let base = "https://example.test/wp-content/uploads/2025/06/gcp_status";
        assert_eq!(
            wordpress_image_identity(&format!("{base}-scaled.png")),
            wordpress_image_identity(&format!("{base}-1024x650.png"))
        );
        for other in [
            format!("{base}-different.png"),
            format!("{base}-1024x650.jpg"),
            format!("{base}-scaled.png?version=2"),
            "https://other.test/wp-content/uploads/2025/06/gcp_status.png".into(),
        ] {
            assert_ne!(
                wordpress_image_identity(&format!("{base}.png")),
                wordpress_image_identity(&other)
            );
        }
        assert!(
            wordpress_image_identity("https://example.test/images/photo-1024x650.png").is_none()
        );
    }
    #[test]
    fn inline_players_preserve_text_and_paragraph_structure() {
        let rendered = render_players(
            "<p>Before <em><video src=\"x\"></video> after</em>.</p>",
            |_| Some("<div>Player</div>".into()),
        );
        assert_eq!(
            rendered,
            "<p>Before <em></em></p><div>Player</div><p><em> after</em>.</p>"
        );
    }
}
