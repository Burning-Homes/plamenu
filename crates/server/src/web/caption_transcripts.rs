//! Readable, on-demand caption text. This is not an author-written visual
//! description: captions may omit visual information altogether.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{media, video_captions};

use super::{i18n::Locale, layout, pages, session::MaybeWebUser};
use crate::{AppState, error::ApiError};

#[derive(Debug, PartialEq)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

fn timestamp_ms(input: &str) -> Option<u64> {
    let (whole, fraction) = input.split_once('.')?;
    if fraction.len() != 3 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<_> = whole.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (
            0,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    if seconds >= 60 || (parts.len() == 3 && minutes >= 60) {
        return None;
    }
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes.checked_mul(60_000)?)?
        .checked_add(seconds.checked_mul(1_000)?)?
        .checked_add(fraction.parse::<u64>().ok()?)
}

fn cue_text(lines: &[&str]) -> String {
    // WebVTT voice spans are annotations, not spoken words. Preserve their
    // speaker label in the readable rendering before stripping cue markup.
    let raw = lines.join(" ");
    let speaker = raw
        .strip_prefix("<v ")
        .and_then(|value| value.split_once('>'))
        .map(|(name, _)| name.trim())
        .filter(|name| !name.is_empty() && name.len() <= 100);
    let plain = plamenu_ap::text::sanitize_remote_plain(&raw);
    if let Some(speaker) = speaker {
        format!(
            "{}: {}",
            plamenu_ap::text::sanitize_remote_plain(speaker),
            plain.trim()
        )
    } else {
        plain.trim().to_owned()
    }
}

fn parse_cues(vtt: &str) -> Vec<Cue> {
    let normalized = vtt.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    for block in normalized.split("\n\n").take(5_001) {
        let lines: Vec<_> = block.lines().collect();
        let Some(timing_index) = lines.iter().position(|line| line.contains(" --> ")) else {
            continue;
        };
        if timing_index > 1
            || lines[..timing_index].iter().any(|line| {
                line.starts_with("NOTE") || line.starts_with("STYLE") || line.starts_with("REGION")
            })
        {
            continue;
        }
        let Some((start, rest)) = lines[timing_index].split_once(" --> ") else {
            continue;
        };
        let Some(end) = rest.split_whitespace().next() else {
            continue;
        };
        let (Some(start_ms), Some(end_ms)) = (timestamp_ms(start), timestamp_ms(end)) else {
            continue;
        };
        if end_ms <= start_ms {
            continue;
        }
        let text = cue_text(&lines[timing_index + 1..]);
        if !text.is_empty() {
            cues.push(Cue {
                start_ms,
                end_ms,
                text,
            });
        }
    }
    cues
}

fn clock_time(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one ACL-checked transcript and native HTML media-alternative template"
)]
pub async fn show(
    State(state): State<AppState>,
    Path((media_id, language)): Path<(i64, String)>,
    MaybeWebUser(session): MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !video_captions::valid_language(&language) {
        return Err(ApiError::NotFound);
    }
    let track = video_captions::for_media(&state.pool, &[media_id])
        .await?
        .remove(&media_id)
        .unwrap_or_default()
        .into_iter()
        .find(|track| track.language == language)
        .ok_or(ApiError::NotFound)?;
    let vtt = if track.legacy {
        media::public_caption_vtt(&state.pool, media_id)
            .await?
            .ok_or(ApiError::NotFound)?
    } else {
        crate::routes::media::public_language_caption_text(&state, media_id, &language)
            .await?
            .0
    };
    let file = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    let status_id = file.status_id.ok_or(ApiError::NotFound)?;
    let renditions = media::renditions_for(&state.pool, &[media_id]).await?;
    let playable = crate::entities::media_json_hls(
        &state.config.domain,
        &file,
        false,
        renditions.get(&media_id).map(Vec::as_slice),
    );
    let url = playable.get("url").and_then(serde_json::Value::as_str);
    let cues = parse_cues(&vtt);
    let locale = session
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |user| user.locale);
    let title = format!("{} · {language}", locale.text("caption-transcript-title"));
    let body = html! {
        section.column.caption-transcript {
            h1 { (title) }
            p { (locale.text("caption-transcript-disclaimer")) }
            @if let Some(description) = file.description.as_deref().filter(|s| !s.is_empty()) {
                details.media__transcript {
                    summary { (locale.text("videos-alt")) }
                    p { (description) }
                }
            }
            @if let Some(transcript) = file.transcript.as_deref().filter(|s| !s.is_empty()) {
                details.media__transcript {
                    summary { (locale.text("status-transcript")) }
                    p { (transcript) }
                }
            }
            nav aria-label=(locale.text("caption-transcript-title")) {
                a href=(format!("/web/statuses/{status_id}")) { (locale.text("caption-transcript-back")) }
                " · "
                a href=(crate::video::caption_public_url(&state.config.domain, media_id, &track)) {
                    (locale.text("caption-transcript-vtt"))
                }
            }
            @if let Some(url) = url {
                video controls playsinline preload="none" src=(url)
                    aria-label=(locale.text("caption-transcript-player")) {
                    track kind="captions"
                        src=(crate::video::caption_public_url(&state.config.domain, media_id, &track))
                        srclang=(language) label=(language) default;
                }
            }
            @if cues.is_empty() {
                p { (locale.text("caption-transcript-empty")) }
            } @else {
                ol class="caption-transcript__cues" lang=(language)
                    aria-label=(locale.text("caption-transcript-title")) {
                    @for cue in &cues {
                        li {
                            @if let Some(url) = url {
                                a href=(format!("{url}#t={}.{:03}", cue.start_ms / 1_000, cue.start_ms % 1_000))
                                    data-transcript-seek-ms=(cue.start_ms) {
                                    (clock_time(cue.start_ms))
                                }
                            } @else {
                                span { (clock_time(cue.start_ms)) }
                            }
                            p { (cue.text) }
                        }
                    }
                }
            }
        }
    };
    let page = layout::shell_visitor_localized(
        &title,
        session.as_ref(),
        pages::anon_nav(&state).await,
        &body,
        locale,
    );
    let mut response = page.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{Cue, parse_cues, timestamp_ms};

    #[test]
    fn valid_timestamps_and_overflow() {
        assert_eq!(timestamp_ms("01:02.003"), Some(62_003));
        assert_eq!(timestamp_ms("02:01:02.003"), Some(7_262_003));
        assert_eq!(timestamp_ms("00:60.000"), None);
        assert_eq!(timestamp_ms("00:00:60.000"), None);
        assert_eq!(timestamp_ms("18446744073709551615:00:00.000"), None);
    }

    #[test]
    fn extracts_cues_without_executing_markup() {
        let cues = parse_cues(
            "WEBVTT\n\nNOTE ignored\n00:00.000 --> 00:01.000\nbad\n\n1\n00:03.500 --> 00:05.000 align:start\n<v Alice>Hello &amp; <script>alert(1)</script>world</v>\n\n00:05.000 --> 00:04.000\ninvalid",
        );
        assert_eq!(
            cues,
            vec![Cue {
                start_ms: 3_500,
                end_ms: 5_000,
                text: "Alice: Hello & world".to_owned(),
            }]
        );
    }
}
