//! Typed display metadata never retains an arbitrary remote payload.
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Default, Deserialize, Serialize, sqlx::Type)]
#[serde(default)]
#[sqlx(type_name = "music_remote_details")]
pub struct Details {
    pub artist: String,
    pub artist_uri: Option<String>,
    pub album: String,
    pub genre: String,
    pub release_year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub musicbrainz_id: Option<String>,
    pub tags: Vec<String>,
    pub duration: Option<f64>,
    pub image_url: Option<String>,
    pub audio_type: Option<String>,
}
impl Details {
    #[must_use]
    pub fn public_value(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("Finite bounded music metadata");
        value
            .as_object_mut()
            .expect("Typed music metadata object")
            .remove("image_url");
        value
    }
}
pub fn serialize_public<S: serde::Serializer>(
    value: &sqlx::types::Json<Details>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value.public_value().serialize(serializer)
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}
fn artist_uri(track: &Value, root: &Value) -> Option<String> {
    let artist = track["artists"].as_array()?.first()?;
    let uri = plamenu_ap::activity::id_of(artist)?;
    let origin = super::document::url(root["id"].as_str()?).ok()?;
    let url = super::document::url(uri).ok()?;
    (url.origin() == origin.origin()).then(|| uri.to_owned())
}

fn ordinal(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| (1..=9999).contains(n))
}
#[must_use]
pub fn parse(value: &Value) -> Details {
    let track = if value["type"] == "Audio" && value["track"].is_object() {
        &value["track"]
    } else {
        value
    };
    let artist = track["artists"]
        .as_array()
        .and_then(|items| items.first())
        .map_or_else(String::new, |artist| text(&artist["name"]));
    let album = if value["type"] == "Album" {
        String::new()
    } else {
        text(&track["album"]["name"])
    };
    let released = if value["type"] == "Album" {
        &value["released"]
    } else {
        &track["album"]["released"]
    };
    let release_year = released
        .as_str()
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse::<i32>().ok())
        .filter(|year| (1..=9999).contains(year));
    let musicbrainz_id = track["musicbrainzId"]
        .as_str()
        .filter(|id| crate::audio::valid_mbid(id))
        .map(str::to_owned);
    let mut tags = Vec::new();
    if let Some(raw) = value["tag"].as_array() {
        for raw in raw.iter().take(64) {
            if raw["type"] == "Hashtag" {
                let name = text(&raw["name"])
                    .trim()
                    .trim_start_matches('#')
                    .to_lowercase();
                if !name.is_empty() && !tags.contains(&name) {
                    tags.push(name);
                }
            }
            if tags.len() == 32 {
                break;
            }
        }
    }
    let duration = value["duration"]
        .as_f64()
        .or_else(|| {
            value["duration"]
                .as_str()
                .and_then(crate::ingest::parse_iso8601_seconds)
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0 && *duration <= 604_800.0);
    let image = value
        .get("image")
        .or_else(|| value.get("icon"))
        .and_then(|image| image.as_str().or_else(|| image["url"].as_str()));
    let image_url = image
        .filter(|url| super::document::url(url).is_ok())
        .map(str::to_owned);
    Details {
        artist,
        artist_uri: artist_uri(track, value),
        album,
        genre: text(&value["genre"]),
        release_year,
        track_number: ordinal(&track["position"]),
        disc_number: ordinal(&track["disc"]),
        musicbrainz_id,
        tags,
        duration,
        image_url,
        audio_type: None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn structured_fields_are_finite_bounded_and_do_not_store_unknown_properties() {
        let value = json!({"type":"Audio","duration":9,"tag":[{"type":"Hashtag","name":"#Ambient"},{"type":"Hashtag","name":"#ambient"}],"track":{"position":2,"disc":1,"musicbrainzId":"6db39f0b-dc09-4dad-a77a-4bc5a6f27977","artists":[{"name":"Musician"}],"album":{"name":"Album","released":"2024-01-01"}},"image":{"url":"file:///etc/passwd"},"secret":"discard"});
        let detail = parse(&value);
        assert_eq!(detail.artist, "Musician");
        assert_eq!(detail.album, "Album");
        assert_eq!(detail.release_year, Some(2024));
        assert_eq!(detail.track_number, Some(2));
        assert_eq!(detail.disc_number, Some(1));
        assert_eq!(detail.tags, vec!["ambient"]);
        assert_eq!(detail.duration, Some(9.0));
        assert!(detail.musicbrainz_id.is_some());
        assert!(detail.image_url.is_none());
        assert!(!serde_json::to_string(&detail).unwrap().contains("secret"));
    }
    #[test]
    fn canonical_artist_credit_is_bounded_to_the_music_origin() {
        let mut value = serde_json::json!({"id":"https://music.example/audio/one","type":"Audio","track":{"artists":[{"id":"https://music.example/artists/one","name":"Artist"}]}});
        assert_eq!(
            parse(&value).artist_uri.as_deref(),
            Some("https://music.example/artists/one")
        );
        for uri in [
            "https://other.example/artists/one",
            "file:///etc/passwd",
            "/artists/one",
            "https://music.example/artists/one#fragment",
        ] {
            value["track"]["artists"][0]["id"] = serde_json::json!(uri);
            assert!(parse(&value).artist_uri.is_none());
        }
    }
}
