//! Pin a finite closed-segment window, not a multi-GB growing recording.
//! Hard links share blocks with the snapshot until the rewind build drops.
use super::{MirroredDiskStore, invalid, live::read_playlist};
use crate::{routes::native_live_hls::valid_segment, storage::LiveRewindSource};
use std::{collections::HashSet, fmt::Write as _, io, time::Duration};
use tokio::io::AsyncWriteExt;

const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SEGMENTS: usize = 2000;
const CLIP_SECONDS: f64 = 300.0;

impl MirroredDiskStore {
    pub(super) async fn live_rewind(
        &self,
        media: i64,
        session: &str,
        slice: i32,
    ) -> io::Result<Option<LiveRewindSource>> {
        if !(0..=5).contains(&slice) {
            return Err(invalid("invalid replica rewind slice"));
        }
        let _guard = tokio::time::timeout(Duration::from_secs(5), self.writer.lock())
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?;
        let source = match self.checked_live_dir(&self.replica_root, media, session, false) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = tokio::fs::symlink_metadata(source.join("index.m3u8")).await?;
        if !meta
            .modified()?
            .elapsed()
            .is_ok_and(|age| age <= Duration::from_secs(30))
        {
            return Ok(None);
        }
        let segments = parse_window(&read_playlist(&source).await?)?;
        let selected = select_slice(&segments, slice);
        if selected.is_empty() {
            return Ok(None);
        }
        let mut inputs = Vec::with_capacity(selected.len());
        let mut bytes = 0_u64;
        for (name, duration) in selected {
            let path = Self::key_path(&source, &name)?;
            let meta = tokio::fs::symlink_metadata(&path).await?;
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() == 0 {
                return Err(invalid("invalid rewind segment"));
            }
            bytes = bytes
                .checked_add(meta.len())
                .ok_or_else(|| invalid("rewind input overflow"))?;
            if bytes > MAX_INPUT_BYTES {
                return Err(invalid("rewind input exceeds bounded snapshot"));
            }
            inputs.push((name, duration, path));
        }
        let directory = tempfile::Builder::new()
            .prefix(".plamenu-rewind-")
            .tempdir_in(&source)?;
        // No awaits between linking and guard ownership: cancellation cannot
        // leave a detached blocking link writer inside a deleted directory.
        for (name, _, path) in &inputs {
            let pinned = directory.path().join(name);
            std::fs::hard_link(path, &pinned)?;
            let meta = std::fs::symlink_metadata(&pinned)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(invalid("pinned rewind segment is not regular"));
            }
        }
        let maximum = inputs
            .iter()
            .map(|(_, duration, _)| *duration)
            .fold(1.0_f64, f64::max)
            .ceil();
        let mut manifest = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{maximum}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n"
        );
        for (name, duration, _) in inputs {
            writeln!(manifest, "#EXTINF:{duration},\n{name}").expect("String write");
        }
        manifest.push_str("#EXT-X-ENDLIST\n");
        let playlist = directory.path().join("index.m3u8");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&playlist).await?;
        file.write_all(manifest.as_bytes()).await?;
        drop(file);
        Ok(Some(LiveRewindSource {
            playlist,
            directory,
        }))
    }
}

fn parse_window(bytes: &[u8]) -> io::Result<Vec<(String, f64)>> {
    let source = std::str::from_utf8(bytes).map_err(|_| invalid("rewind playlist UTF-8"))?;
    if !source.starts_with("#EXTM3U\n") {
        return Err(invalid("rewind playlist header"));
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut duration = None;
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            if duration.is_some() {
                return Err(invalid("duplicate rewind duration"));
            }
            let value = value
                .split(',')
                .next()
                .unwrap_or("")
                .parse::<f64>()
                .map_err(|_| invalid("rewind duration"))?;
            if !value.is_finite() || !(0.01..=60.0).contains(&value) {
                return Err(invalid("rewind duration out of bounds"));
            }
            duration = Some(value);
        } else if !line.is_empty() && !line.starts_with('#') {
            if !valid_segment(line)
                || !seen.insert(line.to_owned())
                || entries.len() >= MAX_SEGMENTS
            {
                return Err(invalid("rewind segment names"));
            }
            entries.push((
                line.to_owned(),
                duration
                    .take()
                    .ok_or_else(|| invalid("missing rewind duration"))?,
            ));
        }
    }
    if duration.is_some() {
        return Err(invalid("unterminated rewind entry"));
    }
    Ok(entries)
}

fn select_slice(entries: &[(String, f64)], slice: i32) -> Vec<(String, f64)> {
    let total: f64 = entries.iter().map(|(_, duration)| *duration).sum();
    let end = total - f64::from(slice) * CLIP_SECONDS;
    if end <= 0.0 {
        return Vec::new();
    }
    let start = (end - CLIP_SECONDS).max(0.0);
    let mut at = 0.0;
    entries
        .iter()
        .filter_map(|(name, duration)| {
            let before = at;
            at += duration;
            (at > start && before < end).then(|| (name.clone(), *duration))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slice_selection_is_ordered_and_bounded_at_whole_segment_edges() {
        let entries: Vec<_> = (0..900)
            .map(|n| (format!("segment{n:06}.ts"), 2.0))
            .collect();
        assert_eq!(select_slice(&entries, 0), entries[750..]);
        assert_eq!(select_slice(&entries, 1), entries[600..750]);
        assert!(select_slice(&entries, 6).is_empty());
        assert_eq!(select_slice(&entries[..4], 0), entries[..4]);
    }
    #[test]
    fn window_rejects_paths_bad_duration_duplicates_and_missing_headers() {
        assert!(parse_window(b"#EXTM3U\n#EXTINF:2,\nsegment000001.ts\n").is_ok());
        for source in [
            "missing header",
            "#EXTM3U\n#EXTINF:NaN,\nsegment000001.ts\n",
            "#EXTM3U\n#EXTINF:2,\n../escape.ts\n",
            "#EXTM3U\nsegment000001.ts\n",
            "#EXTM3U\n#EXTINF:2,\n#EXTINF:3,\nsegment000001.ts\n",
            "#EXTM3U\n#EXTINF:2,\nsegment000001.ts\n#EXTINF:2,\nsegment000001.ts\n",
        ] {
            assert!(
                parse_window(source.as_bytes()).is_err(),
                "accepted {source}"
            );
        }
    }
    #[tokio::test]
    async fn pinned_snapshot_survives_live_rollover_and_drop_removes_only_owned_links() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let replica = temp.path().join("replica");
        std::fs::create_dir(&replica).unwrap();
        let store = MirroredDiskStore::new(primary, replica, 1024 * 1024, 0).unwrap();
        let session = "12345678-1234-1234-1234-123456789abc";
        let root = store
            .checked_live_dir(&store.replica_root, 42, session, true)
            .unwrap();
        let segment = root.join("segment000001.ts");
        std::fs::write(&segment, b"closed segment").unwrap();
        std::fs::write(
            root.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:2,\nsegment000001.ts\n",
        )
        .unwrap();
        let source = store.live_rewind(42, session, 0).await.unwrap().unwrap();
        let owned = source.directory().to_path_buf();
        std::fs::remove_file(&segment).unwrap();
        assert_eq!(
            std::fs::read(owned.join("segment000001.ts")).unwrap(),
            b"closed segment"
        );
        assert!(
            std::fs::read_to_string(&source.playlist)
                .unwrap()
                .ends_with("#EXT-X-ENDLIST\n")
        );
        drop(source);
        assert!(!owned.exists());
        assert!(root.join("index.m3u8").exists());
    }
}
