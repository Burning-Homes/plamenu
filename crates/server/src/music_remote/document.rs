//! Bounded music documents retain canonical identities and controller claims.
use crate::error::ApiError;
use plamenu_ap::activity::{PUBLIC, attributed_to_ids, id_of};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};

pub const MAX_DOCUMENTS: usize = 2000;
const MAX_DEPTH: usize = 6;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Track,
    Artist,
    Album,
    Library,
    Playlist,
}
impl Kind {
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        match value["type"].as_str()? {
            "Audio" => Some(Self::Audio),
            "Track" => Some(Self::Track),
            "Artist" => Some(Self::Artist),
            "Album" => Some(Self::Album),
            "Library" => Some(Self::Library),
            "Playlist" => Some(Self::Playlist),
            _ => None,
        }
    }
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Track => "track",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Library => "library",
            Self::Playlist => "playlist",
        }
    }
}
#[derive(Clone)]
pub struct Document {
    pub uri: String,
    pub controller: String,
    pub proof_uri: String,
    pub proof_kind: Kind,
    pub kind: Kind,
    pub name: String,
    pub summary: String,
    pub media_of: Option<String>,
    pub parent: Option<String>,
    pub audio_url: Option<String>,
    pub details: super::details::Details,
    pub catalogue: Option<(String, String)>,
}
fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
pub fn url(raw: &str) -> Result<url::Url, ApiError> {
    if raw.len() > 2048
        || raw.chars().any(char::is_control)
        || !plamenu_federation::is_federation_url(raw)
    {
        return Err(invalid("Invalid remote music URL"));
    }
    let value = url::Url::parse(raw).map_err(|_| invalid("Invalid remote music URL"))?;
    if !value.username().is_empty() || value.password().is_some() || value.fragment().is_some() {
        return Err(invalid("Invalid remote music URL"));
    }
    Ok(value)
}
fn same_origin(left: &str, right: &str) -> bool {
    match (url(left), url(right)) {
        (Ok(left), Ok(right)) => left.origin() == right.origin(),
        _ => false,
    }
}
fn addresses(value: &Value) -> impl Iterator<Item = &str> {
    ["to", "cc", "audience"].into_iter().flat_map(move |key| {
        value
            .get(key)
            .into_iter()
            .flat_map(|value| {
                value
                    .as_array()
                    .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
            })
            .filter_map(id_of)
    })
}
/// Music catalogue metadata is anonymously served without audiences by the
/// pinned reference. Addressed documents must explicitly include Public.
pub(super) fn public(value: &Value, kind: Kind, inherited_public: bool) -> bool {
    let mut audience = addresses(value).peekable();
    if audience.peek().is_some() {
        return audience.any(|id| id == PUBLIC);
    }
    !matches!(kind, Kind::Audio) || inherited_public
}
fn text(raw: Option<&str>, max: usize) -> String {
    raw.unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(max)
        .collect()
}
fn media(value: &Value) -> Option<(String, Option<String>)> {
    let attachments = value.get("attachment").into_iter().flat_map(|value| {
        value
            .as_array()
            .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
    });
    for attachment in attachments {
        if attachment["mediaType"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("audio/"))
            || attachment["type"].as_str() == Some("Audio")
        {
            let link = id_of(&attachment["url"])?;
            if url(link).is_ok() {
                return Some((link.into(), audio_type(&attachment["mediaType"])));
            }
        }
    }
    let values = value.get("url").into_iter().flat_map(|value| {
        value
            .as_array()
            .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
    });
    for link in values {
        if link["mediaType"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("audio/"))
        {
            let uri = link["href"].as_str().or_else(|| id_of(link))?;
            if url(uri).is_ok() {
                return Some((uri.into(), audio_type(&link["mediaType"])));
            }
        }
    }
    None
}
fn audio_type(value: &Value) -> Option<String> {
    let mime = value
        .as_str()?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    (mime.starts_with("audio/")
        && mime.len() <= 64
        && mime
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/.-+".contains(&byte)))
    .then_some(mime)
}

fn parent_uri<'a>(value: &'a Value, kind: Kind, container: Option<&'a str>) -> Option<&'a str> {
    ["library", "album"]
        .into_iter()
        .find_map(|key| id_of(&value[key]))
        .or_else(|| {
            (kind == Kind::Album)
                .then(|| {
                    value["artists"]
                        .as_array()
                        .and_then(|artists| artists.first())
                        .and_then(id_of)
                })
                .flatten()
        })
        .or(container)
}
fn root_controller<'a>(root: &'a Value, expected: &str) -> Result<&'a str, ApiError> {
    url(expected)?;
    if root["id"].as_str() != Some(expected) {
        return Err(invalid("Remote music identity differs from its URL"));
    }
    let controller = root
        .get("actor")
        .and_then(id_of)
        .or_else(|| attributed_to_ids(root).next())
        .ok_or_else(|| invalid("Remote music has no controller"))?;
    if !same_origin(expected, controller) {
        return Err(invalid("Remote music controller is on a different origin"));
    }
    if Kind::parse(root).is_none() {
        return Err(invalid("Not a music resource"));
    }
    Ok(controller)
}
/// Parse embedded catalogues without network I/O. A child cannot gain a
/// controller or public audience through an unrelated parent. A different
/// explicit controller is retained for independent actor verification.
pub fn parse(root: &Value, expected: &str) -> Result<Vec<Document>, ApiError> {
    let controller = root_controller(root, expected)?;
    let root_kind = Kind::parse(root).ok_or_else(|| invalid("Not a music resource"))?;
    let mut queue = VecDeque::from([(root, controller, None, 0usize, false, expected, root_kind)]);
    let mut seen = HashSet::new();
    let mut result: Vec<Document> = Vec::new();
    let mut track_proofs = HashSet::new();
    while let Some((
        value,
        inherited_controller,
        container,
        depth,
        inherited_public,
        inherited_proof,
        inherited_proof_kind,
    )) = queue.pop_front()
    {
        if depth > MAX_DEPTH || result.len() >= MAX_DOCUMENTS {
            return Err(invalid("Remote music document graph is too large"));
        }
        let Some(kind) = Kind::parse(value) else {
            continue;
        };
        let Some(uri) = value["id"].as_str() else {
            continue;
        };
        let duplicate = !seen.insert(uri);
        if duplicate
            && (kind != Kind::Track
                || !result
                    .iter()
                    .any(|doc| doc.uri == uri && doc.kind == Kind::Track))
        {
            continue;
        }
        let controller = value
            .get("actor")
            .and_then(id_of)
            .or_else(|| attributed_to_ids(value).next())
            .unwrap_or(inherited_controller);
        if !same_origin(uri, controller) || !same_origin(uri, expected) {
            return Err(invalid("Remote music child has an invalid origin"));
        }
        if !public(
            value,
            kind,
            inherited_public && controller == inherited_controller,
        ) {
            return Err(invalid("Remote music is not public"));
        }
        if kind == Kind::Track && !track_proofs.insert((uri, inherited_proof)) {
            continue;
        }
        let child_proof = if kind == Kind::Audio {
            uri
        } else {
            inherited_proof
        };
        result.push(node(
            value,
            kind,
            uri,
            controller,
            (inherited_proof, inherited_proof_kind),
            container,
            expected,
        ));
        for key in ["track", "album", "artists", "items", "orderedItems"] {
            if value
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() > 500)
            {
                return Err(invalid("Remote music node is too large"));
            }
            for child in value.get(key).into_iter().flat_map(|value| {
                value
                    .as_array()
                    .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
            }) {
                if child.is_object() {
                    queue.push_back((
                        child,
                        controller,
                        matches!(key, "items" | "orderedItems").then_some(uri),
                        depth + 1,
                        true,
                        child_proof,
                        if kind == Kind::Audio {
                            Kind::Audio
                        } else {
                            inherited_proof_kind
                        },
                    ));
                }
            }
        }
    }
    inherit_audio_details(&mut result);
    Ok(result)
}

fn node(
    value: &Value,
    kind: Kind,
    uri: &str,
    controller: &str,
    proof: (&str, Kind),
    container: Option<&str>,
    expected: &str,
) -> Document {
    let (inherited_proof, inherited_proof_kind) = proof;
    let parent = parent_uri(value, kind, container);
    let media_of = id_of(&value["track"]);
    let selected = media(value);
    let mut details = super::details::parse(value);
    details.audio_type = selected.as_ref().and_then(|(_, mime)| mime.clone());
    Document {
        catalogue: None,
        uri: uri.into(),
        controller: controller.into(),
        proof_uri: if kind == Kind::Audio && addresses(value).any(|uri| uri == PUBLIC) {
            uri.into()
        } else {
            inherited_proof.into()
        },
        proof_kind: if kind == Kind::Audio && addresses(value).any(|uri| uri == PUBLIC) {
            Kind::Audio
        } else {
            inherited_proof_kind
        },
        kind,
        name: text(value["name"].as_str(), 500),
        summary: plamenu_ap::text::sanitize_remote_html(&text(
            value["summary"]
                .as_str()
                .or_else(|| value["content"].as_str()),
            16000,
        )),
        media_of: media_of
            .filter(|id| same_origin(id, expected))
            .map(str::to_owned),
        parent: parent
            .filter(|id| same_origin(id, expected))
            .map(str::to_owned),
        audio_url: selected.map(|(url, _)| url),
        details,
    }
}

fn inherit_audio_details(result: &mut [Document]) {
    let audio_details: std::collections::HashMap<_, _> = result
        .iter()
        .filter(|item| item.kind == Kind::Audio)
        .map(|item| (item.uri.clone(), item.details.clone()))
        .collect();
    for item in result {
        if item.kind == Kind::Track
            && let Some(audio) = audio_details.get(&item.proof_uri)
        {
            item.details.duration = item.details.duration.or(audio.duration);
            item.details.audio_type.clone_from(&audio.audio_type);
            for tag in &audio.tags {
                if item.details.tags.len() < 32 && !item.details.tags.contains(tag) {
                    item.details.tags.push(tag.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ROOT: &str = "https://music.example/library";
    const OWNER: &str = "https://music.example/actors/alice";
    #[test]
    fn selected_audio_type_matches_exact_valid_enclosure_and_is_bounded() {
        let value = json!({"attachment":[{"type":"Audio","mediaType":"audio/ogg; codecs=vorbis","url":"https://music.example/one.ogg"}],"url":[{"type":"Link","mediaType":"audio/mpeg","href":"https://music.example/two.mp3"}]});
        assert_eq!(
            media(&value),
            Some((
                "https://music.example/one.ogg".into(),
                Some("audio/ogg".into())
            ))
        );
        assert!(audio_type(&json!("audio/mpeg\r\nx-secret: yes")).is_none());
        assert!(audio_type(&json!(format!("audio/{}", "x".repeat(100)))).is_none());
        assert!(audio_type(&json!("text/html")).is_none());
        let unknown = json!({"attachment":{"type":"Audio","url":"https://music.example/raw"}});
        assert_eq!(
            media(&unknown),
            Some(("https://music.example/raw".into(), None))
        );
    }
    #[test]
    fn canonical_children_keep_their_controller_and_media_identity() {
        let root = json!({"id":ROOT,"type":"Library","actor":OWNER,"to":[PUBLIC],"items":[{
            "id":"https://music.example/upload/1","type":"Audio","attributedTo":OWNER,"to":[PUBLIC],
            "name":"A recording","attachment":[{"type":"Audio","mediaType":"audio/mpeg","url":"https://cdn.example/audio.mp3"}],
            "track":{"id":"https://music.example/track/1","type":"Track","name":"Canonical track",
                "album":{"id":"https://music.example/album/1","type":"Album","attributedTo":"https://music.example/actors/catalogue"}}
        }]});
        let docs = parse(&root, ROOT).unwrap();
        assert_eq!(docs.len(), 4);
        assert_eq!(docs[1].parent.as_deref(), Some(ROOT));
        assert_eq!(
            docs[1].media_of.as_deref(),
            Some("https://music.example/track/1")
        );
        assert_eq!(
            docs[1].audio_url.as_deref(),
            Some("https://cdn.example/audio.mp3")
        );
        assert_eq!(
            docs[2].parent.as_deref(),
            Some("https://music.example/album/1")
        );
        assert_eq!(docs[3].controller, "https://music.example/actors/catalogue");
        assert_eq!(docs[3].parent, None);
    }
    #[test]
    fn identity_controller_private_audience_and_embedded_graphs_are_bounded() {
        let mut root = json!({"id":ROOT,"type":"Library","actor":OWNER,"to":[PUBLIC]});
        assert!(parse(&root, "https://music.example/other").is_err());
        root["actor"] = json!("https://imposter.example/actors/alice");
        assert!(parse(&root, ROOT).is_err());
        root["actor"] = json!(OWNER);
        root["items"] = json!([{"id":"https://music.example/private","type":"Audio","to":[OWNER]}]);
        assert!(parse(&root, ROOT).is_err());
        root["items"] = json!([{"id":"https://imposter.example/child","type":"Track"}]);
        assert!(parse(&root, ROOT).is_err());
        root["items"] = json!(
            (0..MAX_DOCUMENTS)
                .map(|i| json!({"id":format!("https://music.example/tracks/{i}"),"type":"Track"}))
                .collect::<Vec<_>>()
        );
        assert!(parse(&root, ROOT).is_err());
        root["items"] = json!(
            (0..500)
                .map(|_| json!({"id":ROOT,"type":"Library","actor":OWNER}))
                .collect::<Vec<_>>()
        );
        assert_eq!(parse(&root, ROOT).unwrap().len(), 1);
        for url in [
            "https://user:pass@music.example/track",
            "https://music.example/track#other",
            "file:///etc/passwd",
        ] {
            assert!(super::url(url).is_err());
        }
    }
    #[test]
    fn graph_depth_and_total_node_limits_apply_across_individually_bounded_children() {
        let mut deep = json!({"id":"https://music.example/nested/7","type":"Library"});
        for index in (0..7).rev() {
            deep = json!({"id":format!("https://music.example/nested/{index}"),"type":"Library","items":[deep]});
        }
        let root = json!({"id":ROOT,"type":"Library","actor":OWNER,"items":[deep]});
        assert!(parse(&root, ROOT).is_err());
        let groups=(0..5).map(|group|json!({"id":format!("https://music.example/group/{group}"),"type":"Library","items":(0..400).map(|track|json!({"id":format!("https://music.example/track/{group}/{track}"),"type":"Track"})).collect::<Vec<_>>() })).collect::<Vec<_>>();
        let root = json!({"id":ROOT,"type":"Library","actor":OWNER,"items":groups});
        assert!(parse(&root, ROOT).is_err());
    }
}
