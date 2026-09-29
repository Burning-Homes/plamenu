//! Exact millisecond edit windows and accessible timeline transformation.
//! A trim cannot stream-copy a preceding keyframe or retain out-of-window cues.
use crate::error::ApiError;
use std::fmt::Write as _;

const MAX_MILLISECONDS: i64 = 604_800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    start_ms: i64,
    end_ms: i64,
}

fn invalid() -> ApiError {
    ApiError::Unprocessable("Choose a finite, increasing trim window in milliseconds".into())
}

impl Window {
    #[must_use]
    pub fn start_ms(self) -> i64 {
        self.start_ms
    }

    #[must_use]
    pub fn end_ms(self) -> i64 {
        self.end_ms
    }

    pub fn new(start_ms: i64, end_ms: i64) -> Result<Self, ApiError> {
        if start_ms < 0 || end_ms <= start_ms || end_ms > MAX_MILLISECONDS {
            return Err(invalid());
        }
        Ok(Self { start_ms, end_ms })
    }

    pub fn from_seconds(start: f64, end: f64) -> Result<Self, ApiError> {
        let millis = |value: f64| {
            let scaled = value * 1000.0;
            if !scaled.is_finite()
                || !(0.0..=604_800_000.0).contains(&scaled)
                || (scaled.round() - scaled).abs() > 0.0001
            {
                return Err(invalid());
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "finite, nonnegative, checked 7-day millisecond bound"
            )]
            Ok(scaled.round() as i64)
        };
        Self::new(millis(start)?, millis(end)?)
    }

    /// Map a new, relative cut into its retained source window. A later edit
    /// must never expand into footage excluded by an earlier public trim.
    pub fn within(self, parent: Self) -> Result<Self, ApiError> {
        if self.end_ms > parent.end_ms - parent.start_ms {
            return Err(invalid());
        }
        Self::new(
            parent.start_ms + self.start_ms,
            parent.start_ms + self.end_ms,
        )
    }

    #[must_use]
    pub fn start_argument(self) -> String {
        seconds(self.start_ms)
    }

    #[must_use]
    pub fn duration_argument(self) -> String {
        seconds(self.end_ms - self.start_ms)
    }

    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "checked 7-day millisecond window is exactly representable"
    )]
    pub fn duration_seconds(self) -> f64 {
        (self.end_ms - self.start_ms) as f64 / 1000.0
    }
}

fn seconds(ms: i64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

fn timestamp(ms: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn parse_timestamp(raw: &str) -> Option<i64> {
    let (clock, fraction) = raw.split_once('.')?;
    if fraction.len() != 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<_> = clock.split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.len() < 2 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let values: Vec<i64> = parts
        .iter()
        .map(|p| p.parse())
        .collect::<Result<_, _>>()
        .ok()?;
    let (hours, minutes, seconds) = if values.len() == 3 {
        (values[0], values[1], values[2])
    } else {
        (0, values[0], values[1])
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    let ms = hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(seconds * 1000)?
        .checked_add(fraction.parse().ok()?)?;
    (ms <= MAX_MILLISECONDS).then_some(ms)
}

fn bad_captions() -> ApiError {
    ApiError::Unprocessable(
        "These captions cannot be safely retimed. Correct their timed cues before trimming.".into(),
    )
}

/// Keep the section containing the cut's first frame as chapter zero. Later
/// whole-second chapter starts round upwards, never into a preceding section.
#[must_use]
pub fn chapters(
    input: &[plamenu_db::video_chapters::Chapter],
    window: Window,
) -> Vec<plamenu_db::video_chapters::Chapter> {
    let mut output = Vec::new();
    if let Some(first) = input
        .iter()
        .rev()
        .find(|c| i64::from(c.start_seconds) * 1000 <= window.start_ms)
    {
        let mut first = first.clone();
        first.start_seconds = 0;
        output.push(first);
    }
    for chapter in input {
        let start = i64::from(chapter.start_seconds) * 1000;
        if start <= window.start_ms || start >= window.end_ms {
            continue;
        }
        let relative = (start - window.start_ms + 999) / 1000;
        if relative * 1000 >= window.end_ms - window.start_ms {
            continue;
        }
        let mut chapter = chapter.clone();
        chapter.start_seconds = i32::try_from(relative).expect("bounded seven-day chapter window");
        output.push(chapter);
    }
    output
}

/// Preserve supported `WebVTT` positioning/styles, intersect cues with the cut,
/// and rebase both cue and inline karaoke timestamps. Comments are discarded:
/// hidden notes must not preserve words intentionally removed from a film.
/// Unknown timing syntax fails closed rather than losing an entire language.
pub fn captions(input: &str, window: Window) -> Result<Option<String>, ApiError> {
    if input.len() > 256 * 1024 || input.contains('\0') {
        return Err(bad_captions());
    }
    let normalized = input
        .strip_prefix('\u{feff}')
        .unwrap_or(input)
        .replace("\r\n", "\n");
    let mut blocks = normalized.split("\n\n").filter(|b| !b.trim().is_empty());
    let header = blocks.next().ok_or_else(bad_captions)?;
    let first = header.lines().next().unwrap_or_default();
    if !(first == "WEBVTT" || first.starts_with("WEBVTT ") || first.starts_with("WEBVTT\t"))
        || header.contains("-->")
        || header.contains("X-TIMESTAMP-MAP")
    {
        return Err(bad_captions());
    }
    let mut output = format!("{header}\n\n");
    let mut cues = 0;
    for block in blocks {
        if block == "NOTE" || block.starts_with("NOTE ") || block.starts_with("NOTE\n") {
            continue;
        }
        if block.starts_with("STYLE\n") || block.starts_with("REGION\n") {
            if block.contains("-->") {
                return Err(bad_captions());
            }
            output.push_str(block);
            output.push_str("\n\n");
            continue;
        }
        let lines: Vec<_> = block.lines().collect();
        let index = lines
            .iter()
            .position(|l| l.contains("-->"))
            .filter(|i| *i <= 1)
            .ok_or_else(bad_captions)?;
        let (start, after) = lines[index].split_once("-->").ok_or_else(bad_captions)?;
        let after = after.trim();
        let mut words = after.split_whitespace();
        let end_word = words.next().ok_or_else(bad_captions)?;
        let start = parse_timestamp(start.trim()).ok_or_else(bad_captions)?;
        let end = parse_timestamp(end_word).ok_or_else(bad_captions)?;
        if start >= end || lines.len() <= index + 1 {
            return Err(bad_captions());
        }
        if end <= window.start_ms || start >= window.end_ms {
            continue;
        }
        let clipped_start = start.max(window.start_ms) - window.start_ms;
        let clipped_end = end.min(window.end_ms) - window.start_ms;
        if index == 1 {
            output.push_str(lines[0]);
            output.push('\n');
        }
        let _ = write!(
            output,
            "{} --> {}",
            timestamp(clipped_start),
            timestamp(clipped_end)
        );
        let settings = after[end_word.len()..].trim();
        if !settings.is_empty() {
            output.push(' ');
            output.push_str(settings);
        }
        output.push('\n');
        let mut previous = start;
        for line in &lines[index + 1..] {
            output.push_str(&inline_timestamps(
                line,
                window,
                clipped_start,
                clipped_end,
                &mut previous,
                end,
            )?);
            output.push('\n');
        }
        output.push('\n');
        cues += 1;
    }
    Ok((cues > 0).then_some(output))
}

fn inline_timestamps(
    input: &str,
    window: Window,
    start: i64,
    end: i64,
    previous: &mut i64,
    cue_end: i64,
) -> Result<String, ApiError> {
    let mut output = String::new();
    let mut rest = input;
    while let Some(index) = rest.find('<') {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        let Some(close) = tail.find('>') else {
            if tail.as_bytes().get(1).is_some_and(u8::is_ascii_digit) {
                return Err(bad_captions());
            }
            output.push_str(tail);
            return Ok(output);
        };
        let tag = &tail[1..close];
        if tag.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            let absolute = parse_timestamp(tag).ok_or_else(bad_captions)?;
            if absolute <= *previous || absolute >= cue_end {
                return Err(bad_captions());
            }
            *previous = absolute;
            let relative = absolute - window.start_ms;
            if relative > start && relative < end {
                output.push('<');
                output.push_str(&timestamp(relative));
                output.push('>');
            }
        } else {
            output.push_str(&tail[..=close]);
        }
        rest = &tail[close + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_are_exact_bounded_and_cannot_expand_a_previous_trim() {
        assert_eq!(
            Window::from_seconds(1.001, 2.003).unwrap(),
            Window::new(1001, 2003).unwrap()
        );
        for (start, end) in [
            (f64::NAN, 3.0),
            (0.0, f64::INFINITY),
            (-1.0, 2.0),
            (2.0, 2.0),
            (0.0, 1.0001),
            (0.0, 604_801.0),
        ] {
            assert!(Window::from_seconds(start, end).is_err());
        }
        let parent = Window::new(3000, 7000).unwrap();
        assert_eq!(
            Window::new(1000, 3000).unwrap().within(parent).unwrap(),
            Window::new(4000, 6000).unwrap()
        );
        assert!(Window::new(1000, 5000).unwrap().within(parent).is_err());
        assert_eq!(parent.start_argument(), "3.000");
        assert_eq!(parent.duration_argument(), "4.000");
    }
    #[test]
    fn chapters_keep_the_current_section_and_do_not_round_into_preceding_footage() {
        let input =
            [(0, "Intro"), (3, "Middle"), (5, "End"), (9, "Outside")].map(|(start, title)| {
                plamenu_db::video_chapters::Chapter {
                    media_id: 1,
                    start_seconds: start,
                    title: title.into(),
                }
            });
        let out = chapters(&input, Window::new(3250, 9100).unwrap());
        assert_eq!(
            out.iter()
                .map(|c| (c.start_seconds, c.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "Middle"), (2, "End")]
        );
        assert_eq!(chapters(&input, Window::new(3000, 5000).unwrap()).len(), 1);
        assert_eq!(
            chapters(&input, Window::new(0, 1000).unwrap())[0].title,
            "Intro"
        );
    }
    #[test]
    fn cue_intersections_ids_settings_and_karaoke_timestamps_are_rebased() {
        let input = "WEBVTT\r\n\r\nNOTE hidden removed words\r\n\r\nfirst\r\n00:01.000 --> 00:03.000 line:90% align:start\r\n<c.blue>first<00:02.500>word</c>\r\n\r\n00:04.000 --> 00:08.000\r\nlast<00:06.000>word\r\n\r\n00:09.000 --> 00:10.000\r\noutside\r\n";
        let out = captions(input, Window::new(2000, 6000).unwrap())
            .unwrap()
            .unwrap();
        assert!(out.contains("first\n00:00:00.000 --> 00:00:01.000 line:90% align:start"));
        assert!(out.contains("<c.blue>first<00:00:00.500>word</c>"));
        assert!(out.contains("00:00:02.000 --> 00:00:04.000\nlastword"));
        assert!(!out.contains("outside"));
        assert!(!out.contains("removed words"));
    }
    #[test]
    fn malformed_or_offset_captions_fail_closed_and_empty_intersections_are_omitted() {
        let window = Window::new(3000, 5000).unwrap();
        for input in [
            "WEBVTT\n\n00:99.000 --> 00:99.100\ninvalid",
            "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:1\n\n00:01.000 --> 00:02.000\nno",
            "WEBVTT\n\n00:04.000 --> 00:05.000\nbad<00:4.500>timestamp",
            "WEBVTT\n\n00:04.000 --> 00:05.000\nbad<00:04.500",
            "WEBVTT\n\n00:04.000 --> 00:05.000\na<00:04.600>b\nc<00:04.500>d",
            "WEBVTT\n\n00:05.000 --> 00:04.000\nbackwards",
        ] {
            assert!(captions(input, window).is_err());
        }
        assert!(
            captions("WEBVTT\n\n00:01.000 --> 00:02.000\nremoved\n", window)
                .unwrap()
                .is_none()
        );
    }
}
