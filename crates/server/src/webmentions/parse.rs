//! Small bounded microformat reader; extracted values are plain text, never HTML.
use html5ever::tokenizer::states::RawKind;
use html5ever::{
    tendril::StrTendril,
    tokenizer::{TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts},
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Debug, thiserror::Error)]
#[error("Website microformats exceed parser limits")]
pub struct ParseError;
use url::Url;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub kind: String,
    pub name: String,
    pub summary: String,
    pub author: String,
    pub author_url: Option<String>,
    pub photo: Option<String>,
    pub published: Option<String>,
    pub categories: Vec<String>,
}
#[derive(Default)]
struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    parent: Option<usize>,
}
impl Node {
    fn attr(&self, name: &str) -> &str {
        self.attrs
            .iter()
            .find(|a| a.0 == name)
            .map_or("", |a| a.1.as_str())
    }
    fn class(&self, class: &str) -> bool {
        self.attr("class")
            .split_ascii_whitespace()
            .any(|s| s == class)
    }
    fn value(&self) -> &str {
        match self.tag.as_str() {
            "img" | "area" => self.attr("alt"),
            "data" | "input" => self.attr("value"),
            "time" if !self.attr("datetime").is_empty() => self.attr("datetime"),
            "abbr" if !self.attr("title").is_empty() => self.attr("title"),
            _ => &self.text,
        }
    }
}
#[derive(Default)]
struct Scan {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    overflow: bool,
    copied: usize,
}
#[derive(Default)]
struct Sink(RefCell<Scan>);
impl TokenSink for Sink {
    type Handle = ();
    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut scan = self.0.borrow_mut();
        if scan.overflow {
            return TokenSinkResult::Continue;
        }
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => {
                if scan.nodes.len() >= 16384 || scan.stack.len() >= 128 {
                    scan.overflow = true;
                    return TokenSinkResult::Continue;
                }
                let name = tag.name.to_string();
                let parent = scan.stack.last().copied();
                let index = scan.nodes.len();
                scan.nodes.push(Node {
                    tag: name.clone(),
                    attrs: tag
                        .attrs
                        .into_iter()
                        .map(|a| (a.name.local.to_string(), a.value.to_string()))
                        .collect(),
                    parent,
                    ..Node::default()
                });
                if !tag.self_closing
                    && !matches!(
                        name.as_str(),
                        "area"
                            | "base"
                            | "br"
                            | "col"
                            | "embed"
                            | "hr"
                            | "img"
                            | "input"
                            | "link"
                            | "meta"
                            | "param"
                            | "source"
                            | "track"
                            | "wbr"
                    )
                {
                    scan.stack.push(index);
                }
                match name.as_str() {
                    "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
                    "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                        return TokenSinkResult::RawData(RawKind::Rawtext);
                    }
                    "title" | "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
                    _ => {}
                }
            }
            Token::TagToken(tag) if tag.kind == TagKind::EndTag => {
                if let Some(pos) = scan
                    .stack
                    .iter()
                    .rposition(|i| scan.nodes[*i].tag == *tag.name)
                {
                    scan.stack.truncate(pos);
                }
            }
            Token::CharacterTokens(value)
                if !scan.stack.iter().any(|i| {
                    matches!(scan.nodes[*i].tag.as_str(), "script" | "style" | "template")
                }) =>
            {
                scan.copied = scan
                    .copied
                    .saturating_add(value.len().saturating_mul(scan.stack.len()));
                if scan.copied > 4 * 1024 * 1024 {
                    scan.overflow = true;
                    return TokenSinkResult::Continue;
                }
                let indices = scan.stack.clone();
                for index in indices {
                    if scan.nodes[index].text.len() < 65536 {
                        scan.nodes[index].text.push_str(&value);
                    }
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}
fn nodes(body: &str) -> Result<Vec<Node>, ParseError> {
    let tokenizer = Tokenizer::new(Sink::default(), TokenizerOpts::default());
    let input = html5ever::buffer_queue::BufferQueue::default();
    input.push_back(StrTendril::from(body));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let scan = tokenizer.sink.0.into_inner();
    if scan.overflow {
        Err(ParseError)
    } else {
        Ok(scan.nodes)
    }
}
fn plain(value: &str, max: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}
fn http_url(base: &Url, value: &str) -> Option<String> {
    let url = base.join(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.into())
}
fn reference(node: &Node) -> Option<&str> {
    match node.tag.as_str() {
        "a" | "area" | "link" => Some(node.attr("href")),
        "img" | "audio" | "video" | "source" | "track" => Some(node.attr("src")),
        _ => None,
    }
}
fn inside(nodes: &[Node], mut index: usize, root: usize) -> bool {
    loop {
        if index == root {
            return true;
        }
        let Some(parent) = nodes[index].parent else {
            return false;
        };
        index = parent;
    }
}
fn in_card(nodes: &[Node], mut index: usize) -> bool {
    loop {
        if nodes[index].class("h-card") {
            return true;
        }
        let Some(parent) = nodes[index].parent else {
            return false;
        };
        index = parent;
    }
}
/// Only backlinks in the chosen entry verify it; unrelated sidebar links cannot.
pub fn entry(body: &str, base: &Url, target: &str) -> Result<Option<Entry>, ParseError> {
    let nodes = nodes(body)?;
    let has_entries = nodes.iter().any(|n| n.class("h-entry"));
    let root = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            reference(n)
                .and_then(|value| http_url(base, value))
                .as_deref()
                == Some(target)
        })
        .find_map(|(mut index, _)| {
            loop {
                if nodes[index].class("h-entry") {
                    return Some(index);
                }
                let parent = nodes[index].parent?;
                index = parent;
            }
        });
    if has_entries && root.is_none() {
        return Ok(None);
    }

    let selected: Vec<_> = nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| root.is_none_or(|r| inside(&nodes, *i, r)))
        .collect();
    let backlinks: Vec<_> = selected
        .iter()
        .filter(|(_, n)| {
            reference(n)
                .and_then(|value| http_url(base, value))
                .as_deref()
                == Some(target)
        })
        .collect();
    if backlinks.is_empty() {
        return Ok(None);
    }
    let mut result = Entry {
        kind: "mention".into(),
        ..Entry::default()
    };
    for (class, kind) in [
        ("u-in-reply-to", "reply"),
        ("u-like-of", "like"),
        ("u-repost-of", "repost"),
        ("u-quotation-of", "quote"),
    ] {
        if backlinks.iter().any(|(_, n)| n.class(class)) {
            result.kind = kind.into();
            break;
        }
    }
    let value = |class: &str, max| {
        selected
            .iter()
            .find(|(i, n)| n.class(class) && !in_card(&nodes, *i))
            .map_or_else(String::new, |(_, n)| plain(n.value(), max))
    };
    result.name = value("p-name", 240);
    result.summary = value("p-summary", 240);
    if result.summary.is_empty() {
        result.summary = value("e-content", 240);
    }
    result.published = selected
        .iter()
        .find(|(_, n)| n.class("dt-published"))
        .map(|(_, n)| plain(n.value(), 100));
    result.categories = selected
        .iter()
        .filter(|(_, n)| n.class("p-category"))
        .take(32)
        .map(|(_, n)| plain(n.value(), 100))
        .filter(|s| !s.is_empty())
        .collect();
    fill_author(&mut result, &nodes, &selected, base);
    Ok(Some(result))
}
fn fill_author(result: &mut Entry, nodes: &[Node], selected: &[(usize, &Node)], base: &Url) {
    if let Some((card, _)) = selected
        .iter()
        .find(|(_, n)| n.class("h-card") && n.class("p-author"))
        .copied()
        .or_else(|| selected.iter().find(|(_, n)| n.class("h-card")).copied())
    {
        let fields: Vec<_> = nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| inside(nodes, *i, card))
            .collect();
        result.author = fields.iter().find(|(_, n)| n.class("p-name")).map_or_else(
            || plain(nodes[card].value(), 100),
            |(_, n)| plain(n.value(), 100),
        );
        result.author_url = fields
            .iter()
            .find(|(_, n)| n.class("u-url"))
            .and_then(|(_, n)| http_url(base, n.attr("href")));
        result.photo = fields
            .iter()
            .find(|(_, n)| n.class("u-photo"))
            .and_then(|(_, n)| http_url(base, n.attr("src")));
    }
}
#[must_use]
pub fn endpoint(body: &str, headers: &[String], base: &Url) -> Option<String> {
    for header in headers {
        for value in header.split(',') {
            let Some((address, params)) = value.trim().split_once('>') else {
                continue;
            };
            if params
                .split(';')
                .filter_map(|s| s.trim().split_once('='))
                .any(|(key, val)| {
                    key.eq_ignore_ascii_case("rel")
                        && val
                            .trim_matches('"')
                            .split_ascii_whitespace()
                            .any(|r| r == "webmention")
                })
                && let Some(url) = address.strip_prefix('<').and_then(|s| http_url(base, s))
            {
                return Some(url);
            }
        }
    }
    nodes(body)
        .ok()?
        .iter()
        .filter(|n| {
            matches!(n.tag.as_str(), "a" | "link")
                && n.attr("rel")
                    .split_ascii_whitespace()
                    .any(|s| s == "webmention")
        })
        .find_map(|n| http_url(base, n.attr("href")))
}
/// Public hyperlinks only; capability query strings are never sent as mentions.
#[must_use]
pub fn targets(body: &str, base: &Url) -> Vec<String> {
    let mut values: Vec<_> = nodes(body)
        .unwrap_or_default()
        .iter()
        .filter(|n| n.tag == "a")
        .filter_map(|n| http_url(base, n.attr("href")))
        .filter(|s| {
            Url::parse(s).is_ok_and(|u| {
                u.query().is_none() && u.fragment().is_none() && u.origin() != base.origin()
            })
        })
        .take(100)
        .collect();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_extracts_plain_microformats_and_excludes_sidebar_backlinks() {
        let base = Url::parse("https://site.example/post").unwrap();
        let html = r#"<a href="https://local.example/music/1">sidebar</a><article class="h-entry"><span class="p-author h-card"><a class="u-url p-name" href="/alice">Alice</a><img class="u-photo" src="/avatar.png"></span><h1 class="p-name">A song</h1><div class="e-content">Hello &amp; <b>thanks</b><script>evil()</script></div><time class="dt-published" datetime="2026-10-10T00:00:00Z"></time><a class="u-like-of" href="https://local.example/music/1">song</a><a class="p-category">Jazz</a></article>"#;
        let item = entry(html, &base, "https://local.example/music/1")
            .unwrap()
            .unwrap();
        assert_eq!(item.kind, "like");
        assert_eq!(item.author, "Alice");
        assert_eq!(item.name, "A song");
        assert_eq!(item.summary, "Hello & thanks");
        assert_eq!(
            item.author_url.as_deref(),
            Some("https://site.example/alice")
        );
        assert!(
            entry(
                &html.replace(
                    "class=\"u-like-of\" href=\"https://local.example/music/1\"",
                    "href=\"/elsewhere\""
                ),
                &base,
                "https://local.example/music/1"
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn parser_limits_fail_closed_and_script_links_do_not_verify_a_source() {
        let base = Url::parse("https://site.example/post").unwrap();
        let target = "https://local.example/song";
        assert!(entry(&format!("<article class=h-entry><script>const fake=\"<a href='{target}'>fake</a>\"</script></article>"),&base,target).unwrap().is_none());
        assert!(entry(&"<div>".repeat(130), &base, target).is_err());
    }
    #[test]
    fn matching_entry_is_selected_after_unrelated_entries() {
        let base = Url::parse("https://site.example/post").unwrap();
        let target = "https://local.example/song";
        let html = format!(
            "<article class=h-entry><h1 class=p-name>Unrelated</h1></article><article class=h-entry><h1 class=p-name>Actual</h1><a class=u-like-of href='{target}'>Like</a></article>"
        );
        let item = entry(&html, &base, target).unwrap().unwrap();
        assert_eq!(item.name, "Actual");
        assert_eq!(item.kind, "like");
    }
    #[test]
    fn embedded_media_references_are_mentions_without_a_fabricated_actor() {
        let base = Url::parse("https://site.example/post").unwrap();
        let target = "https://local.example/music/assets/1/stream";
        assert_eq!(
            entry(
                &format!(
                    "<article class=h-entry><audio src='{target}' controls></audio></article>"
                ),
                &base,
                target
            )
            .unwrap()
            .unwrap()
            .kind,
            "mention"
        );
    }
    #[test]
    fn discovery_prefers_http_link_and_rejects_non_http_endpoints() {
        let base = Url::parse("https://site.example/post").unwrap();
        assert_eq!(
            endpoint(
                "",
                &["<../mention>; rel=\"alternate webmention\"".into()],
                &base
            )
            .as_deref(),
            Some("https://site.example/mention")
        );
        assert!(endpoint("<link rel=webmention href=javascript:alert(1)>", &[], &base).is_none());
    }
}
