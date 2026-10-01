//! Federation-compatible HLS redundancy offers (`PeerTube` `CacheFile`).
//! An offer is not permission to fetch, retain or serve its referenced Video.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CacheFile {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub object: String,
    pub expires: Option<String>,
    pub url: PlaylistLink,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaylistLink {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "mediaType", alias = "mimeType")]
    pub media_type: String,
    pub href: String,
}

impl CacheFile {
    pub fn new(
        id: String,
        video: String,
        playlist: String,
        expires: OffsetDateTime,
    ) -> Result<Self, time::error::Format> {
        Ok(Self {
            id,
            kind: "CacheFile".into(),
            object: video,
            expires: Some(expires.format(&Rfc3339)?),
            url: PlaylistLink {
                kind: "Link".into(),
                media_type: "application/x-mpegURL".into(),
                href: playlist,
            },
        })
    }

    /// Finite local acceptance, even for peers advertising no expiry.
    /// Signature/controller/URL/publication/consent checks belong to the
    /// authenticated server handler, not this vocabulary representation.
    pub fn bounded_expiry(&self, now: OffsetDateTime) -> Result<OffsetDateTime, &'static str> {
        if self.kind != "CacheFile"
            || self.url.kind != "Link"
            || !self
                .url
                .media_type
                .eq_ignore_ascii_case("application/x-mpegURL")
            || [&self.id, &self.object, &self.url.href]
                .iter()
                .any(|uri| uri.is_empty() || uri.len() > 2048)
        {
            return Err("Invalid HLS CacheFile offer");
        }
        let advertised = self.expires.as_deref().map_or_else(
            || Ok(now + Duration::days(1)),
            |value| OffsetDateTime::parse(value, &Rfc3339).map_err(|_| "Invalid CacheFile expiry"),
        )?;
        if advertised <= now {
            return Err("CacheFile offer has expired");
        }
        Ok(advertised.min(now + Duration::days(7)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actual_peertube_shape_roundtrips_and_missing_expiry_is_finite() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let wire = json!({
            "id":"https://mirror.example/redundancy/streaming-playlists/hls/video",
            "type":"CacheFile", "object":"https://origin.example/videos/watch/video",
            "expires":null,
            "url":{"type":"Link","mediaType":"application/x-mpegURL",
                   "href":"https://mirror.example/static/redundancy/hls/video"}
        });
        let offer: CacheFile = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&offer).unwrap(), wire);
        assert_eq!(offer.bounded_expiry(now), Ok(now + Duration::days(1)));
    }

    #[test]
    fn expired_invalid_unbounded_and_non_hls_offers_do_not_grant_endless_retention() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut offer = CacheFile::new(
            "https://mirror.example/cache/1".into(),
            "https://origin.example/videos/1".into(),
            "https://mirror.example/cache/1/master.m3u8".into(),
            now + Duration::days(365),
        )
        .unwrap();
        assert_eq!(offer.bounded_expiry(now), Ok(now + Duration::days(7)));
        offer.expires = Some(now.format(&Rfc3339).unwrap());
        assert!(offer.bounded_expiry(now).is_err());
        offer.expires = Some("not a timestamp".into());
        assert!(offer.bounded_expiry(now).is_err());
        offer.expires = None;
        offer.url.media_type = "video/mp4".into();
        assert!(offer.bounded_expiry(now).is_err());
    }
}
