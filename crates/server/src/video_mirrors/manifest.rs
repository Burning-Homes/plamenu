//! Bounded, complete VOD HLS dependencies. Original media basenames are kept
//! for `PeerTube` redundancy URL substitution and segment checksum identity.
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use url::Url;

pub const MAX_PLAYLIST_BYTES: usize = 128 * 1024;
pub const MAX_REFERENCES: usize = 4096;
static URI_ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[,:]\s*URI="([^"]+)""#).unwrap());

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub url: String,
    pub playlist: bool,
}

fn resolve(base: &Url, uri: &str) -> Result<String, &'static str> {
    if uri.len() > 2048 {
        return Err("HLS reference exceeds URI limit");
    }
    let url = base.join(uri).map_err(|_| "Invalid HLS reference")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 2048
    {
        return Err("Mirror references require credential-free HTTPS URLs");
    }
    Ok(url.into())
}

fn attributes_are_playlists(line: &str) -> Result<bool, &'static str> {
    if line.starts_with("#EXT-X-MEDIA:") || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:") {
        return Ok(true);
    }
    if line.starts_with("#EXT-X-MAP:") {
        return Ok(false);
    }
    Err("Unsupported URI-bearing HLS tag; mirror was not published")
}

pub fn references(base: &Url, text: &str) -> Result<Vec<Reference>, &'static str> {
    if text.len() > MAX_PLAYLIST_BYTES || !text.starts_with("#EXTM3U") {
        return Err("Invalid or oversized HLS playlist");
    }
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    let mut next_playlist = false;
    let mut media = false;
    let mut ended = false;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with("#EXT-X-KEY:") && line != "#EXT-X-KEY:METHOD=NONE" {
            return Err("Encrypted HLS is not copied by the VOD mirror worker");
        }
        if line.starts_with("#EXT-X-SESSION-KEY:")
            || line.starts_with("#EXT-X-PART:")
            || line.starts_with("#EXT-X-PRELOAD-HINT:")
        {
            return Err("Only complete VOD playlists can be mirrored");
        }
        if line.starts_with("#EXT-X-STREAM-INF:") {
            next_playlist = true;
        }
        if line.starts_with("#EXTINF:") {
            media = true;
        }
        if line == "#EXT-X-ENDLIST" {
            ended = true;
        }
        let entries = if line.starts_with('#') {
            let mut entries = Vec::new();
            for capture in URI_ATTRIBUTE.captures_iter(line) {
                entries.push((resolve(base, &capture[1])?, attributes_are_playlists(line)?));
            }
            entries
        } else {
            let playlist = next_playlist;
            next_playlist = false;
            vec![(resolve(base, line)?, playlist)]
        };
        for (url, playlist) in entries {
            if seen.insert((url.clone(), playlist)) {
                output.push(Reference { url, playlist });
            }
            if output.len() > MAX_REFERENCES {
                return Err("HLS dependency count exceeds mirror limit");
            }
        }
    }
    if media && !ended {
        return Err("Unfinished/live HLS cannot become a complete mirror");
    }
    if next_playlist || output.is_empty() {
        return Err("Incomplete HLS dependency list");
    }
    Ok(output)
}

pub fn rewrite<S: std::hash::BuildHasher>(
    base: &Url,
    text: &str,
    names: &HashMap<String, String, S>,
) -> Result<String, &'static str> {
    references(base, text)?;
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            let mut cursor = 0;
            for capture in URI_ATTRIBUTE.captures_iter(line) {
                let value = capture.get(1).unwrap();
                let url = resolve(base, value.as_str())?;
                let name = names.get(&url).ok_or("Unacknowledged mirror dependency")?;
                if !valid_name(name) {
                    return Err("Unsafe public mirror dependency name");
                }
                output.push_str(&line[cursor..value.start()]);
                output.push_str(name);
                cursor = value.end();
            }
            output.push_str(&line[cursor..]);
        } else {
            let url = resolve(base, trimmed)?;
            let name = names.get(&url).ok_or("Unacknowledged mirror dependency")?;
            if !valid_name(name) {
                return Err("Unsafe public mirror dependency name");
            }
            output.push_str(name);
        }
        output.push('\n');
    }
    Ok(output)
}

#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_peertube_ranges_keep_names_ranges_and_alternate_audio() {
        let base = Url::parse("https://source.example/hls/master.m3u8").unwrap();
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a,b\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"a,b\"\n240.m3u8\n";
        let refs = references(&base, master).unwrap();
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().all(|item| item.playlist));
        let media = "#EXTM3U\n#EXT-X-MAP:URI=\"film-fragmented.mp4\",BYTERANGE=\"100@0\"\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:200@100\nfilm-fragmented.mp4\n#EXT-X-ENDLIST\n";
        let refs = references(&base, media).unwrap();
        assert_eq!(refs.len(), 1);
        assert!(!refs[0].playlist);
        let names = HashMap::from([(refs[0].url.clone(), "film-fragmented.mp4".into())]);
        assert_eq!(rewrite(&base, media, &names).unwrap(), media);
        assert!(rewrite(&base, media, &HashMap::new()).is_err());
    }
    #[test]
    fn unsafe_unfinished_encrypted_and_excessive_dependencies_never_publish() {
        let base = Url::parse("https://source.example/master.m3u8").unwrap();
        for text in [
            "#EXTM3U\n#EXTINF:2,\nfile.ts\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"https://user:secret@source.example/file.mp4\"\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"file:///etc/passwd\"\n",
        ] {
            assert!(references(&base, text).is_err());
        }
        for name in [
            "../file.mp4",
            ".hidden",
            "file/part.mp4",
            "file\".mp4",
            "file?query",
        ] {
            assert!(!valid_name(name));
        }
        let mut excessive = "#EXTM3U\n".to_owned();
        for index in 0..=MAX_REFERENCES {
            use std::fmt::Write as _;
            writeln!(excessive, "{index}.mp4").unwrap();
        }
        assert!(references(&base, &excessive).is_err());
    }
}
