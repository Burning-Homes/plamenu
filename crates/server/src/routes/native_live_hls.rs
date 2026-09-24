//! Public, generation-fenced HLS reads for Plamenu-origin broadcasts.

use std::fmt::Write as _;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

const M3U8: &str = "application/vnd.apple.mpegurl";
const MAX_PLAYLIST: u64 = 256 * 1024;
const MAX_SEGMENT: u64 = 64 * 1024 * 1024;

pub async fn master(state: &AppState, media_id: i64) -> Result<Option<Response>, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let Some(session) = plamenu_db::native_live::public_session(&state.pool, media_id).await?
    else {
        return Ok(None);
    };
    let body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=4000000\n/media/hls/{media_id}/local/{session}/index.m3u8\n"
    );
    Ok(Some(
        (
            [
                (header::CONTENT_TYPE, M3U8),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
    ))
}

pub async fn child(
    State(state): State<AppState>,
    Path((media_id, session, name)): Path<(i64, String, String)>,
) -> Result<Response, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let current = plamenu_db::native_live::public_session(&state.pool, media_id).await?;
    if current.as_deref() != Some(session.as_str()) || !valid_uuid(&session) {
        return Err(ApiError::NotFound);
    }
    let playlist = name == "index.m3u8";
    if !playlist && !valid_segment(&name) {
        return Err(ApiError::NotFound);
    }
    let path = state
        .config
        .media_dir
        .join("native-live")
        .join(media_id.to_string())
        .join(&session)
        .join(&name);
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if !meta.is_file() || meta.len() > if playlist { MAX_PLAYLIST } else { MAX_SEGMENT } {
        return Err(ApiError::NotFound);
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if playlist {
        let source = String::from_utf8(bytes).map_err(|_| ApiError::NotFound)?;
        let mut output = String::with_capacity(source.len() + 512);
        for line in source.lines() {
            if line.starts_with('#') || line.is_empty() {
                output.push_str(line);
            } else if valid_segment(line) {
                write!(output, "/media/hls/{media_id}/local/{session}/{line}")
                    .expect("writing to a String cannot fail");
            } else {
                return Err(ApiError::NotFound);
            }
            output.push('\n');
        }
        Ok((
            [
                (header::CONTENT_TYPE, M3U8),
                (header::CACHE_CONTROL, "no-store"),
            ],
            output,
        )
            .into_response())
    } else {
        Ok((
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "public, max-age=30"),
            ],
            bytes,
        )
            .into_response())
    }
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn valid_segment(name: &str) -> bool {
    name.strip_prefix("segment")
        .and_then(|tail| tail.strip_suffix(".ts"))
        .is_some_and(|digits| {
            (6..=12).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_paths_reject_traversal() {
        assert!(valid_segment("segment000001.ts"));
        assert!(!valid_segment("../segment000001.ts"));
        assert!(!valid_segment("segment000001.ts/../secret"));
        assert!(valid_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(!valid_uuid("../../../../evil"));
    }
}
