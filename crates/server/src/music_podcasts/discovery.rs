//! A podcast website can advertise its feed without the listener knowing RSS.
use html5ever::{
    tendril::StrTendril,
    tokenizer::{
        TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts, states::RawKind,
    },
};
use std::cell::RefCell;

#[derive(Default)]
struct Scan {
    html: bool,
    links: Vec<(bool, String)>,
}
#[derive(Default)]
struct Sink(RefCell<Scan>);
impl TokenSink for Sink {
    type Handle = ();
    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let Token::TagToken(tag) = token else {
            return TokenSinkResult::Continue;
        };
        if tag.kind != TagKind::StartTag {
            return TokenSinkResult::Continue;
        }
        match &*tag.name {
            "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                return TokenSinkResult::RawData(RawKind::Rawtext);
            }
            "title" | "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
            _ => {}
        }
        let mut scan = self.0.borrow_mut();
        if matches!(&*tag.name, "html" | "head") {
            scan.html = true;
        }
        if &*tag.name != "link" || scan.links.len() >= 8 {
            return TokenSinkResult::Continue;
        }
        let attr = |name: &str| {
            tag.attrs
                .iter()
                .find(|a| &*a.name.local == name)
                .map(|a| a.value.as_ref())
                .unwrap_or_default()
        };
        let kind = attr("type").split(';').next().unwrap_or_default().trim();
        if attr("rel")
            .split_ascii_whitespace()
            .any(|rel| rel.eq_ignore_ascii_case("alternate"))
            && (kind.eq_ignore_ascii_case("application/rss+xml")
                || kind.eq_ignore_ascii_case("application/atom+xml"))
        {
            scan.links.push((
                kind.eq_ignore_ascii_case("application/atom+xml"),
                attr("href").into(),
            ));
        }
        TokenSinkResult::Continue
    }
}

/// One advertised RSS feed is preferred to Atom; no recursive website crawl.
pub(super) fn feed_link(bytes: &[u8], base: &str) -> Option<String> {
    if bytes.len() > 2 * 1024 * 1024 {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let tokenizer = Tokenizer::new(Sink::default(), TokenizerOpts::default());
    let input = html5ever::buffer_queue::BufferQueue::default();
    input.push_back(StrTendril::from(text));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let mut scan = tokenizer.sink.0.into_inner();
    if !scan.html {
        return None;
    }
    scan.links.sort_by_key(|(atom, _)| *atom);
    let base = url::Url::parse(base).ok()?;
    scan.links
        .into_iter()
        .filter_map(|(_, href)| base.join(href.trim()).ok())
        .filter_map(|url| super::feed::normalize_url(url.as_str()).ok())
        .find(|url| url != base.as_str())
}
