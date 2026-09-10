//! Custom-emoji vocabulary: the `:shortcode:` scanner and the `Emoji` tag
//! objects notes and actors carry, shaped the way Mastodon publishes them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::actor::Image;

/// Mastodon's local shortcode length cap.
pub const MAX_SHORTCODE_LEN: usize = 128;
/// Mastodon accepts longer shortcodes from other servers.
pub const MAX_FEDERATED_SHORTCODE_LEN: usize = 2048;

/// Whether `code` is a well-formed shortcode (`[a-zA-Z0-9_]{2,}`, length
/// capped by `max_len`) — Mastodon's `SHORTCODE_ONLY_RE` plus its length
/// validations.
#[must_use]
pub fn is_valid_shortcode(code: &str, max_len: usize) -> bool {
    code.len() >= 2
        && code.len() <= max_len
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Walks the `:shortcode:` emoji references of `text` the way Mastodon's
/// `CustomEmoji::SCAN_RE` does: the shortcode is `[a-zA-Z0-9_]{2,}` between
/// colons, and the reference must not butt against an alphanumeric character
/// or another colon on either side (so `2:30:45` and `::notemoji::` don't
/// match). Every occurrence is visited, in order, with its byte range
/// (colons included) and the bare shortcode.
fn each_reference<'a>(text: &'a str, mut visit: impl FnMut(core::ops::Range<usize>, &'a str)) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        let boundary_ok = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != ':');
        if !boundary_ok(text[..i].chars().next_back()) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end - start >= 2
            && bytes.get(end) == Some(&b':')
            && boundary_ok(text[end + 1..].chars().next())
        {
            visit(i..end + 1, &text[start..end]);
            // The closing colon may not double as the next opening one
            // (the lookbehind would reject it anyway: 'c' precedes it).
            i = end + 1;
        } else {
            i += 1;
        }
    }
}

/// Scans text for `:shortcode:` emoji references (the [`each_reference`]
/// matcher). First-appearance order, deduplicated.
#[must_use]
pub fn scan_shortcodes(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    each_reference(text, |_, code| {
        if !found.contains(&code) {
            found.push(code);
        }
    });
    found
}

/// Rewrites every `:shortcode:` reference (the same matcher as
/// [`scan_shortcodes`]) through `replacement`; `None` keeps the reference
/// as written. What display-side emojification is built on.
pub fn replace_shortcodes(
    text: &str,
    mut replacement: impl FnMut(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    each_reference(text, |range, code| {
        if let Some(replaced) = replacement(code) {
            out.push_str(&text[copied..range.start]);
            out.push_str(&replaced);
            copied = range.end;
        }
    });
    out.push_str(&text[copied..]);
    out
}

/// An `Emoji` tag entry on a Note or actor document, Mastodon's shape:
/// `{id, type: "Emoji", name: ":shortcode:", updated, icon}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmojiTag {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// `:shortcode:`, colons included.
    pub name: String,
    /// RFC 3339; remote servers use it to refresh their copy.
    pub updated: String,
    pub icon: Image,
}

impl EmojiTag {
    #[must_use]
    pub fn new(id: String, shortcode: &str, updated: String, icon: Image) -> Self {
        Self {
            id,
            kind: "Emoji".to_owned(),
            name: format!(":{shortcode}:"),
            updated,
            icon,
        }
    }

    /// As a JSON value, for mixing into a Note's heterogeneous `tag` array.
    #[must_use]
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("EmojiTag serializes infallibly")
    }
}

/// The URL of a local emoji's `ActivityPub` object.
#[must_use]
pub fn emoji_url(domain: &str, emoji_id: i64) -> String {
    format!("https://{domain}/emojis/{emoji_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_shortcodes_like_mastodon() {
        assert_eq!(scan_shortcodes("hello :blobcat: world"), ["blobcat"]);
        assert_eq!(scan_shortcodes(":blobcat:"), ["blobcat"]);
        assert_eq!(
            scan_shortcodes(":a_b2: x :a_b2: :other:"),
            ["a_b2", "other"]
        );
        // Adjacent references separated by a space both match.
        assert_eq!(scan_shortcodes(":one: :two:"), ["one", "two"]);
        // HTML context: tags delimit fine.
        assert_eq!(scan_shortcodes("<p>:blobcat:</p>"), ["blobcat"]);
        // Newlines count as boundaries.
        assert_eq!(scan_shortcodes("x\n:blobcat:\ny"), ["blobcat"]);
    }

    #[test]
    fn rejects_non_matches_like_mastodon() {
        // Too short.
        assert!(scan_shortcodes(":a:").is_empty());
        // Times and ratios: digits butt against the colons.
        assert!(scan_shortcodes("2:30:45").is_empty());
        // Letters butting against the opening colon.
        assert!(scan_shortcodes("ab:cd:").is_empty());
        // Double colons disqualify both sides.
        assert!(scan_shortcodes("::shrug::").is_empty());
        assert!(scan_shortcodes(":one::two:").is_empty());
        // Invalid shortcode characters break the run.
        assert!(scan_shortcodes(":blob cat:").is_empty());
        assert!(scan_shortcodes(":blob-cat:").is_empty());
        // Unicode letters count as alphanumeric boundaries.
        assert!(scan_shortcodes("é:blobcat:").is_empty());
        assert!(scan_shortcodes(":blobcat:é").is_empty());
        // Unterminated.
        assert!(scan_shortcodes(":blobcat").is_empty());
        assert!(scan_shortcodes("").is_empty());
    }

    #[test]
    fn replaces_shortcodes_through_the_callback() {
        let swap = |text: &str| {
            replace_shortcodes(text, |code| {
                (code == "blobcat").then(|| format!("<img alt=\":{code}:\">"))
            })
        };
        // Every occurrence is rewritten, not just the first.
        assert_eq!(
            swap("hi :blobcat: bye :blobcat:"),
            "hi <img alt=\":blobcat:\"> bye <img alt=\":blobcat:\">"
        );
        // Unknown shortcodes stay as written.
        assert_eq!(
            swap("hi :other: :blobcat:"),
            "hi :other: <img alt=\":blobcat:\">"
        );
        // Non-references are untouched.
        assert_eq!(swap("at 2:30:45 :blobcat"), "at 2:30:45 :blobcat");
        assert_eq!(swap(""), "");
    }

    #[test]
    fn validates_shortcodes() {
        assert!(is_valid_shortcode("ab", MAX_SHORTCODE_LEN));
        assert!(is_valid_shortcode("blob_cat_99", MAX_SHORTCODE_LEN));
        assert!(!is_valid_shortcode("a", MAX_SHORTCODE_LEN));
        assert!(!is_valid_shortcode("has space", MAX_SHORTCODE_LEN));
        assert!(!is_valid_shortcode("has-dash", MAX_SHORTCODE_LEN));
        assert!(!is_valid_shortcode("ünïcode", MAX_SHORTCODE_LEN));
        assert!(!is_valid_shortcode(&"x".repeat(129), MAX_SHORTCODE_LEN));
        assert!(is_valid_shortcode(
            &"x".repeat(129),
            MAX_FEDERATED_SHORTCODE_LEN
        ));
    }

    #[test]
    fn emoji_tag_serializes_with_mastodons_wire_names() {
        let tag = EmojiTag::new(
            emoji_url("plamenu.local", 7),
            "blobcat",
            "2026-06-12T00:00:00Z".to_owned(),
            Image::new("https://plamenu.local/media/7.png".to_owned(), "image/png"),
        );
        let value = tag.into_value();
        assert_eq!(value["id"], "https://plamenu.local/emojis/7");
        assert_eq!(value["type"], "Emoji");
        assert_eq!(value["name"], ":blobcat:");
        assert_eq!(value["updated"], "2026-06-12T00:00:00Z");
        assert_eq!(value["icon"]["type"], "Image");
        assert_eq!(value["icon"]["mediaType"], "image/png");
        assert_eq!(value["icon"]["url"], "https://plamenu.local/media/7.png");
    }
}
