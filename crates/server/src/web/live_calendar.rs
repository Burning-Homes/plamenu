//! Public, subscribable iCalendar views of locally authored live schedules.
//! The stored schedule is the only source of truth; calendar software, not a
//! new Plamenu worker, delivers the optional reminder.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use plamenu_db::native_live::{self, CalendarOccurrence};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, UtcOffset};

use crate::{AppState, error::ApiError};

#[derive(Default, Deserialize)]
pub struct CalendarQuery {
    reminder: Option<u16>,
}

impl CalendarQuery {
    fn reminder(&self) -> Result<bool, ApiError> {
        match self.reminder {
            None | Some(0) => Ok(false),
            Some(15) => Ok(true),
            _ => Err(ApiError::BadRequest(
                "Choose no reminder or a 15-minute reminder".into(),
            )),
        }
    }
}

pub async fn broadcast(
    State(state): State<AppState>,
    Path(status_id): Path<i64>,
    Query(query): Query<CalendarQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let reminder = query.reminder()?;
    let broadcast = native_live::calendar_broadcast(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let now = OffsetDateTime::now_utc();
    let events = broadcast
        .scheduled_starts
        .iter()
        .copied()
        .filter(|at| *at > now)
        .map(|start_at| CalendarOccurrence {
            status_id: broadcast.status_id,
            broadcast_uuid: broadcast.broadcast_uuid.clone(),
            title: broadcast.title.clone(),
            start_at,
            updated_at: broadcast.updated_at,
        });
    let url = format!(
        "https://{}/videos/live/{status_id}/calendar.ics",
        state.config.domain
    );
    let source = if reminder {
        format!("{url}?reminder=15")
    } else {
        url
    };
    let name = format!("{} — live starts", broadcast.title);
    let body = calendar(&state.config.domain, &name, &source, events, reminder);
    Ok(calendar_response(body, &headers, false))
}

pub async fn channel(
    State(state): State<AppState>,
    Path(channel_id): Path<i64>,
    Query(query): Query<CalendarQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let reminder = query.reminder()?;
    let name = native_live::calendar_channel_name(&state.pool, channel_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let events = native_live::channel_calendar_broadcasts(&state.pool, channel_id).await?;
    let url = format!(
        "https://{}/videos/channels/{channel_id}/calendar.ics",
        state.config.domain
    );
    let source = if reminder {
        format!("{url}?reminder=15")
    } else {
        url
    };
    let name = format!("{name} — public live starts");
    let body = calendar(&state.config.domain, &name, &source, events, reminder);
    Ok(calendar_response(body, &headers, true))
}

fn calendar(
    domain: &str,
    name: &str,
    source: &str,
    events: impl IntoIterator<Item = CalendarOccurrence>,
    reminder: bool,
) -> String {
    let mut body = String::with_capacity(4096);
    for line in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Plamenu//Live Schedule//EN",
        "CALSCALE:GREGORIAN",
        "METHOD:PUBLISH",
    ] {
        push_line(&mut body, line);
    }
    push_line(&mut body, &format!("NAME:{}", escape_text(name)));
    push_line(&mut body, &format!("X-WR-CALNAME:{}", escape_text(name)));
    push_line(&mut body, &format!("SOURCE;VALUE=URI:{source}"));
    for event in events {
        push_line(&mut body, "BEGIN:VEVENT");
        push_line(
            &mut body,
            &format!(
                "UID:{}-{}@{domain}",
                event.broadcast_uuid,
                event.start_at.unix_timestamp()
            ),
        );
        push_line(
            &mut body,
            &format!("DTSTAMP:{}", utc_time(event.updated_at)),
        );
        push_line(
            &mut body,
            &format!("LAST-MODIFIED:{}", utc_time(event.updated_at)),
        );
        push_line(&mut body, &format!("DTSTART:{}", utc_time(event.start_at)));
        push_line(&mut body, &format!("SUMMARY:{}", escape_text(&event.title)));
        push_line(&mut body, "STATUS:TENTATIVE");
        push_line(&mut body, "TRANSP:TRANSPARENT");
        push_line(
            &mut body,
            &format!("URL:https://{domain}/web/statuses/{}", event.status_id),
        );
        push_line(
            &mut body,
            "DESCRIPTION:Planned start time. The actual start and duration may change.",
        );
        if reminder {
            push_line(&mut body, "BEGIN:VALARM");
            push_line(&mut body, "ACTION:DISPLAY");
            push_line(&mut body, "TRIGGER:-PT15M");
            push_line(&mut body, "DESCRIPTION:Live broadcast starts soon");
            push_line(&mut body, "END:VALARM");
        }
        push_line(&mut body, "END:VEVENT");
    }
    push_line(&mut body, "END:VCALENDAR");
    body
}

fn utc_time(value: OffsetDateTime) -> String {
    value
        .to_offset(UtcOffset::UTC)
        .format(time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .expect("a fixed UTC timestamp format cannot fail")
}

/// RFC 5545 TEXT escaping, including a line break that must not become a
/// second property in a malicious title or channel name.
fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            ';' => escaped.push_str("\\;"),
            ',' => escaped.push_str("\\,"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                escaped.push_str("\\n");
            }
            '\n' => escaped.push_str("\\n"),
            ch if ch.is_control() => {}
            ch => escaped.push(ch),
        }
    }
    escaped
}

/// Content lines are at most 75 *octets*. Fold only at UTF-8 character
/// boundaries, and count the continuation space against the next line.
fn push_line(out: &mut String, line: &str) {
    let mut bytes = 0;
    for ch in line.chars() {
        let width = ch.len_utf8();
        if bytes + width > 75 {
            out.push_str("\r\n ");
            bytes = 1;
        }
        out.push(ch);
        bytes += width;
    }
    out.push_str("\r\n");
}

fn calendar_response(body: String, request: &HeaderMap, public: bool) -> Response {
    let etag = format!(
        "\"{}\"",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(body.as_bytes()))
    );
    let cache = if public {
        "public, max-age=300"
    } else {
        "private, max-age=300"
    };
    let unchanged = request
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == etag);
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        body.into_response()
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline; filename=\"live-calendar.ics\""),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("base64url ETag uses only safe ASCII"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::{escape_text, push_line};

    #[test]
    fn calendar_text_cannot_inject_properties() {
        assert_eq!(
            escape_text("hello, world;\\\r\nBEGIN:VEVENT"),
            "hello\\, world\\;\\\\\\nBEGIN:VEVENT"
        );
    }

    #[test]
    fn folded_utf8_lines_obey_octet_limit() {
        let mut out = String::new();
        push_line(&mut out, &format!("SUMMARY:{}", "é".repeat(80)));
        assert!(out.ends_with("\r\n"));
        assert!(out.split("\r\n").all(|line| line.len() <= 75));
        assert!(out.contains("\r\n "));
    }
}
