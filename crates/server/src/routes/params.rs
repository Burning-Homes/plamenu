//! Request-body parsing for the client API: Mastodon clients send both
//! `application/json` and form-urlencoded bodies interchangeably.

use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// Mastodon's boolean query-param cast (`ActiveModel::Type::Boolean`):
/// any present value is true except the recognized false spellings.
#[must_use]
pub fn truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| !matches!(v, "" | "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"))
}

/// Recovers a Rails-style repeated `field[]` form parameter (also accepting the
/// bare `field` key) into a `Vec` — `serde_urlencoded` cannot collect repeated
/// keys, and Mastodon clients send arrays this way. Empty for JSON bodies (use
/// the deserialized field there).
#[must_use]
pub fn repeated_form_field(headers: &HeaderMap, body: &[u8], field: &str) -> Vec<String> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return Vec::new();
    }
    let bracketed = format!("{field}[]");
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(body)
        .map(|pairs| {
            pairs
                .into_iter()
                .filter(|(key, _)| key == &bracketed || key == field)
                .map(|(_, value)| value)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a form body carries `field` (or `field[]`) at all — for params
/// where present-but-empty means "clear" while absent means "keep". Always
/// false for JSON bodies (test the deserialized `Option` there).
#[must_use]
pub fn form_has_field(headers: &HeaderMap, body: &[u8], field: &str) -> bool {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return false;
    }
    let bracketed = format!("{field}[]");
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(body).is_ok_and(|pairs| {
        pairs
            .iter()
            .any(|(key, _)| key == &bracketed || key == field)
    })
}

pub fn parse_body<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))
    } else {
        serde_urlencoded::from_bytes(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))
    }
}

/// Deduplicates a client-supplied id array (keeping first-seen order) and
/// rejects with 422 once the distinct count would exceed `max`. Several
/// authenticated bulk inputs — report statuses/rules, list membership,
/// notification-request batches — feed serial per-id database work, so an
/// uncapped or duplicate-stuffed array from the ordinary ~2 MiB body would
/// otherwise amplify one request into unbounded queries, rows, and outbound
/// activity. Duplicates are dropped rather than counted, so a
/// self-repeating array collapses to one entry — matching the Mastodon
/// services that resolve these ids through a set.
pub(crate) fn bounded_unique_ids(ids: Vec<i64>, max: usize) -> Result<Vec<i64>, ApiError> {
    let mut unique = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if seen.insert(id) {
            unique.push(id);
            if unique.len() > max {
                return Err(ApiError::Unprocessable("Validation failed".to_owned()));
            }
        }
    }
    Ok(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_unique_ids_dedups_keeping_first_seen_order() {
        assert_eq!(
            bounded_unique_ids(vec![3, 1, 3, 2, 1], 10).unwrap(),
            vec![3, 1, 2],
        );
    }

    #[test]
    fn bounded_unique_ids_allows_exactly_the_cap() {
        assert_eq!(bounded_unique_ids(vec![1, 2, 3], 3).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn bounded_unique_ids_rejects_over_the_distinct_cap() {
        let err = bounded_unique_ids(vec![1, 2, 3, 4], 3).unwrap_err();
        assert!(matches!(err, ApiError::Unprocessable(_)), "{err:?}");
    }

    #[test]
    fn bounded_unique_ids_counts_distinct_not_raw_entries() {
        // A duplicate-stuffed array within the distinct cap is accepted and
        // collapsed to one id rather than rejected on raw length.
        assert_eq!(bounded_unique_ids(vec![7; 10_000], 5).unwrap(), vec![7]);
    }
}
