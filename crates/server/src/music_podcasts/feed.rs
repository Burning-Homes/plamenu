//! Bounded RSS/Atom and OPML parsing. No DTD or external entity resolution.
use crate::error::ApiError;
use roxmltree::{Document, Node, ParsingOptions};
use std::collections::HashSet;
use time::{
    OffsetDateTime,
    format_description::well_known::{Rfc2822, Rfc3339},
};
use url::Url;
const ITUNES: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";
const ATOM: &str = "http://www.w3.org/2005/Atom";
const CONTENT: &str = "http://purl.org/rss/1.0/modules/content/";
#[derive(Debug)]
pub struct Feed {
    pub title: String,
    pub description: String,
    pub author: String,
    pub link: Option<String>,
    pub image_url: Option<String>,
    pub language: String,
    pub categories: Vec<String>,
    pub explicit: bool,
    pub episodes: Vec<Episode>,
}
#[derive(Debug)]
pub struct Episode {
    pub guid: String,
    pub title: String,
    pub description: String,
    pub link: Option<String>,
    pub image_url: Option<String>,
    pub audio_url: String,
    pub audio_type: String,
    pub audio_length: Option<i64>,
    pub duration_seconds: Option<i32>,
    pub published_at: Option<OffsetDateTime>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub episode_type: Option<String>,
}
fn invalid() -> ApiError {
    ApiError::Unprocessable("Invalid or oversized podcast XML".into())
}
static LABEL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)\bencoding\s*=\s*['"]([a-z0-9._-]{1,40})['"]"#)
        .expect("static XML encoding expression")
});
fn decode_xml(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>, ApiError> {
    use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(invalid());
    }
    let bom = Encoding::for_bom(bytes);
    let inferred = if bytes.starts_with(b"<\0") {
        Some(UTF_16LE)
    } else if bytes.starts_with(b"\0<") {
        Some(UTF_16BE)
    } else {
        None
    };
    let declared = if bytes.starts_with(b"<?xml") {
        let end = bytes.windows(2).take(512).position(|v| v == b"?>");
        if let Some(end) = end {
            let declaration = std::str::from_utf8(&bytes[..end]).map_err(|_| invalid())?;
            LABEL
                .captures(declaration)
                .and_then(|c| c.get(1))
                .map(|v| {
                    Encoding::for_label_no_replacement(v.as_str().as_bytes()).ok_or_else(invalid)
                })
                .transpose()?
        } else {
            None
        }
    } else {
        None
    };
    let encoding = bom
        .map(|(e, _)| e)
        .or(inferred)
        .or(declared)
        .unwrap_or(UTF_8);
    let (text, errors) = encoding.decode_with_bom_removal(bytes);
    if errors || text.len() > 8 * 1024 * 1024 {
        return Err(invalid());
    }
    Ok(text)
}
fn document(text: &str) -> Result<Document<'_>, ApiError> {
    Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
            ..ParsingOptions::default()
        },
    )
    .map_err(|_| invalid())
}
pub fn normalize_url(raw: &str) -> Result<String, ApiError> {
    let mut url = Url::parse(raw.trim())
        .map_err(|_| ApiError::Unprocessable("Use an absolute HTTP or HTTPS feed URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || raw.len() > 2048
    {
        return Err(ApiError::Unprocessable(
            "Use an absolute HTTP or HTTPS feed URL without credentials".into(),
        ));
    }
    url.set_fragment(None);
    Ok(url.to_string())
}
fn resolved(base: &Url, raw: Option<&str>) -> Option<String> {
    raw.and_then(|v| base.join(v.trim()).ok())
        .and_then(|v| normalize_url(v.as_str()).ok())
}
fn child<'a, 'i>(node: Node<'a, 'i>, ns: Option<&str>, name: &str) -> Option<Node<'a, 'i>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == ns)
}
fn text(node: Node<'_, '_>, ns: Option<&str>, name: &str, max: usize) -> String {
    child(node, ns, name)
        .and_then(|n| n.text())
        .unwrap_or_default()
        .trim()
        .chars()
        .take(max)
        .collect()
}
fn description(node: Node<'_, '_>, atom: bool) -> String {
    let value = if atom {
        child(node, Some(ATOM), "content")
            .or_else(|| child(node, Some(ATOM), "summary"))
            .or_else(|| child(node, Some(ATOM), "subtitle"))
    } else {
        child(node, Some(CONTENT), "encoded").or_else(|| child(node, None, "description"))
    };
    let Some(value) = value else {
        return String::new();
    };
    let raw = if atom && value.attribute("type") == Some("xhtml") {
        value
            .children()
            .find(Node::is_element)
            .map(|n| &n.document().input_text()[n.range()])
            .unwrap_or_default()
    } else {
        value.text().unwrap_or_default()
    };
    let bounded = raw.chars().take(100_000).collect::<String>();
    if atom && value.attribute("type").is_none_or(|v| v == "text") {
        plamenu_ap::text::plain_to_html(&bounded)
    } else {
        plamenu_ap::text::sanitize_remote_html(&bounded)
    }
}
fn image(node: Node<'_, '_>, base: &Url) -> Option<String> {
    resolved(
        base,
        child(node, Some(ITUNES), "image").and_then(|n| n.attribute("href")),
    )
    .or_else(|| {
        resolved(
            base,
            child(node, None, "image")
                .and_then(|n| child(n, None, "url"))
                .and_then(|n| n.text()),
        )
    })
    .or_else(|| {
        resolved(
            base,
            child(node, Some(ATOM), "logo")
                .or_else(|| child(node, Some(ATOM), "icon"))
                .and_then(|n| n.text()),
        )
    })
}
fn link(node: Node<'_, '_>, base: &Url, atom: bool) -> Option<String> {
    if atom {
        resolved(
            base,
            node.children()
                .find(|n| {
                    n.has_tag_name((ATOM, "link"))
                        && n.attribute("rel").is_none_or(|v| v == "alternate")
                })
                .and_then(|n| n.attribute("href")),
        )
    } else {
        resolved(base, child(node, None, "link").and_then(|n| n.text()))
    }
}
fn duration(raw: &str) -> Option<i32> {
    let parts = raw.trim().split(':').collect::<Vec<_>>();
    if parts.len() > 3 {
        return None;
    }
    let mut total = 0_i32;
    for (i, p) in parts.iter().enumerate() {
        let value = p.parse::<i32>().ok()?;
        if value < 0 || (i > 0 && value >= 60) {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(value)?;
    }
    Some(total)
}
fn number(node: Node<'_, '_>, name: &str) -> Option<i32> {
    text(node, Some(ITUNES), name, 16)
        .parse::<i32>()
        .ok()
        .filter(|n| *n >= 0)
}
fn date(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .or_else(|| OffsetDateTime::parse(raw, &Rfc2822).ok())
        .or_else(|| {
            httpdate::parse_http_date(raw)
                .ok()
                .map(OffsetDateTime::from)
        })
}
fn parse_episode(node: Node<'_, '_>, base: &Url, atom: bool) -> Option<Episode> {
    let ns = atom.then_some(ATOM);
    let enclosure = if atom {
        node.children()
            .find(|n| n.has_tag_name((ATOM, "link")) && n.attribute("rel") == Some("enclosure"))
    } else {
        child(node, None, "enclosure")
    };
    let enclosure = enclosure?;
    let audio_url = resolved(base, enclosure.attribute(if atom { "href" } else { "url" }))?;
    let mut guid = text(node, ns, if atom { "id" } else { "guid" }, 2048);
    if guid.is_empty() {
        guid.clone_from(&audio_url);
    }
    let title = text(node, ns, "title", 512);
    let title = if title.is_empty() {
        "Untitled episode".into()
    } else {
        title
    };
    let published = if atom {
        let value = text(node, ns, "published", 100);
        if value.is_empty() {
            text(node, ns, "updated", 100)
        } else {
            value
        }
    } else {
        text(node, None, "pubDate", 100)
    };
    Some(Episode {
        guid,
        title,
        description: description(node, atom),
        link: link(node, base, atom),
        image_url: image(node, base),
        audio_url,
        audio_type: enclosure
            .attribute("type")
            .unwrap_or_default()
            .chars()
            .take(128)
            .collect(),
        audio_length: enclosure
            .attribute("length")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|n| *n >= 0),
        duration_seconds: duration(&text(node, Some(ITUNES), "duration", 32)),
        published_at: date(&published),
        season_number: number(node, "season"),
        episode_number: number(node, "episode"),
        episode_type: Some(text(node, Some(ITUNES), "episodeType", 32))
            .filter(|v| matches!(v.as_str(), "full" | "trailer" | "bonus")),
    })
}

pub fn parse(bytes: &[u8], base: &str) -> Result<Feed, ApiError> {
    let xml = decode_xml(bytes)?;
    let document = document(&xml)?;
    let root = document.root_element();
    let atom = root.has_tag_name((ATOM, "feed"));
    let channel = if atom {
        root
    } else if root.has_tag_name("rss") {
        child(root, None, "channel").ok_or_else(invalid)?
    } else {
        return Err(invalid());
    };
    let ns = atom.then_some(ATOM);
    let base = Url::parse(base).map_err(|_| invalid())?;
    let title = text(channel, ns, "title", 512);
    if title.is_empty() {
        return Err(ApiError::Unprocessable("Podcast feed has no title".into()));
    }
    let mut categories = channel
        .descendants()
        .filter(|n| {
            n.is_element()
                && n.tag_name().namespace() == Some(ITUNES)
                && n.tag_name().name() == "category"
        })
        .filter_map(|n| n.attribute("text"))
        .map(|v| v.chars().take(200).collect::<String>())
        .take(100)
        .collect::<Vec<_>>();
    categories.extend(
        channel
            .children()
            .filter(|n| {
                if atom {
                    n.has_tag_name((ATOM, "category"))
                } else {
                    n.has_tag_name("category")
                }
            })
            .filter_map(|n| if atom { n.attribute("term") } else { n.text() })
            .take(100)
            .map(|v| v.chars().take(200).collect::<String>()),
    );
    categories.sort();
    categories.dedup();
    categories.truncate(100);
    let author = if atom {
        child(channel, Some(ATOM), "author")
            .map(|n| text(n, Some(ATOM), "name", 512))
            .unwrap_or_default()
    } else {
        text(channel, Some(ITUNES), "author", 512)
    };
    let mut episodes = Vec::new();
    let mut guids = HashSet::new();
    for node in channel
        .children()
        .filter(|n| {
            if atom {
                n.has_tag_name((ATOM, "entry"))
            } else {
                n.has_tag_name("item")
            }
        })
        .take(2000)
    {
        if let Some(episode) = parse_episode(node, &base, atom)
            && guids.insert(episode.guid.clone())
        {
            episodes.push(episode);
        }
    }
    Ok(Feed {
        title,
        description: description(channel, atom),
        author,
        link: link(channel, &base, atom),
        image_url: image(channel, &base),
        language: text(channel, None, "language", 32),
        categories,
        explicit: matches!(
            text(channel, Some(ITUNES), "explicit", 16)
                .to_lowercase()
                .as_str(),
            "yes" | "true" | "1"
        ),
        episodes,
    })
}
pub fn parse_opml(bytes: &[u8]) -> Result<Vec<String>, ApiError> {
    let xml = decode_xml(bytes)?;
    let document = document(&xml)?;
    if !document.root_element().has_tag_name("opml") {
        return Err(invalid());
    }
    let mut urls = HashSet::new();
    let mut result = Vec::new();
    for node in document.descendants().filter(|n| n.has_tag_name("outline")) {
        if let Some(raw) = node.attribute("xmlUrl") {
            let url = normalize_url(raw)?;
            if urls.insert(url.clone()) {
                if result.len() >= 500 {
                    return Err(ApiError::Unprocessable(
                        "OPML import limit is 500 subscriptions".into(),
                    ));
                }
                result.push(url);
            }
        }
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rss_namespaces_relative_enclosures_duration_and_descriptions() {
        let xml=br#"<rss xmlns:i="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Show &amp; friends</title><description><![CDATA[<p>Notes</p><script>bad()</script>]]></description><i:author>Host</i:author><i:category text="Science"/><item><guid>stable</guid><title>Episode</title><enclosure url="../audio.mp3" type="audio/mpeg" length="32"/><i:duration>1:02:03</i:duration><i:season>2</i:season><pubDate>Fri, 09 Oct 2026 12:00:00 GMT</pubDate></item><item><guid>stable</guid><enclosure url="/duplicate.mp3"/></item><item><enclosure url="file:///secret"/></item></channel></rss>"#;
        let feed = parse(xml, "https://podcast.example/feeds/show.xml").unwrap();
        assert_eq!(feed.title, "Show & friends");
        assert_eq!(feed.author, "Host");
        assert_eq!(feed.categories, vec!["Science"]);
        assert!(!feed.description.contains("script"));
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(
            feed.episodes[0].audio_url,
            "https://podcast.example/audio.mp3"
        );
        assert_eq!(feed.episodes[0].duration_seconds, Some(3723));
        assert_eq!(feed.episodes[0].season_number, Some(2));
        assert!(feed.episodes[0].published_at.is_some());
        assert_eq!(
            date("Fri, 09 Oct 2026 15:00:00 +0300")
                .unwrap()
                .unix_timestamp(),
            date("2026-10-09T12:00:00Z").unwrap().unix_timestamp()
        );
    }
    #[test]
    fn atom_enclosures_and_nested_opml_are_bounded_and_do_not_resolve_entities() {
        let feed=parse(br#"<feed xmlns="http://www.w3.org/2005/Atom"><title>Show</title><entry><id>one</id><title>One</title><summary type="text">&lt;script&gt;plain&lt;/script&gt;</summary><link rel="enclosure" href="one.ogg" type="audio/ogg"/><published>2026-10-09T12:00:00Z</published></entry></feed>"#,"https://podcast.example/feed").unwrap();
        assert_eq!(
            feed.episodes[0].audio_url,
            "https://podcast.example/one.ogg"
        );
        assert!(feed.episodes[0].description.contains("&lt;script&gt;"));
        let opml=parse_opml(br#"<opml><body><outline text="Group"><outline xmlUrl="https://podcast.example/feed#one"/><outline xmlUrl="https://podcast.example/feed"/></outline></body></opml>"#).unwrap();
        assert_eq!(opml, vec!["https://podcast.example/feed"]);
        assert!(parse(br#"<!DOCTYPE rss [<!ENTITY x SYSTEM "file:///secret">]><rss><channel><title>&x;</title></channel></rss>"#,"https://podcast.example/feed").is_err());
        assert!(parse(b"<rss><channel>", "https://podcast.example/feed").is_err());
        assert!(
            parse_opml(
                br#"<opml><outline xmlUrl="https://user:secret@podcast.example/feed"/></opml>"#
            )
            .is_err()
        );
        assert_eq!(duration("1:99"), None);
    }
    #[test]
    fn xml_encodings_preserve_titles_without_replacement_or_external_entities() {
        let bytes=b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss><channel><title>Caf\xe9</title></channel></rss>";
        assert_eq!(
            parse(bytes, "https://podcast.example/feed").unwrap().title,
            "Café"
        );
        let mut utf16 = vec![0xff, 0xfe];
        for unit in "<?xml version=\"1.0\" encoding=\"UTF-16\"?><rss><channel><title>Музыка</title></channel></rss>".encode_utf16(){utf16.extend(unit.to_le_bytes());}
        assert_eq!(
            parse(&utf16, "https://podcast.example/feed").unwrap().title,
            "Музыка"
        );
        assert!(
            parse(
                b"<rss><channel><title>bad\xff</title></channel></rss>",
                "https://podcast.example/feed"
            )
            .is_err()
        );
    }
}
