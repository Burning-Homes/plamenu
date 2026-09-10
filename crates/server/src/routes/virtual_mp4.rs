//! Minimal fragmented-MP4 composition for `PeerTube`'s split-audio HLS mode.
//!
//! Media payloads are never remuxed: a small combined initialization section
//! is synthesized, then the original video/audio HLS fragments are exposed in
//! time order. Audio track identifiers are patched in box headers so the two
//! independently generated files form one valid two-track movie.

#[derive(Debug, Clone, Copy)]
pub struct BytePart {
    pub start: u64,
    pub len: u64,
}

#[derive(Debug, Clone)]
pub struct MediaPlaylist {
    pub init: BytePart,
    pub segments: Vec<MediaSegment>,
}

#[derive(Debug, Clone, Copy)]
pub struct MediaSegment {
    pub bytes: BytePart,
    pub starts_at: f64,
    pub duration: f64,
}

impl MediaPlaylist {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|segment| segment.duration).sum()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SegmentReference {
    pub size: u64,
    pub duration: f64,
}

#[derive(Debug, Clone)]
pub struct SegmentGroup {
    pub video: usize,
    pub audio: std::ops::Range<usize>,
    pub duration: f64,
}

#[derive(Debug, Clone)]
pub struct RelativePatch {
    pub offset: u64,
    pub expected: [u8; 4],
    pub replacement: [u8; 4],
}

/// Space reserved before every virtual media fragment for one two-track
/// `moof`. `PeerTube`'s four-second `FFmpeg` fragments use roughly 2--4 KiB; the
/// fixed bound keeps every global-index offset knowable before any fragment
/// header is fetched.
pub const FRAGMENT_HEADER_BYTES: u64 = 16 * 1024;

fn part_end(part: BytePart) -> Result<u64, String> {
    part.start
        .checked_add(part.len)
        .ok_or_else(|| "HLS byte range overflow".to_owned())
}

pub fn parse_media_playlist(body: &str) -> Result<MediaPlaylist, String> {
    let mut init = None;
    let mut segments = Vec::new();
    let mut next_offset = 0_u64;
    let mut pending = None;
    let mut pending_duration = None;
    let mut clock = 0.0_f64;
    for raw in body.lines() {
        let line = raw.trim();
        if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let range =
                attr(attrs, "BYTERANGE").ok_or_else(|| "HLS init has no BYTERANGE".to_owned())?;
            let part = parse_byterange(&range, 0)?;
            next_offset = part_end(part)?;
            init = Some(part);
        } else if let Some(range) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let part = parse_byterange(range, next_offset)?;
            next_offset = part_end(part)?;
            pending = Some(part);
        } else if let Some(duration) = line.strip_prefix("#EXTINF:") {
            pending_duration = duration
                .split(',')
                .next()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0);
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some(part) = pending.take()
        {
            let duration = pending_duration
                .take()
                .ok_or_else(|| "HLS media range has no EXTINF".to_owned())?;
            segments.push(MediaSegment {
                bytes: part,
                starts_at: clock,
                duration,
            });
            clock += duration;
        }
    }
    let init = init.ok_or_else(|| "HLS playlist has no init range".to_owned())?;
    if segments.is_empty() {
        return Err("HLS playlist has no media ranges".to_owned());
    }
    Ok(MediaPlaylist { init, segments })
}

/// Groups overlapping video/audio playlist fragments into contiguous MP4
/// subsegments. Video boundaries define seek granularity; this also handles an
/// audio playlist split at different boundaries without double-counting the
/// overlapping track durations in the global `sidx` timeline.
pub fn segment_groups(video: &MediaPlaylist, audio: &MediaPlaylist) -> Vec<SegmentGroup> {
    let mut groups = Vec::with_capacity(video.segments.len());
    let mut audio_start = 0;
    let equal_counts = video.segments.len() == audio.segments.len();
    let presentation_end = video
        .segments
        .last()
        .map_or(0.0, |segment| segment.starts_at + segment.duration)
        .max(
            audio
                .segments
                .last()
                .map_or(0.0, |segment| segment.starts_at + segment.duration),
        );
    for (index, segment) in video.segments.iter().enumerate() {
        let audio_end = if equal_counts {
            index + 1
        } else if let Some(next) = video.segments.get(index + 1) {
            audio
                .segments
                .iter()
                .enumerate()
                .skip(audio_start)
                .take_while(|(_, audio)| audio.starts_at < next.starts_at)
                .last()
                .map_or(audio_start, |(audio_index, _)| audio_index + 1)
        } else {
            audio.segments.len()
        };
        let duration = video
            .segments
            .get(index + 1)
            .map_or(presentation_end, |next| next.starts_at)
            - segment.starts_at;
        groups.push(SegmentGroup {
            video: index,
            audio: audio_start..audio_end,
            duration,
        });
        audio_start = audio_end;
    }
    groups
}

fn parse_byterange(raw: &str, default_start: u64) -> Result<BytePart, String> {
    let raw = raw.trim().trim_matches('"');
    let (len, start) = match raw.split_once('@') {
        Some((len, start)) => (len, start.parse().map_err(|_| "bad HLS range offset")?),
        None => (raw, default_start),
    };
    let len = len.parse().map_err(|_| "bad HLS range length")?;
    if len == 0 {
        return Err("empty HLS range".to_owned());
    }
    Ok(BytePart { start, len })
}

fn attr(attrs: &str, wanted: &str) -> Option<String> {
    let mut quoted = false;
    let mut start = 0;
    for (index, ch) in attrs
        .char_indices()
        .chain(std::iter::once((attrs.len(), ',')))
    {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                let part = &attrs[start..index];
                if let Some((name, value)) = part.split_once('=')
                    && name.trim() == wanted
                {
                    return Some(value.trim().trim_matches('"').to_owned());
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct Mp4Box {
    start: usize,
    size: usize,
    header: usize,
    kind: [u8; 4],
}

impl Mp4Box {
    fn content_start(self) -> usize {
        self.start + self.header
    }

    fn end(self) -> usize {
        // Every `Mp4Box` is created only after `boxes` validates this sum.
        self.start.checked_add(self.size).unwrap()
    }
}

fn boxes(bytes: &[u8], start: usize, end: usize) -> Result<Vec<Mp4Box>, String> {
    let mut result = Vec::new();
    let mut cursor = start;
    while cursor < end {
        if end - cursor < 8 {
            return Err("truncated MP4 box header".to_owned());
        }
        let short = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        let kind = bytes[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header) = if short == 1 {
            if end - cursor < 16 {
                return Err("truncated extended MP4 box header".to_owned());
            }
            (
                usize::try_from(u64::from_be_bytes(
                    bytes[cursor + 8..cursor + 16].try_into().unwrap(),
                ))
                .map_err(|_| "oversized MP4 box")?,
                16,
            )
        } else if short == 0 {
            (end - cursor, 8)
        } else {
            (short as usize, 8)
        };
        let box_end = cursor
            .checked_add(size)
            .ok_or_else(|| "MP4 box size overflow".to_owned())?;
        if size < header || box_end > end {
            return Err("invalid MP4 box size".to_owned());
        }
        result.push(Mp4Box {
            start: cursor,
            size,
            header,
            kind,
        });
        cursor = box_end;
    }
    Ok(result)
}

fn only_box(bytes: &[u8], kind: [u8; 4]) -> Result<Mp4Box, String> {
    boxes(bytes, 0, bytes.len())?
        .into_iter()
        .find(|item| item.kind == kind)
        .ok_or_else(|| format!("MP4 init has no {} box", String::from_utf8_lossy(&kind)))
}

fn make_box(kind: [u8; 4], children: impl IntoIterator<Item = Vec<u8>>) -> Result<Vec<u8>, String> {
    let content: Vec<u8> = children.into_iter().flatten().collect();
    let size = u32::try_from(8 + content.len()).map_err(|_| "MP4 box is too large")?;
    let mut result = Vec::with_capacity(size as usize);
    result.extend_from_slice(&size.to_be_bytes());
    result.extend_from_slice(&kind);
    result.extend_from_slice(&content);
    Ok(result)
}

fn owned_box(bytes: &[u8], item: Mp4Box) -> Vec<u8> {
    bytes[item.start..item.end()].to_vec()
}

fn track_id(bytes: &[u8]) -> Result<u32, String> {
    let track_box = only_box(bytes, *b"trak")?;
    let tkhd = boxes(bytes, track_box.content_start(), track_box.end())?
        .into_iter()
        .find(|item| &item.kind == b"tkhd")
        .ok_or_else(|| "MP4 track has no tkhd".to_owned())?;
    let version = *bytes
        .get(tkhd.content_start())
        .ok_or_else(|| "truncated tkhd version".to_owned())?;
    let offset = tkhd.content_start() + if version == 1 { 20 } else { 12 };
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| "truncated tkhd track id".to_owned())
}

fn patch_track(bytes: &mut [u8], new_id: u32) -> Result<(), String> {
    let track_box = only_box(bytes, *b"trak")?;
    let tkhd = boxes(bytes, track_box.content_start(), track_box.end())?
        .into_iter()
        .find(|item| &item.kind == b"tkhd")
        .ok_or_else(|| "MP4 track has no tkhd".to_owned())?;
    let version = *bytes
        .get(tkhd.content_start())
        .ok_or_else(|| "truncated tkhd version".to_owned())?;
    let offset = tkhd.content_start() + if version == 1 { 20 } else { 12 };
    bytes
        .get_mut(offset..offset + 4)
        .ok_or_else(|| "truncated tkhd track id".to_owned())?
        .copy_from_slice(&new_id.to_be_bytes());
    Ok(())
}

fn patch_trex(trex: &mut [u8], new_id: u32) -> Result<(), String> {
    let item = only_box(trex, *b"trex")?;
    let offset = item.content_start() + 4;
    if offset + 4 > trex.len() {
        return Err("truncated trex track id".to_owned());
    }
    trex[offset..offset + 4].copy_from_slice(&new_id.to_be_bytes());
    Ok(())
}

fn full_box_version(bytes: &[u8], item: Mp4Box) -> Result<u8, String> {
    bytes
        .get(item.content_start())
        .copied()
        .ok_or_else(|| format!("truncated {} version", String::from_utf8_lossy(&item.kind)))
}

fn read_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| format!("truncated {field}"))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "finite non-negative value is range-checked before rounded conversion"
)]
fn duration_units(seconds: f64, timescale: u32) -> Result<u64, String> {
    if !seconds.is_finite() || seconds < 0.0 || timescale == 0 {
        return Err("invalid MP4 duration or timescale".to_owned());
    }
    let units = (seconds * f64::from(timescale)).round();
    if units > u64::MAX as f64 {
        return Err("MP4 duration overflow".to_owned());
    }
    Ok(units as u64)
}

fn patch_duration_field(
    bytes: &mut [u8],
    offset: usize,
    version: u8,
    duration: u64,
    field: &str,
) -> Result<(), String> {
    if version == 1 {
        bytes
            .get_mut(offset..offset + 8)
            .ok_or_else(|| format!("truncated {field}"))?
            .copy_from_slice(&duration.to_be_bytes());
    } else {
        let duration = u32::try_from(duration).map_err(|_| format!("{field} duration overflow"))?;
        bytes
            .get_mut(offset..offset + 4)
            .ok_or_else(|| format!("truncated {field}"))?
            .copy_from_slice(&duration.to_be_bytes());
    }
    Ok(())
}

fn movie_timescale(bytes: &[u8]) -> Result<u32, String> {
    let mvhd = only_box(bytes, *b"mvhd")?;
    let version = full_box_version(bytes, mvhd)?;
    let offset = mvhd.content_start() + if version == 1 { 20 } else { 12 };
    read_u32(bytes, offset, "mvhd timescale")
}

fn patch_movie_duration(bytes: &mut [u8], seconds: f64) -> Result<u32, String> {
    let mvhd = only_box(bytes, *b"mvhd")?;
    let version = full_box_version(bytes, mvhd)?;
    let timescale_offset = mvhd.content_start() + if version == 1 { 20 } else { 12 };
    let timescale = read_u32(bytes, timescale_offset, "mvhd timescale")?;
    let duration = duration_units(seconds, timescale)?;
    patch_duration_field(bytes, timescale_offset + 4, version, duration, "mvhd")?;
    Ok(timescale)
}

fn patch_track_duration(
    track: &mut [u8],
    seconds: f64,
    movie_timescale: u32,
) -> Result<(), String> {
    let track_box = only_box(track, *b"trak")?;
    let children = boxes(track, track_box.content_start(), track_box.end())?;
    let tkhd = children
        .iter()
        .find(|item| &item.kind == b"tkhd")
        .copied()
        .ok_or_else(|| "MP4 track has no tkhd".to_owned())?;
    let tkhd_version = full_box_version(track, tkhd)?;
    let tkhd_duration_offset = tkhd.content_start() + if tkhd_version == 1 { 28 } else { 20 };
    patch_duration_field(
        track,
        tkhd_duration_offset,
        tkhd_version,
        duration_units(seconds, movie_timescale)?,
        "tkhd",
    )?;

    let mdia = children
        .iter()
        .find(|item| &item.kind == b"mdia")
        .copied()
        .ok_or_else(|| "MP4 track has no mdia".to_owned())?;
    let mdhd = boxes(track, mdia.content_start(), mdia.end())?
        .into_iter()
        .find(|item| &item.kind == b"mdhd")
        .ok_or_else(|| "MP4 track has no mdhd".to_owned())?;
    let mdhd_version = full_box_version(track, mdhd)?;
    let mdhd_timescale_offset = mdhd.content_start() + if mdhd_version == 1 { 20 } else { 12 };
    let media_timescale = read_u32(track, mdhd_timescale_offset, "mdhd timescale")?;
    patch_duration_field(
        track,
        mdhd_timescale_offset + 4,
        mdhd_version,
        duration_units(seconds, media_timescale)?,
        "mdhd",
    )
}

fn media_timescale(track: &[u8]) -> Result<u32, String> {
    let track_box = only_box(track, *b"trak")?;
    let mdia = boxes(track, track_box.content_start(), track_box.end())?
        .into_iter()
        .find(|item| &item.kind == b"mdia")
        .ok_or_else(|| "MP4 track has no mdia".to_owned())?;
    let mdhd = boxes(track, mdia.content_start(), mdia.end())?
        .into_iter()
        .find(|item| &item.kind == b"mdhd")
        .ok_or_else(|| "MP4 track has no mdhd".to_owned())?;
    let version = full_box_version(track, mdhd)?;
    read_u32(
        track,
        mdhd.content_start() + if version == 1 { 20 } else { 12 },
        "mdhd timescale",
    )
}

fn movie_extends_header(duration: u64) -> Result<Vec<u8>, String> {
    let mut content = Vec::new();
    if let Ok(short) = u32::try_from(duration) {
        content.extend_from_slice(&[0, 0, 0, 0]);
        content.extend_from_slice(&short.to_be_bytes());
    } else {
        content.extend_from_slice(&[1, 0, 0, 0]);
        content.extend_from_slice(&duration.to_be_bytes());
    }
    make_box(*b"mehd", [content])
}

/// Builds the first index a conventional fragmented-MP4 player sees. Each
/// reference covers one contiguous video+audio HLS time slice, so the player
/// can map the complete timeline to byte ranges without scanning every
/// fragment-local `sidx` first.
pub fn global_sidx(
    reference_id: u32,
    timescale: u32,
    references: &[SegmentReference],
) -> Result<Vec<u8>, String> {
    if timescale == 0 {
        return Err("zero sidx timescale".to_owned());
    }
    let count = u16::try_from(references.len()).map_err(|_| "too many HLS segments for sidx")?;
    let mut content = Vec::with_capacity(24 + references.len() * 12);
    content.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
    content.extend_from_slice(&reference_id.to_be_bytes());
    content.extend_from_slice(&timescale.to_be_bytes());
    content.extend_from_slice(&0_u32.to_be_bytes()); // earliest presentation time
    content.extend_from_slice(&0_u32.to_be_bytes()); // first byte follows this box
    content.extend_from_slice(&0_u16.to_be_bytes());
    content.extend_from_slice(&count.to_be_bytes());
    for reference in references {
        let size = u32::try_from(reference.size).map_err(|_| "oversized sidx reference")?;
        if size >= 1 << 31 {
            return Err("oversized sidx reference".to_owned());
        }
        let duration = u32::try_from(duration_units(reference.duration, timescale)?)
            .map_err(|_| "oversized sidx duration")?;
        content.extend_from_slice(&size.to_be_bytes()); // reference_type = media (0)
        content.extend_from_slice(&duration.to_be_bytes());
        // starts_with_SAP=1, SAP_type=1, SAP_delta_time=0. PeerTube's HLS
        // boundaries are independent fragment starts.
        content.extend_from_slice(&0x9000_0000_u32.to_be_bytes());
    }
    make_box(*b"sidx", [content])
}

/// Combines two HLS init ranges and returns `(init, video_id, video_timescale,
/// old_audio_id, new_audio_id)`. Media data is not present in either input.
#[allow(
    clippy::too_many_lines,
    reason = "one linear MP4 init composition pass"
)]
pub fn combine_init(
    video: &[u8],
    audio: &[u8],
    video_duration: f64,
    audio_duration: f64,
) -> Result<(Vec<u8>, u32, u32, u32, u32), String> {
    let video_ftyp = only_box(video, *b"ftyp")?;
    let video_moov = only_box(video, *b"moov")?;
    let audio_moov = only_box(audio, *b"moov")?;
    let video_children = boxes(video, video_moov.content_start(), video_moov.end())?;
    let audio_children = boxes(audio, audio_moov.content_start(), audio_moov.end())?;
    let mut video_tracks: Vec<Vec<u8>> = video_children
        .iter()
        .filter(|item| &item.kind == b"trak")
        .map(|item| owned_box(video, *item))
        .collect();
    let mut audio_track = audio_children
        .iter()
        .find(|item| &item.kind == b"trak")
        .map(|item| owned_box(audio, *item))
        .ok_or_else(|| "audio init has no track".to_owned())?;
    let old_audio_id = track_id(&audio_track)?;
    let video_ids = video_tracks
        .iter()
        .map(|track| track_id(track))
        .collect::<Result<Vec<_>, _>>()?;
    let video_id = *video_ids
        .first()
        .ok_or_else(|| "video init has no track".to_owned())?;
    let video_timescale = media_timescale(
        video_tracks
            .first()
            .ok_or_else(|| "video init has no track".to_owned())?,
    )?;
    let max_video_id = video_ids
        .into_iter()
        .max()
        .ok_or_else(|| "video init has no track".to_owned())?;
    let new_audio_id = max_video_id.saturating_add(1);
    patch_track(&mut audio_track, new_audio_id)?;

    let video_mvhd = video_children
        .iter()
        .find(|item| &item.kind == b"mvhd")
        .map(|item| owned_box(video, *item))
        .ok_or_else(|| "video init has no mvhd".to_owned())?;
    let movie_timescale = movie_timescale(&video_mvhd)?;
    for track in &mut video_tracks {
        patch_track_duration(track, video_duration, movie_timescale)?;
    }
    patch_track_duration(&mut audio_track, audio_duration, movie_timescale)?;

    let video_mvex = video_children
        .iter()
        .find(|item| &item.kind == b"mvex")
        .ok_or_else(|| "video init has no mvex".to_owned())?;
    let audio_mvex = audio_children
        .iter()
        .find(|item| &item.kind == b"mvex")
        .ok_or_else(|| "audio init has no mvex".to_owned())?;
    let mut mvex_children: Vec<Vec<u8>> =
        boxes(video, video_mvex.content_start(), video_mvex.end())?
            .into_iter()
            .filter(|item| &item.kind != b"mehd")
            .map(|item| owned_box(video, item))
            .collect();
    let mut audio_trex = boxes(audio, audio_mvex.content_start(), audio_mvex.end())?
        .into_iter()
        .find(|item| &item.kind == b"trex")
        .map(|item| owned_box(audio, item))
        .ok_or_else(|| "audio init has no trex".to_owned())?;
    patch_trex(&mut audio_trex, new_audio_id)?;
    mvex_children.insert(
        0,
        movie_extends_header(duration_units(
            video_duration.max(audio_duration),
            movie_timescale,
        )?)?,
    );
    mvex_children.push(audio_trex);
    let combined_mvex = make_box(*b"mvex", mvex_children)?;

    let mut inserted_audio = false;
    let mut next_video_track = video_tracks.into_iter();
    let mut moov_children = Vec::new();
    for child in video_children {
        if &child.kind == b"mvex" {
            if !inserted_audio {
                moov_children.push(audio_track.clone());
                inserted_audio = true;
            }
            moov_children.push(combined_mvex.clone());
        } else if &child.kind == b"trak" {
            moov_children.push(
                next_video_track
                    .next()
                    .ok_or_else(|| "video track count changed".to_owned())?,
            );
        } else {
            let mut bytes = owned_box(video, child);
            if &child.kind == b"mvhd" && bytes.len() >= 4 {
                patch_movie_duration(&mut bytes, video_duration.max(audio_duration))?;
                let next = new_audio_id.saturating_add(1).to_be_bytes();
                let end = bytes.len();
                bytes[end - 4..].copy_from_slice(&next);
            }
            moov_children.push(bytes);
        }
    }
    if !inserted_audio {
        return Err("video init never inserted the audio track".to_owned());
    }
    let moov = make_box(*b"moov", moov_children)?;
    let mut result = owned_box(video, video_ftyp);
    result.extend_from_slice(&moov);
    Ok((
        result,
        video_id,
        video_timescale,
        old_audio_id,
        new_audio_id,
    ))
}

/// Patches a muxed HLS init section with finite movie/track durations and
/// returns `(init, reference_track_id, reference_timescale)`. Unlike
/// [`combine_init`], no tracks are added or renumbered.
pub fn prepare_muxed_init(init: &[u8], duration: f64) -> Result<(Vec<u8>, u32, u32), String> {
    let ftyp = only_box(init, *b"ftyp")?;
    let moov = only_box(init, *b"moov")?;
    let children = boxes(init, moov.content_start(), moov.end())?;
    let first_track = children
        .iter()
        .find(|item| &item.kind == b"trak")
        .map(|item| owned_box(init, *item))
        .ok_or_else(|| "muxed init has no track".to_owned())?;
    let reference_id = track_id(&first_track)?;
    let reference_timescale = media_timescale(&first_track)?;
    let mvhd = children
        .iter()
        .find(|item| &item.kind == b"mvhd")
        .map(|item| owned_box(init, *item))
        .ok_or_else(|| "muxed init has no mvhd".to_owned())?;
    let movie_timescale = movie_timescale(&mvhd)?;
    let mut patched_children = Vec::with_capacity(children.len());
    for child in children {
        let mut bytes = owned_box(init, child);
        if &child.kind == b"mvhd" {
            patch_movie_duration(&mut bytes, duration)?;
        } else if &child.kind == b"trak" {
            patch_track_duration(&mut bytes, duration, movie_timescale)?;
        } else if &child.kind == b"mvex" {
            let mut mvex_children = boxes(init, child.content_start(), child.end())?
                .into_iter()
                .filter(|item| &item.kind != b"mehd")
                .map(|item| owned_box(init, item))
                .collect::<Vec<_>>();
            mvex_children.insert(
                0,
                movie_extends_header(duration_units(duration, movie_timescale)?)?,
            );
            bytes = make_box(*b"mvex", mvex_children)?;
        }
        patched_children.push(bytes);
    }
    let mut result = owned_box(init, ftyp);
    result.extend_from_slice(&make_box(*b"moov", patched_children)?);
    Ok((result, reference_id, reference_timescale))
}

/// Finds every fragment-local `sidx` before the muxed `moof`. A complete
/// global index precedes the virtual media, so these equal-sized boxes are
/// exposed as neutral `free` boxes to prevent clients from replacing the full
/// seek map with a four-second per-fragment map.
pub fn fragment_sidx_patches(prefix: &[u8]) -> Result<Vec<RelativePatch>, String> {
    let mut result = Vec::new();
    let mut cursor = 0_usize;
    while cursor.checked_add(8).is_some_and(|end| end <= prefix.len()) {
        let short = u32::from_be_bytes(prefix[cursor..cursor + 4].try_into().unwrap());
        let kind: [u8; 4] = prefix[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header) = if short == 1 {
            if cursor.checked_add(16).is_none_or(|end| end > prefix.len()) {
                return Err("truncated extended MP4 box header".to_owned());
            }
            (
                usize::try_from(u64::from_be_bytes(
                    prefix[cursor + 8..cursor + 16].try_into().unwrap(),
                ))
                .map_err(|_| "oversized MP4 box")?,
                16,
            )
        } else if short == 0 {
            (prefix.len() - cursor, 8)
        } else {
            (short as usize, 8)
        };
        if size < header {
            return Err("invalid MP4 box size".to_owned());
        }
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| "fragment MP4 box size overflow".to_owned())?;
        if end > prefix.len() {
            return Err("fragment metadata exceeds probe prefix".to_owned());
        }
        if &kind == b"sidx" {
            result.push(RelativePatch {
                offset: (cursor + 4) as u64,
                expected: *b"sidx",
                replacement: *b"free",
            });
        } else if &kind == b"moof" {
            break;
        }
        cursor = end;
    }
    if result.is_empty() {
        return Err("muxed fragment has no local sidx".to_owned());
    }
    Ok(result)
}

struct FragmentMetadata {
    mfhd: Vec<u8>,
    trafs: Vec<Vec<u8>>,
    mdat_data_offset: u64,
    moof_mdat_offset: u64,
}

fn fragment_metadata(prefix: &[u8]) -> Result<FragmentMetadata, String> {
    // The caller only needs to fetch the small box prefix, not the fragment's
    // (potentially megabytes-long) mdat payload. Accept a final truncated mdat
    // while still requiring every metadata box we inspect to be complete.
    let mut top_boxes = Vec::new();
    let mut cursor = 0_usize;
    while cursor.checked_add(8).is_some_and(|end| end <= prefix.len()) {
        let short = u32::from_be_bytes(prefix[cursor..cursor + 4].try_into().unwrap());
        let kind: [u8; 4] = prefix[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header) = if short == 1 {
            if cursor.checked_add(16).is_none_or(|end| end > prefix.len()) {
                return Err("truncated extended MP4 box header".to_owned());
            }
            (
                usize::try_from(u64::from_be_bytes(
                    prefix[cursor + 8..cursor + 16].try_into().unwrap(),
                ))
                .map_err(|_| "oversized MP4 box")?,
                16,
            )
        } else if short == 0 {
            (prefix.len() - cursor, 8)
        } else {
            (short as usize, 8)
        };
        if size < header {
            return Err("invalid MP4 box size".to_owned());
        }
        let box_end = cursor
            .checked_add(size)
            .ok_or_else(|| "fragment MP4 box size overflow".to_owned())?;
        if box_end > prefix.len() && &kind != b"mdat" {
            return Err("fragment metadata exceeds probe prefix".to_owned());
        }
        top_boxes.push(Mp4Box {
            start: cursor,
            size,
            header,
            kind,
        });
        if box_end > prefix.len() {
            break;
        }
        cursor = box_end;
    }
    top_boxes
        .iter()
        .find(|item| &item.kind == b"sidx")
        .ok_or_else(|| "fragment lacks sidx".to_owned())?;
    let moof = top_boxes
        .iter()
        .find(|item| &item.kind == b"moof")
        .copied()
        .ok_or_else(|| "fragment lacks moof".to_owned())?;
    let mdat = top_boxes
        .iter()
        .find(|item| &item.kind == b"mdat")
        .ok_or_else(|| "fragment lacks mdat".to_owned())?;
    let children = boxes(prefix, moof.content_start(), moof.end())?;
    let mfhd = children
        .iter()
        .find(|item| &item.kind == b"mfhd")
        .map(|item| owned_box(prefix, *item))
        .ok_or_else(|| "fragment moof lacks mfhd".to_owned())?;
    let trafs = children
        .iter()
        .filter(|item| &item.kind == b"traf")
        .map(|item| owned_box(prefix, *item))
        .collect::<Vec<_>>();
    if trafs.is_empty() {
        return Err("fragment moof lacks traf".to_owned());
    }
    let mdat_data_offset =
        u64::try_from(mdat.start + mdat.header).map_err(|_| "fragment mdat offset overflow")?;
    let moof_start = u64::try_from(moof.start).map_err(|_| "fragment moof offset overflow")?;
    Ok(FragmentMetadata {
        mfhd,
        trafs,
        mdat_data_offset,
        moof_mdat_offset: mdat_data_offset
            .checked_sub(moof_start)
            .ok_or_else(|| "fragment mdat precedes moof".to_owned())?,
    })
}

fn patch_traf(
    traf: &mut [u8],
    old_track_id: Option<u32>,
    new_track_id: Option<u32>,
    old_mdat_offset: u64,
    new_mdat_offset: u64,
    header_start: u64,
) -> Result<(), String> {
    let traf_box = only_box(traf, *b"traf")?;
    let mut saw_tfhd = false;
    let mut saw_data_offset = false;
    for leaf in boxes(traf, traf_box.content_start(), traf_box.end())? {
        if &leaf.kind == b"tfhd" {
            saw_tfhd = true;
            let flags = u32::from_be_bytes([
                0,
                traf[leaf.content_start() + 1],
                traf[leaf.content_start() + 2],
                traf[leaf.content_start() + 3],
            ]);
            let track_offset = leaf.content_start() + 4;
            let current = read_u32(traf, track_offset, "tfhd track id")?;
            if let (Some(old), Some(new)) = (old_track_id, new_track_id) {
                if current != old {
                    return Err("audio fragment track id changed".to_owned());
                }
                traf[track_offset..track_offset + 4].copy_from_slice(&new.to_be_bytes());
            }
            if flags & 1 != 0 {
                traf.get_mut(track_offset + 4..track_offset + 12)
                    .ok_or_else(|| "truncated tfhd base data offset".to_owned())?
                    .copy_from_slice(&header_start.to_be_bytes());
            } else if flags & 0x02_0000 == 0 {
                return Err("fragment tfhd has no explicit moof data base".to_owned());
            }
        } else if &leaf.kind == b"trun" {
            let flags = u32::from_be_bytes([
                0,
                traf[leaf.content_start() + 1],
                traf[leaf.content_start() + 2],
                traf[leaf.content_start() + 3],
            ]);
            if flags & 1 == 0 {
                return Err("fragment trun has no data offset".to_owned());
            }
            let offset = leaf.content_start() + 8;
            let old = i64::from(i32::from_be_bytes(
                traf.get(offset..offset + 4)
                    .and_then(|value| value.try_into().ok())
                    .ok_or_else(|| "truncated trun data offset".to_owned())?,
            ));
            let delta = old - i64::try_from(old_mdat_offset).map_err(|_| "mdat offset overflow")?;
            let replacement = i64::try_from(new_mdat_offset)
                .map_err(|_| "mdat offset overflow")?
                .checked_add(delta)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| "combined trun data offset overflow".to_owned())?;
            traf[offset..offset + 4].copy_from_slice(&replacement.to_be_bytes());
            saw_data_offset = true;
        }
    }
    if !saw_tfhd || !saw_data_offset {
        return Err("fragment traf lacks tfhd/trun data offset".to_owned());
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct TfhdDefaults {
    duration: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
}

fn normalized_tfhd(tfhd: &[u8]) -> Result<(Vec<u8>, TfhdDefaults), String> {
    let item = only_box(tfhd, *b"tfhd")?;
    let full = read_u32(tfhd, item.content_start(), "tfhd full header")?;
    let flags = full & 0x00ff_ffff;
    let mut cursor = item.content_start() + 4;
    let track_id = read_u32(tfhd, cursor, "tfhd track id")?;
    cursor += 4;
    let mut content = Vec::new();
    content.extend_from_slice(&(full & !0x38).to_be_bytes());
    content.extend_from_slice(&track_id.to_be_bytes());
    for (flag, width, field) in [
        (0x01, 8_usize, "tfhd base data offset"),
        (0x02, 4_usize, "tfhd sample description index"),
    ] {
        if flags & flag != 0 {
            let value = tfhd
                .get(cursor..cursor + width)
                .ok_or_else(|| format!("truncated {field}"))?;
            content.extend_from_slice(value);
            cursor += width;
        }
    }
    let mut take_default = |flag: u32, field: &str| -> Result<Option<u32>, String> {
        if flags & flag == 0 {
            return Ok(None);
        }
        let value = read_u32(tfhd, cursor, field)?;
        cursor += 4;
        Ok(Some(value))
    };
    let defaults = TfhdDefaults {
        duration: take_default(0x08, "tfhd default sample duration")?,
        size: take_default(0x10, "tfhd default sample size")?,
        flags: take_default(0x20, "tfhd default sample flags")?,
    };
    if cursor != item.end() {
        return Err("tfhd has unsupported trailing metadata".to_owned());
    }
    Ok((make_box(*b"tfhd", [content])?, defaults))
}

fn explicit_trun(trun: &[u8], defaults: TfhdDefaults) -> Result<Vec<u8>, String> {
    let item = only_box(trun, *b"trun")?;
    let full = read_u32(trun, item.content_start(), "trun full header")?;
    let flags = full & 0x00ff_ffff;
    if flags & !0x0f05 != 0 || flags & 0x01 == 0 || flags & 0x04 != 0 && flags & 0x400 != 0 {
        return Err("trun has unsupported flags for merging".to_owned());
    }
    let sample_count = read_u32(trun, item.content_start() + 4, "trun sample count")?;
    let mut cursor = item.content_start() + 8;
    let data_offset = read_u32(trun, cursor, "trun data offset")?;
    cursor += 4;
    let first_sample_flags = if flags & 0x04 != 0 {
        let value = read_u32(trun, cursor, "trun first sample flags")?;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    let mut samples = Vec::with_capacity(
        usize::try_from(sample_count).map_err(|_| "oversized trun sample count")?,
    );
    for index in 0..sample_count {
        let duration = if flags & 0x100 != 0 {
            let value = read_u32(trun, cursor, "trun sample duration")?;
            cursor += 4;
            value
        } else {
            defaults
                .duration
                .ok_or_else(|| "trun sample has no duration default".to_owned())?
        };
        let size = if flags & 0x200 != 0 {
            let value = read_u32(trun, cursor, "trun sample size")?;
            cursor += 4;
            value
        } else {
            defaults
                .size
                .ok_or_else(|| "trun sample has no size default".to_owned())?
        };
        let sample_flags = if flags & 0x400 != 0 {
            let value = read_u32(trun, cursor, "trun sample flags")?;
            cursor += 4;
            Some(value)
        } else if index == 0 {
            first_sample_flags.or(defaults.flags)
        } else {
            defaults.flags
        }
        .ok_or_else(|| "trun sample has no flags default".to_owned())?;
        let composition_offset = if flags & 0x800 != 0 {
            let value = read_u32(trun, cursor, "trun composition offset")?;
            cursor += 4;
            Some(value)
        } else {
            None
        };
        samples.push((duration, size, sample_flags, composition_offset));
    }
    if cursor != item.end() {
        return Err("trun has unsupported trailing metadata".to_owned());
    }
    let new_flags = (flags & 0x800) | 0x701;
    let mut content = Vec::new();
    content.extend_from_slice(&((full & 0xff00_0000) | new_flags).to_be_bytes());
    content.extend_from_slice(&sample_count.to_be_bytes());
    content.extend_from_slice(&data_offset.to_be_bytes());
    for (duration, size, sample_flags, composition_offset) in samples {
        content.extend_from_slice(&duration.to_be_bytes());
        content.extend_from_slice(&size.to_be_bytes());
        content.extend_from_slice(&sample_flags.to_be_bytes());
        if let Some(offset) = composition_offset {
            content.extend_from_slice(&offset.to_be_bytes());
        }
    }
    make_box(*b"trun", [content])
}

/// Collapses consecutive clear-media fragments for one track into the single
/// `traf` shape understood by Media3. Every source `trun` keeps its patched
/// data offset, while changing `tfhd` sample defaults are materialized into
/// explicit per-sample fields beneath the first fragment's `tfdt`.
///
/// Refuse auxiliary/encryption boxes and changing `tfhd` defaults. The caller
/// can then preserve the former multi-`traf` representation as a compatibility
/// fallback for an unfamiliar origin instead of corrupting its sample table.
fn merge_track_fragments(trafs: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if trafs.is_empty() {
        return Err("cannot merge an empty track fragment set".to_owned());
    }
    if trafs.len() == 1 {
        return Ok(trafs[0].clone());
    }
    let mut common_tfhd: Option<Vec<u8>> = None;
    let mut first_tfdt: Option<Vec<u8>> = None;
    let mut truns = Vec::new();
    for (index, traf) in trafs.iter().enumerate() {
        let container = only_box(traf, *b"traf")?;
        let mut tfhd = None;
        let mut tfdt = None;
        let mut fragment_truns = Vec::new();
        for child in boxes(traf, container.content_start(), container.end())? {
            match &child.kind {
                b"tfhd" => {
                    let normalized = normalized_tfhd(&owned_box(traf, child))?;
                    if tfhd.replace(normalized).is_some() {
                        return Err("track fragment has multiple tfhd boxes".to_owned());
                    }
                }
                b"tfdt" => {
                    if tfdt.replace(owned_box(traf, child)).is_some() {
                        return Err("track fragment has multiple tfdt boxes".to_owned());
                    }
                }
                b"trun" => fragment_truns.push(owned_box(traf, child)),
                _ => return Err("track fragment has unsupported merge metadata".to_owned()),
            }
        }
        let (tfhd, defaults) = tfhd.ok_or_else(|| "track fragment has no tfhd".to_owned())?;
        if let Some(common) = &common_tfhd {
            if common != &tfhd {
                return Err("track fragment tfhd base changed".to_owned());
            }
        } else {
            common_tfhd = Some(tfhd);
        }
        let tfdt = tfdt.ok_or_else(|| "track fragment has no tfdt".to_owned())?;
        if index == 0 {
            first_tfdt = Some(tfdt);
        }
        if fragment_truns.is_empty() {
            return Err("track fragment has no trun".to_owned());
        }
        truns.extend(
            fragment_truns
                .iter()
                .map(|trun| explicit_trun(trun, defaults))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let mut children = vec![common_tfhd.unwrap(), first_tfdt.unwrap()];
    children.extend(truns);
    make_box(*b"traf", children)
}

/// Builds one fixed-size fragment header containing both tracks and an outer
/// `mdat`. The original fragment bytes follow as opaque payload within it.
#[allow(clippy::too_many_arguments)]
pub fn combined_fragment_header(
    video_prefix: &[u8],
    video_len: u64,
    audio_prefixes: &[Vec<u8>],
    audio_lens: &[u64],
    old_audio_id: u32,
    new_audio_id: u32,
    header_start: u64,
) -> Result<Vec<u8>, String> {
    if audio_prefixes.len() != audio_lens.len() {
        return Err("combined fragment has mismatched audio parts".to_owned());
    }
    let video = fragment_metadata(video_prefix)?;
    let mut children = vec![video.mfhd];
    for mut traf in video.trafs {
        patch_traf(
            &mut traf,
            None,
            None,
            video.moof_mdat_offset,
            FRAGMENT_HEADER_BYTES
                .checked_add(video.mdat_data_offset)
                .ok_or_else(|| "combined video data offset overflow".to_owned())?,
            header_start,
        )?;
        children.push(traf);
    }
    let mut preceding = FRAGMENT_HEADER_BYTES
        .checked_add(video_len)
        .ok_or_else(|| "combined audio data offset overflow".to_owned())?;
    let mut patched_audio_trafs = Vec::new();
    for (prefix, len) in audio_prefixes.iter().zip(audio_lens) {
        let audio = fragment_metadata(prefix)?;
        let data_offset = preceding
            .checked_add(audio.mdat_data_offset)
            .ok_or_else(|| "combined audio data offset overflow".to_owned())?;
        for mut traf in audio.trafs {
            patch_traf(
                &mut traf,
                Some(old_audio_id),
                Some(new_audio_id),
                audio.moof_mdat_offset,
                data_offset,
                header_start,
            )?;
            patched_audio_trafs.push(traf);
        }
        preceding = preceding
            .checked_add(*len)
            .ok_or_else(|| "combined audio fragment offset overflow".to_owned())?;
    }
    let slot = usize::try_from(FRAGMENT_HEADER_BYTES).unwrap();
    if patched_audio_trafs.len() > 1 {
        match merge_track_fragments(&patched_audio_trafs) {
            Ok(merged) => {
                let mut merged_children = children.clone();
                merged_children.push(merged);
                let merged_moof = make_box(*b"moof", merged_children.clone())?;
                if merged_moof.len() + 8 <= slot {
                    children = merged_children;
                } else {
                    tracing::debug!(
                        "keeping separate audio trafs because explicit runs exceed the header slot"
                    );
                    children.extend(patched_audio_trafs);
                }
            }
            Err(error) => {
                tracing::debug!(%error, "keeping separate audio trafs for unfamiliar metadata");
                children.extend(patched_audio_trafs);
            }
        }
    } else {
        // Preserve the established one-audio-fragment representation exactly.
        children.extend(patched_audio_trafs);
    }
    let moof = make_box(*b"moof", children)?;
    if moof.len() + 8 > slot {
        return Err("combined fragment header exceeds reserved slot".to_owned());
    }
    // Media3 consumes every run announced by a moof while inside the *current*
    // mdat. Enclose the padding and both untouched source fragments in one
    // virtual mdat; their original box headers are merely unreferenced bytes
    // inside its payload. Chromium also accepts this conventional one-moof,
    // one-mdat subsegment shape.
    let referenced_size = FRAGMENT_HEADER_BYTES
        .checked_add(video_len)
        .and_then(|size| {
            audio_lens
                .iter()
                .try_fold(size, |total, len| total.checked_add(*len))
        })
        .ok_or_else(|| "combined fragment size overflow".to_owned())?;
    let mdat_size = referenced_size
        .checked_sub(moof.len() as u64)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| "combined mdat is too large".to_owned())?;
    let mut padding = Vec::with_capacity(slot - moof.len());
    padding.extend_from_slice(&mdat_size.to_be_bytes());
    padding.extend_from_slice(b"mdat");
    padding.resize(slot - moof.len(), 0);
    let mut result = moof;
    result.extend_from_slice(&padding);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::process::Command;

    use super::*;

    #[test]
    fn parses_single_file_hls_ranges() {
        let parsed = parse_media_playlist(
            "#EXTM3U\n#EXT-X-MAP:URI=\"x.mp4\",BYTERANGE=\"100@0\"\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:20@100\nx.mp4\n\
             #EXTINF:4,\n#EXT-X-BYTERANGE:30\nx.mp4\n",
        )
        .unwrap();
        assert_eq!(parsed.init.start, 0);
        assert_eq!(parsed.init.len, 100);
        assert_eq!(parsed.segments[0].bytes.start, 100);
        assert_eq!(parsed.segments[1].bytes.start, 120);
        assert!((parsed.segments[0].duration - 4.0).abs() < f64::EPSILON);
        assert!((parsed.duration() - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_overflowing_remote_offsets() {
        let playlist = format!(
            "#EXTM3U\n#EXT-X-MAP:URI=\"x.mp4\",BYTERANGE=\"1@{}\"\n",
            u64::MAX
        );
        assert_eq!(
            parse_media_playlist(&playlist).unwrap_err(),
            "HLS byte range overflow"
        );

        let mut extended = Vec::from([0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        extended.extend_from_slice(&[0, 0, 0, 1, b'm', b'o', b'o', b'v']);
        extended.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(
            boxes(&extended, 0, extended.len()).unwrap_err(),
            "MP4 box size overflow"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "generates, composes, and decodes one fixture"
    )]
    fn synthesized_split_hls_is_a_two_track_mp4() {
        if Command::new("ffmpeg").arg("-version").output().is_err()
            || Command::new("ffprobe").arg("-version").output().is_err()
        {
            eprintln!("ffmpeg/ffprobe unavailable; skipping format validation");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let video_playlist_path = temp.path().join("video.m3u8");
        let audio_playlist_path = temp.path().join("audio.m3u8");
        let video_status = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x90:rate=10",
                "-t",
                "6",
                "-an",
                "-c:v",
                "libx264",
                "-g",
                "20",
                "-keyint_min",
                "20",
                "-sc_threshold",
                "0",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "hls",
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "single_file",
                "-hls_playlist_type",
                "vod",
                "-hls_time",
                "2",
            ])
            .arg(&video_playlist_path)
            .status()
            .unwrap();
        assert!(video_status.success());
        let audio_status = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "6",
                "-vn",
                "-c:a",
                "aac",
                "-f",
                "hls",
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "single_file",
                "-hls_playlist_type",
                "vod",
                "-hls_time",
                "1",
            ])
            .arg(&audio_playlist_path)
            .status()
            .unwrap();
        assert!(audio_status.success());

        let video_playlist =
            parse_media_playlist(&std::fs::read_to_string(&video_playlist_path).unwrap()).unwrap();
        let audio_playlist =
            parse_media_playlist(&std::fs::read_to_string(&audio_playlist_path).unwrap()).unwrap();
        let video_file = std::fs::read(temp.path().join("video.m4s")).unwrap();
        let audio_file = std::fs::read(temp.path().join("audio.m4s")).unwrap();
        let video_init = part(&video_file, video_playlist.init);
        let audio_init = part(&audio_file, audio_playlist.init);
        let (mut output, video_id, video_timescale, old_audio_id, new_audio_id) = combine_init(
            video_init,
            audio_init,
            video_playlist.duration(),
            audio_playlist.duration(),
        )
        .unwrap();
        let groups = segment_groups(&video_playlist, &audio_playlist);
        assert!(
            groups.iter().any(|group| group.audio.len() > 1),
            "fixture must exercise multiple audio fragments per video fragment"
        );
        let references: Vec<_> = groups
            .iter()
            .map(|group| SegmentReference {
                size: FRAGMENT_HEADER_BYTES
                    + video_playlist.segments[group.video].bytes.len
                    + audio_playlist.segments[group.audio.clone()]
                        .iter()
                        .map(|segment| segment.bytes.len)
                        .sum::<u64>(),
                duration: group.duration,
            })
            .collect();
        let index = global_sidx(video_id, video_timescale, &references).unwrap();
        assert_eq!(&index[4..8], b"sidx");
        assert_eq!(
            u16::from_be_bytes(index[30..32].try_into().unwrap()) as usize,
            groups.len()
        );
        let indexed_duration: u64 = index[32..]
            .as_chunks::<12>()
            .0
            .iter()
            .map(|entry| u64::from(u32::from_be_bytes(entry[4..8].try_into().unwrap())))
            .sum();
        let expected_duration = video_playlist.duration().max(audio_playlist.duration());
        assert!(
            indexed_duration.abs_diff(duration_units(expected_duration, video_timescale).unwrap())
                <= 1
        );

        let moov = only_box(&output, *b"moov").unwrap();
        let mvhd = boxes(&output, moov.content_start(), moov.end())
            .unwrap()
            .into_iter()
            .find(|item| &item.kind == b"mvhd")
            .unwrap();
        let version = full_box_version(&output, mvhd).unwrap();
        let timescale_offset = mvhd.content_start() + if version == 1 { 20 } else { 12 };
        let timescale = read_u32(&output, timescale_offset, "timescale").unwrap();
        let movie_duration = read_u32(&output, timescale_offset + 4, "duration").unwrap();
        assert!(
            (f64::from(movie_duration) / f64::from(timescale) - expected_duration).abs() < 0.01
        );
        output.extend_from_slice(&index);
        for group in groups {
            let video = &video_playlist.segments[group.video];
            let audio_segments = &audio_playlist.segments[group.audio.clone()];
            let audio_prefixes = audio_segments
                .iter()
                .map(|audio| part(&audio_file, audio.bytes).to_vec())
                .collect::<Vec<_>>();
            let audio_lens = audio_segments
                .iter()
                .map(|audio| audio.bytes.len)
                .collect::<Vec<_>>();
            let header = combined_fragment_header(
                part(&video_file, video.bytes),
                video.bytes.len,
                &audio_prefixes,
                &audio_lens,
                old_audio_id,
                new_audio_id,
                output.len() as u64,
            )
            .unwrap();
            assert_eq!(header.len() as u64, FRAGMENT_HEADER_BYTES);
            output.extend_from_slice(&header);
            output.extend_from_slice(part(&video_file, video.bytes));
            for audio in audio_segments {
                output.extend_from_slice(part(&audio_file, audio.bytes));
            }
        }
        let top = boxes(&output, 0, output.len()).unwrap();
        let media = top
            .iter()
            .skip_while(|item| &item.kind != b"moof")
            .collect::<Vec<_>>();
        assert_eq!(media.len(), references.len() * 2);
        for pair in media.as_chunks::<2>().0 {
            assert_eq!(&pair[0].kind, b"moof");
            assert_eq!(&pair[1].kind, b"mdat");
            assert_eq!(pair[0].end(), pair[1].start);
        }
        for pair in media.windows(2) {
            if &pair[0].kind == b"mdat" {
                assert_eq!(pair[0].end(), pair[1].start);
            }
        }
        let mut saw_multi_run_audio = false;
        for moof in top.iter().filter(|item| &item.kind == b"moof") {
            let mut track_ids = HashSet::new();
            for traf in boxes(&output, moof.content_start(), moof.end())
                .unwrap()
                .into_iter()
                .filter(|item| &item.kind == b"traf")
            {
                let leaves = boxes(&output, traf.content_start(), traf.end()).unwrap();
                let tfhd = leaves.iter().find(|item| &item.kind == b"tfhd").unwrap();
                let track_id =
                    read_u32(&output, tfhd.content_start() + 4, "fixture tfhd track id").unwrap();
                assert!(
                    track_ids.insert(track_id),
                    "one moof must not repeat track {track_id}"
                );
                let runs = leaves.iter().filter(|item| &item.kind == b"trun").count();
                saw_multi_run_audio |= track_id == new_audio_id && runs > 1;
            }
        }
        assert!(
            saw_multi_run_audio,
            "mismatched audio fragments are retained as multiple runs in one traf"
        );
        let first_video = &video_playlist.segments[0];
        let video_only = combined_fragment_header(
            part(&video_file, first_video.bytes),
            first_video.bytes.len,
            &[],
            &[],
            old_audio_id,
            new_audio_id,
            output.len() as u64,
        )
        .unwrap();
        assert_eq!(video_only.len() as u64, FRAGMENT_HEADER_BYTES);
        let output_path = temp.path().join("combined.mp4");
        std::fs::write(&output_path, &output).unwrap();
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "compact=p=0:nk=1",
            ])
            .arg(&output_path)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "ffprobe rejected virtual MP4: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let streams = String::from_utf8_lossy(&probe.stdout);
        assert!(streams.lines().any(|line| line == "video"), "{streams}");
        assert!(streams.lines().any(|line| line == "audio"), "{streams}");
        let duration = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(&output_path)
            .output()
            .unwrap();
        let duration: f64 = String::from_utf8_lossy(&duration.stdout)
            .trim()
            .parse()
            .unwrap();
        assert!(
            duration >= expected_duration && duration < expected_duration + 1.0,
            "duration={duration}, expected={expected_duration}"
        );
        let decode = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&output_path)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            decode.status.success(),
            "ffmpeg could not decode both virtual tracks: {}",
            String::from_utf8_lossy(&decode.stderr)
        );
    }

    fn part(bytes: &[u8], part: BytePart) -> &[u8] {
        let start = usize::try_from(part.start).unwrap();
        let end = usize::try_from(part.start + part.len).unwrap();
        &bytes[start..end]
    }
}
