//! `/api/v1/admin/{measures,dimensions,retention}` — the admin dashboard
//! metrics API (Mastodon's `Api::V1::Admin::{Measures,Dimensions,Retention}Controller`).
//!
//! Each endpoint authorizes `view_dashboard` + `admin:read` (Mastodon
//! `authorize :dashboard, :index?`), takes a `keys[]` list plus a `[start_at,
//! end_at]` window, dispatches each key to a query in [`plamenu_db::metrics`],
//! and serializes it. Unknown/unimplemented keys are silently skipped, matching
//! Mastodon's `filter_map`. Results are cached for 5 minutes (`AppState`'s
//! `metrics_cache`).
//!
//! Both form-encoded (`keys[]=…&instance_accounts[domain]=…`, the documented
//! shape) and JSON request bodies are accepted.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::id::id_at;
use plamenu_db::metrics;
use plamenu_db::role::permission;
use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime, Time, UtcOffset};

use crate::AppState;
use crate::auth::AdminUser;
use crate::entities::{dimension_item, dimension_json, human_size, locale_name, measure_json};
use crate::error::ApiError;

/// A dashboard renders a handful of measures/dimensions; cap the `keys[]` array
/// so one authenticated request can't fan out into an unbounded query batch.
const MAX_METRICS_KEYS: usize = 24;
/// Ceiling on a dimension's top-N `limit`. Mastodon's dashboards request small
/// lists; this rejects non-positive and pathologically large values that would
/// otherwise reach the SQL `LIMIT` verbatim.
const MAX_DIMENSION_LIMIT: i64 = 200;
/// Broad ceiling on a measure/dimension window so a single request can't scan
/// an arbitrarily long snowflake/time range (~3 years).
const MAX_METRICS_WINDOW: Duration = Duration::days(1096);
/// Retention builds an O(days²) cohort×retention cross product in SQL, so its
/// window gets a far tighter day-count bound than the generic window cap.
const MAX_RETENTION_DAYS: i64 = 366;

/// `POST /api/v1/admin/measures`.
pub async fn measures(
    State(state): State<AppState>,
    admin: AdminUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require(permission::VIEW_DASHBOARD, false)?;
    let request = MetricsRequest::parse(&headers, &body)?;
    let (start, end) = request.window()?;
    let keys = request.checked_keys()?;

    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let cache_key = request.cache_key("measure", key);
        if let Some(cached) = state.metrics_cache.get(&cache_key) {
            out.push(cached);
            continue;
        }
        let Some(value) = build_measure(&state, &request, key, start, end).await? else {
            continue;
        };
        state.metrics_cache.put(cache_key, value.clone());
        out.push(value);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/dimensions`.
pub async fn dimensions(
    State(state): State<AppState>,
    admin: AdminUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require(permission::VIEW_DASHBOARD, false)?;
    let request = MetricsRequest::parse(&headers, &body)?;
    let keys = request.checked_keys()?;
    let limit = request.checked_limit()?;

    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let cache_key = request.cache_key("dimension", key);
        if let Some(cached) = state.metrics_cache.get(&cache_key) {
            out.push(cached);
            continue;
        }
        let Some(value) = build_dimension(&state, &request, key, limit).await? else {
            continue;
        };
        state.metrics_cache.put(cache_key, value.clone());
        out.push(value);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/retention`.
pub async fn retention(
    State(state): State<AppState>,
    admin: AdminUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require(permission::VIEW_DASHBOARD, false)?;
    let request = MetricsRequest::parse(&headers, &body)?;
    let (start, end) = request.retention_dates()?;
    let frequency = request.frequency();

    let cache_key = format!(
        "metrics/retention;{};{};{frequency}",
        request
            .body
            .get("start_at")
            .and_then(Value::as_str)
            .unwrap_or(""),
        request
            .body
            .get("end_at")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if let Some(cached) = state.metrics_cache.get(&cache_key) {
        return Ok(Json(cached));
    }

    let cells = metrics::retention(&state.pool, start, end, &frequency).await?;
    let cohorts = Value::Array(crate::entities::cohorts_json(&cells, &frequency)?);
    state.metrics_cache.put(cache_key, cohorts.clone());
    Ok(Json(cohorts))
}

/// Builds one measure, or `None` if the key is unknown/unimplemented.
async fn build_measure(
    state: &AppState,
    request: &MetricsRequest,
    key: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Option<Value>, ApiError> {
    let pool = &state.pool;
    let value = match key {
        "active_users" => measure_json(
            key,
            None,
            &metrics::active_users(pool, start, end).await?,
            true,
        )?,
        "interactions" => measure_json(
            key,
            None,
            &metrics::interactions(pool, start, end).await?,
            true,
        )?,
        "new_users" => measure_json(
            key,
            None,
            &metrics::new_users(pool, start, end).await?,
            false,
        )?,
        "opened_reports" => measure_json(
            key,
            None,
            &metrics::opened_reports(pool, start, end).await?,
            false,
        )?,
        "resolved_reports" => measure_json(
            key,
            None,
            &metrics::resolved_reports(pool, start, end).await?,
            false,
        )?,
        "tag_servers" => {
            let tag_id = request.id_param(key)?;
            let (earliest, latest) = snowflake_range(start, end);
            let prev_window = snowflake_range_shifted(start, end);
            measure_json(
                key,
                None,
                &metrics::tag_servers_measure(
                    pool,
                    start,
                    end,
                    (earliest, latest),
                    prev_window,
                    tag_id,
                )
                .await?,
                false,
            )?
        }
        "tag_uses" => {
            let tag_id = request.id_param(key)?;
            measure_json(
                key,
                None,
                &metrics::tag_uses_measure(pool, start, end, tag_id).await?,
                true,
            )?
        }
        "tag_accounts" => {
            let tag_id = request.id_param(key)?;
            measure_json(
                key,
                None,
                &metrics::tag_accounts_measure(pool, start, end, tag_id).await?,
                true,
            )?
        }
        _ => return build_instance_measure(state, request, key, start, end).await,
    };
    Ok(Some(value))
}

/// The `instance_*` measure family — each scoped to a `{domain,
/// include_subdomains}` and carrying no `previous_total` (Mastodon
/// `total_in_time_range? == false`). Returns `None` for an unknown key.
async fn build_instance_measure(
    state: &AppState,
    request: &MetricsRequest,
    key: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Option<Value>, ApiError> {
    let pool = &state.pool;
    let value = match key {
        "instance_media_attachments" => {
            let (domain, subs) = request.domain_params(key)?;
            measure_json(
                key,
                Some("bytes"),
                &metrics::instance_media_attachments(pool, start, end, &domain, subs).await?,
                false,
            )?
        }
        "instance_accounts" => {
            let (domain, subs) = request.domain_params(key)?;
            measure_json(
                key,
                None,
                &metrics::instance_accounts(pool, start, end, &domain, subs).await?,
                false,
            )?
        }
        "instance_statuses" => {
            let (domain, subs) = request.domain_params(key)?;
            let (earliest, latest) = snowflake_range(start, end);
            measure_json(
                key,
                None,
                &metrics::instance_statuses(pool, start, end, earliest, latest, &domain, subs)
                    .await?,
                false,
            )?
        }
        "instance_follows" => {
            let (domain, subs) = request.domain_params(key)?;
            measure_json(
                key,
                None,
                &metrics::instance_follows(pool, start, end, &domain, subs).await?,
                false,
            )?
        }
        "instance_followers" => {
            let (domain, subs) = request.domain_params(key)?;
            measure_json(
                key,
                None,
                &metrics::instance_followers(pool, start, end, &domain, subs).await?,
                false,
            )?
        }
        "instance_reports" => {
            let (domain, subs) = request.domain_params(key)?;
            measure_json(
                key,
                None,
                &metrics::instance_reports(pool, start, end, &domain, subs).await?,
                false,
            )?
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Builds one dimension, or `None` if the key is unknown/unimplemented.
async fn build_dimension(
    state: &AppState,
    request: &MetricsRequest,
    key: &str,
    limit: Option<i64>,
) -> Result<Option<Value>, ApiError> {
    let pool = &state.pool;
    let local_domain = state.config.domain.clone();
    let value = match key {
        "servers" => {
            let (start, end) = request.window()?;
            let (earliest, latest) = snowflake_range(start, end);
            let rows = metrics::dim_servers(pool, earliest, latest, limit).await?;
            dimension_json(key, domain_rows(rows, &local_domain))
        }
        "instance_accounts" => {
            let (domain, _) = request.domain_params(key)?;
            let rows = metrics::dim_instance_accounts(pool, &domain, limit).await?;
            let data = rows
                .into_iter()
                .map(|r| {
                    let username = r.key.unwrap_or_default();
                    dimension_item(&username, &username, r.value)
                })
                .collect();
            dimension_json(key, data)
        }
        "instance_languages" => {
            let (domain, _) = request.domain_params(key)?;
            let (start, end) = request.window()?;
            let (earliest, latest) = snowflake_range(start, end);
            let rows =
                metrics::dim_instance_languages(pool, &domain, earliest, latest, limit).await?;
            dimension_json(key, language_rows(rows))
        }
        "tag_servers" => {
            let tag_id = request.id_param(key)?;
            let (start, end) = request.window()?;
            let (earliest, latest) = snowflake_range(start, end);
            let rows = metrics::dim_tag_servers(pool, tag_id, earliest, latest, limit).await?;
            dimension_json(key, domain_rows(rows, &local_domain))
        }
        "tag_languages" => {
            let tag_id = request.id_param(key)?;
            let (start, end) = request.window()?;
            let (earliest, latest) = snowflake_range(start, end);
            let rows = metrics::dim_tag_languages(pool, tag_id, earliest, latest, limit).await?;
            dimension_json(key, language_rows(rows))
        }
        "languages" => {
            let (start, end) = request.window()?;
            let rows = metrics::dim_languages(pool, start, end, limit).await?;
            dimension_json(key, language_rows(rows))
        }
        "sources" => {
            let (start, end) = request.window()?;
            let rows = metrics::dim_sources(pool, start, end, limit).await?;
            let data = rows
                .into_iter()
                .map(|r| match r.key {
                    Some(name) => dimension_item(&name, &name, r.value),
                    // A NULL app name (no signup attribution) is the local web app.
                    None => dimension_item("web", "Website", r.value),
                })
                .collect();
            dimension_json(key, data)
        }
        "space_usage" => dimension_json(key, space_usage(state).await?),
        "software_versions" => dimension_json(key, software_versions(state).await?),
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// `space_usage` dimension data — the `postgresql` line (DB size) and the
/// `media` line (stored attachment/avatar/header/emoji/card bytes).
async fn space_usage(state: &AppState) -> Result<Vec<Value>, ApiError> {
    let pg = metrics::pg_database_size(&state.pool).await?;
    let media = metrics::media_storage_bytes(&state.pool).await?;
    Ok(vec![
        serde_json::json!({
            "key": "postgresql",
            "human_key": "PostgreSQL",
            "value": pg.to_string(),
            "unit": "bytes",
            "human_value": human_size(pg),
        }),
        serde_json::json!({
            "key": "media",
            "human_key": "Media storage",
            "value": media.to_string(),
            "unit": "bytes",
            "human_value": human_size(media),
        }),
    ])
}

/// `software_versions` dimension data. Mastodon's redis/elasticsearch/libvips
/// lines are N/A (omitted, like Mastodon `.compact`s nils); the `mastodon` line
/// becomes `plamenu`.
async fn software_versions(state: &AppState) -> Result<Vec<Value>, ApiError> {
    let plamenu_version = crate::FULL_VERSION;
    let mut versions = vec![serde_json::json!({
        "key": "plamenu",
        "human_key": "Plamenu",
        "value": plamenu_version,
        "human_value": plamenu_version,
    })];
    if let Ok(pg) = metrics::pg_version(&state.pool).await {
        versions.push(serde_json::json!({
            "key": "postgresql",
            "human_key": "PostgreSQL",
            "value": pg,
            "human_value": pg,
        }));
    }
    if let Some(ffmpeg) = ffmpeg_version(state).await {
        versions.push(serde_json::json!({
            "key": "ffmpeg",
            "human_key": "FFmpeg",
            "value": ffmpeg,
            "human_value": ffmpeg,
        }));
    }
    Ok(versions)
}

/// FFmpeg/ffprobe version via `ffprobe -show_program_version` (Mastodon's exact
/// probe), or `None` if the binary is unavailable. Also feeds the web
/// dashboard's software-versions card.
pub(crate) async fn ffmpeg_version(state: &AppState) -> Option<String> {
    // Bounded + reaped on drop: the dashboard must not hang on a wedged binary.
    let run = tokio::process::Command::new(&state.config.ffprobe_path)
        .args(["-show_program_version", "-v", "0", "-of", "json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    parsed
        .get("program_version")?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

/// Maps domain dimension rows; a `NULL` domain renders as the local domain.
fn domain_rows(rows: Vec<metrics::DimRow>, local_domain: &str) -> Vec<Value> {
    rows.into_iter()
        .map(|r| {
            let domain = r.key.unwrap_or_else(|| local_domain.to_owned());
            dimension_item(&domain, &domain, r.value)
        })
        .collect()
}

/// Maps language dimension rows; `human_key` is the standard locale name.
fn language_rows(rows: Vec<metrics::DimRow>) -> Vec<Value> {
    rows.into_iter()
        .map(|r| {
            let code = r.key.unwrap_or_else(|| "und".to_owned());
            dimension_item(&code, &locale_name(&code), r.value)
        })
        .collect()
}

/// The `[start, end]` snowflake-id range: the smallest id at the window's first
/// midnight through the largest id at the last day's end (sequence bits set).
/// Mirrors `BaseMeasure#earliest_status_id`/`latest_status_id`. Also feeds the
/// web dashboard's top-servers dimension.
pub(crate) fn snowflake_range(start: OffsetDateTime, end: OffsetDateTime) -> (i64, i64) {
    let begin_ms = (start.to_offset(UtcOffset::UTC).replace_time(Time::MIDNIGHT))
        .unix_timestamp_nanos()
        / 1_000_000;
    let end_ms = (end
        .to_offset(UtcOffset::UTC)
        .replace_time(Time::from_hms_milli(23, 59, 59, 999).unwrap_or(Time::MIDNIGHT)))
    .unix_timestamp_nanos()
        / 1_000_000;
    #[allow(clippy::cast_possible_truncation)]
    (id_at(begin_ms as i64), id_at(end_ms as i64) | 0xFFFF)
}

/// The previous-window snowflake range (shifted back by the window length), for
/// `tag_servers`' `previous_total`.
fn snowflake_range_shifted(start: OffsetDateTime, end: OffsetDateTime) -> (i64, i64) {
    let len = end - start;
    snowflake_range(start - len, end - len)
}

/// A parsed metrics request: the `keys`, window, `limit`, and per-key params.
struct MetricsRequest {
    body: Value,
}

impl MetricsRequest {
    /// Parses the request body — JSON, or the documented form-encoded shape.
    fn parse(headers: &HeaderMap, body: &[u8]) -> Result<Self, ApiError> {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let value = if content_type.starts_with("application/json") {
            serde_json::from_slice(body)
                .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?
        } else {
            parse_form(body)?
        };
        Ok(Self { body: value })
    }

    /// The requested `keys[]` (empty when absent).
    fn keys(&self) -> Vec<&str> {
        self.body
            .get("keys")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// The requested `keys[]`, rejecting an over-long batch.
    fn checked_keys(&self) -> Result<Vec<&str>, ApiError> {
        let keys = self.keys();
        if keys.len() > MAX_METRICS_KEYS {
            return Err(ApiError::Unprocessable(format!(
                "too many metrics keys: max {MAX_METRICS_KEYS}"
            )));
        }
        Ok(keys)
    }

    /// The `[start_at, end_at]` window. Required for measures and the
    /// time-bounded dimensions. Rejects a reversed range or one wider than
    /// [`MAX_METRICS_WINDOW`].
    fn window(&self) -> Result<(OffsetDateTime, OffsetDateTime), ApiError> {
        let start = self.datetime("start_at")?;
        let end = self.datetime("end_at")?;
        if end < start {
            return Err(ApiError::Unprocessable(
                "end_at must not precede start_at".to_owned(),
            ));
        }
        if end - start > MAX_METRICS_WINDOW {
            return Err(ApiError::Unprocessable(format!(
                "metrics window too large: max {} days",
                MAX_METRICS_WINDOW.whole_days()
            )));
        }
        Ok((start, end))
    }

    /// The retention `[start, end]` as UTC dates, with the tighter
    /// [`MAX_RETENTION_DAYS`] day-count bound over the generic window
    /// (the retention query is O(days²)).
    fn retention_dates(&self) -> Result<(Date, Date), ApiError> {
        let (start, end) = self.window()?;
        let start = start.to_offset(UtcOffset::UTC).date();
        let end = end.to_offset(UtcOffset::UTC).date();
        if (end - start).whole_days() > MAX_RETENTION_DAYS {
            return Err(ApiError::Unprocessable(format!(
                "retention window too large: max {MAX_RETENTION_DAYS} days"
            )));
        }
        Ok((start, end))
    }

    /// The retention `frequency` — `day` or `month` (anything else → `day`),
    /// matching Mastodon's whitelist.
    fn frequency(&self) -> String {
        match self.body.get("frequency").and_then(Value::as_str) {
            Some("month") => "month".to_owned(),
            _ => "day".to_owned(),
        }
    }

    /// The optional dimension `limit` (Mastodon: absent → no `LIMIT`).
    fn limit(&self) -> Option<i64> {
        match self.body.get("limit") {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        }
    }

    /// The optional dimension `limit`, rejecting non-positive or oversized
    /// values before they reach the SQL `LIMIT`.
    fn checked_limit(&self) -> Result<Option<i64>, ApiError> {
        match self.limit() {
            None => Ok(None),
            Some(n) if n <= 0 => Err(ApiError::Unprocessable(
                "limit must be a positive integer".to_owned(),
            )),
            Some(n) if n > MAX_DIMENSION_LIMIT => Err(ApiError::Unprocessable(format!(
                "limit too large: max {MAX_DIMENSION_LIMIT}"
            ))),
            Some(n) => Ok(Some(n)),
        }
    }

    /// A required `[key][domain]` + `[key][include_subdomains]` param pair.
    fn domain_params(&self, key: &str) -> Result<(String, bool), ApiError> {
        let params = self
            .body
            .get(key)
            .ok_or_else(|| ApiError::BadRequest(format!("missing params for {key}")))?;
        let domain = params
            .get("domain")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest(format!("missing {key}[domain]")))?
            .to_owned();
        let include_subdomains = match params.get("include_subdomains") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "t"),
            Some(Value::Number(n)) => n.as_i64() == Some(1),
            _ => false,
        };
        Ok((domain, include_subdomains))
    }

    /// A required `[key][id]` tag-id param (accepts string or number).
    fn id_param(&self, key: &str) -> Result<i64, ApiError> {
        let params = self
            .body
            .get(key)
            .ok_or_else(|| ApiError::BadRequest(format!("missing params for {key}")))?;
        match params.get("id") {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        }
        .ok_or_else(|| ApiError::BadRequest(format!("missing or invalid {key}[id]")))
    }

    fn datetime(&self, field: &str) -> Result<OffsetDateTime, ApiError> {
        let raw = self
            .body
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest(format!("missing {field}")))?;
        parse_datetime(raw).ok_or_else(|| ApiError::BadRequest(format!("invalid {field}: {raw}")))
    }

    /// The Mastodon-style cache key for one measure/dimension item.
    fn cache_key(&self, kind: &str, key: &str) -> String {
        let start = self
            .body
            .get("start_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let end = self
            .body
            .get("end_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let limit = self
            .body
            .get("limit")
            .map_or_else(String::new, ToString::to_string);
        let params = self
            .body
            .get(key)
            .map_or_else(String::new, ToString::to_string);
        format!("metrics/{kind}/{key};{start};{end};{limit};{params}")
    }
}

/// Parses a `start_at`/`end_at` value: a full RFC3339 timestamp, or a bare
/// `YYYY-MM-DD` date (assumed midnight UTC).
fn parse_datetime(raw: &str) -> Option<OffsetDateTime> {
    if let Ok(dt) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Some(dt);
    }
    let date = Date::parse(
        raw,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()?;
    Some(date.with_time(Time::MIDNIGHT).assume_utc())
}

/// Builds a JSON object from the documented form-encoded body: repeated
/// `keys[]`, scalar fields, and one-level `key[param]` nesting.
fn parse_form(body: &[u8]) -> Result<Value, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    let mut root = Map::new();
    for (key, value) in pairs {
        if let Some(name) = key.strip_suffix("[]") {
            let entry = root
                .entry(name.to_owned())
                .or_insert_with(|| Value::Array(vec![]));
            if let Value::Array(arr) = entry {
                arr.push(Value::String(value));
            }
        } else if let Some((outer, rest)) = key.split_once('[') {
            let inner = rest.strip_suffix(']').unwrap_or(rest);
            let entry = root
                .entry(outer.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(obj) = entry {
                obj.insert(inner.to_owned(), Value::String(value));
            }
        } else {
            root.insert(key, Value::String(value));
        }
    }
    Ok(Value::Object(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `MetricsRequest` from a JSON body, as the API does.
    fn req(json: &str) -> MetricsRequest {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        MetricsRequest::parse(&headers, json.as_bytes()).expect("valid JSON body")
    }

    #[test]
    fn window_rejects_reversed_range() {
        let r = req(r#"{"start_at":"2025-01-02T00:00:00Z","end_at":"2025-01-01T00:00:00Z"}"#);
        assert!(matches!(r.window(), Err(ApiError::Unprocessable(_))));
    }

    #[test]
    fn window_rejects_century_span() {
        let r = req(r#"{"start_at":"1900-01-01T00:00:00Z","end_at":"2025-01-01T00:00:00Z"}"#);
        assert!(matches!(r.window(), Err(ApiError::Unprocessable(_))));
    }

    #[test]
    fn window_accepts_reasonable_range() {
        let r = req(r#"{"start_at":"2025-01-01T00:00:00Z","end_at":"2025-01-31T00:00:00Z"}"#);
        assert!(r.window().is_ok());
    }

    #[test]
    fn retention_dates_tighter_than_generic_window() {
        // A two-year span is fine for measures but over the retention day cap.
        let two_years = r#"{"start_at":"2023-01-01T00:00:00Z","end_at":"2025-01-01T00:00:00Z"}"#;
        assert!(req(two_years).window().is_ok());
        assert!(matches!(
            req(two_years).retention_dates(),
            Err(ApiError::Unprocessable(_))
        ));
        let ok = req(r#"{"start_at":"2024-01-01T00:00:00Z","end_at":"2024-06-01T00:00:00Z"}"#);
        assert!(ok.retention_dates().is_ok());
    }

    #[test]
    fn checked_keys_caps_the_batch() {
        let many: Vec<String> = (0..=MAX_METRICS_KEYS)
            .map(|i| format!("\"k{i}\""))
            .collect();
        let body = format!(r#"{{"keys":[{}]}}"#, many.join(","));
        assert!(matches!(
            req(&body).checked_keys(),
            Err(ApiError::Unprocessable(_))
        ));
        // At the cap is accepted.
        let at_cap: Vec<String> = (0..MAX_METRICS_KEYS).map(|i| format!("\"k{i}\"")).collect();
        let body = format!(r#"{{"keys":[{}]}}"#, at_cap.join(","));
        assert_eq!(req(&body).checked_keys().unwrap().len(), MAX_METRICS_KEYS);
    }

    #[test]
    fn checked_limit_bounds_the_dimension_limit() {
        assert!(matches!(
            req(r#"{"limit":0}"#).checked_limit(),
            Err(ApiError::Unprocessable(_))
        ));
        assert!(matches!(
            req(r#"{"limit":-5}"#).checked_limit(),
            Err(ApiError::Unprocessable(_))
        ));
        let over = format!(r#"{{"limit":{}}}"#, MAX_DIMENSION_LIMIT + 1);
        assert!(matches!(
            req(&over).checked_limit(),
            Err(ApiError::Unprocessable(_))
        ));
        assert_eq!(req(r#"{"limit":50}"#).checked_limit().unwrap(), Some(50));
        assert_eq!(req("{}").checked_limit().unwrap(), None);
    }
}
