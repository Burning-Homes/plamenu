//! `/api/v1/statuses` — posting, viewing, context and interactions.

use std::collections::HashSet;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use plamenu_db::{status, user};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::parse_body;
use crate::actions::{self, EditParams, PostParams};
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{
    PreviewStatusOpts, apply_redraft_source, can_view, filter_viewable, preview_status_json,
    render_status, render_status_history, render_statuses,
};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateParams {
    pub status: Option<String>,
    pub visibility: Option<String>,
    /// Clients send this as a string id; accept numbers too.
    pub in_reply_to_id: Option<Value>,
    #[serde(default)]
    pub media_ids: Vec<Value>,
    /// FEP-044f quote target (Mastodon 4.5 parameter).
    pub quoted_status_id: Option<Value>,
    /// Content warning.
    pub spoiler_text: Option<String>,
    /// JSON clients send a boolean, form clients a string.
    pub sensitive: Option<Value>,
    pub language: Option<String>,
    /// Poll attachment (JSON clients; form clients use `poll[...]` keys,
    /// recovered separately).
    pub poll: Option<PollBody>,
    /// RFC 3339 time to publish at. More than 5 minutes ahead queues the post
    /// (returns a `ScheduledStatus`); at or before now it is ignored.
    pub scheduled_at: Option<String>,
    /// Who may quote: `public` | `followers` | `nobody` (Mastodon 4.6).
    /// Absent falls back to the user's default quote policy preference.
    pub quote_approval_policy: Option<String>,
    /// Pleroma's rich-text format (P4): `text/plain` | `text/markdown` |
    /// `text/html`. Absent falls back to the user's default format;
    /// unsupported values mean plain, like Pleroma's `get_content_type`.
    pub content_type: Option<String>,
    /// Plamenu extension: local group to submit this post to.
    /// Top-level public posts only; replies inherit their parent's group.
    pub group_id: Option<Value>,
    /// Plamenu extension: thread title — the post federates as a
    /// Lemmy-style `Page`. Group submissions only.
    pub title: Option<String>,
    /// Plamenu extension: a titled link post's target URL.
    pub external_url: Option<String>,
    /// Plamenu extension (E4): publish this as an `Event`.
    pub event: Option<EventBody>,
    /// Plamenu extension: the post kind. `note` (the default) | `article`.
    /// `article` publishes long-form — the full body federates as an `Article`
    /// and a title is required. Absent keeps the historical behaviour, where the
    /// kind follows from a poll, a group submission or event fields being
    /// present. The event kind is stated by `event` rather than here, so every
    /// client written against E4 keeps working unchanged.
    pub post_kind: Option<String>,
}

/// Maps the `post_kind` parameter onto the author-selectable kinds. An
/// unrecognised value is refused rather than silently treated as a `Note`: a
/// client that misspells the kind would otherwise publish a plain post and never
/// learn why its title vanished.
fn post_kind_param(raw: Option<&str>) -> Result<plamenu_ap::activity::PostKind, ApiError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("note") => Ok(plamenu_ap::activity::PostKind::Note),
        Some("article" | "long_form") => Ok(plamenu_ap::activity::PostKind::Article),
        Some(other) => Err(ApiError::Unprocessable(format!(
            "Validation failed: Unknown post kind {other}"
        ))),
    }
}

/// Event fields on a create/edit (E4). Present only when the client means it:
/// there is no inference from "a date turned up in the body", because an `Event`
/// is truncated to a title-plus-link stub by Mastodon and its forks.
#[derive(Deserialize, Default)]
pub struct EventBody {
    /// RFC 3339 start time — required.
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    /// The venue's IANA zone (a display hint; times still render on each
    /// viewer's own clock).
    pub timezone: Option<String>,
    /// `free` | `restricted` | `invite` | `external`.
    pub join_mode: Option<String>,
    pub external_participation_url: Option<String>,
    /// Clients send numbers or strings.
    pub max_attendees: Option<Value>,
    /// `CONFIRMED` | `TENTATIVE` | `CANCELLED`.
    pub status: Option<String>,
    pub is_online: Option<Value>,
    pub location: Option<String>,
    pub location_street: Option<String>,
    pub location_locality: Option<String>,
    pub location_region: Option<String>,
    pub location_country: Option<String>,
    pub location_postal_code: Option<String>,
}

impl EventBody {
    /// Maps the wire shape onto [`actions::EventPatch`] for an edit: every absent
    /// field keeps what is stored. A client that PUTs only `status: "CANCELLED"`
    /// must not thereby erase the venue and the end time.
    fn into_patch(self) -> actions::EventPatch {
        let some = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
        actions::EventPatch {
            max_attendees: match &self.max_attendees {
                Some(Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()),
                Some(Value::String(s)) => s.trim().parse().ok(),
                _ => None,
            },
            is_online: self.is_online.as_ref().map(|v| bool_param(Some(v))),
            start_time: some(self.start_time),
            end_time: some(self.end_time),
            timezone: some(self.timezone),
            join_mode: some(self.join_mode),
            external_participation_url: some(self.external_participation_url),
            status: some(self.status),
            location_name: some(self.location),
            location_street: some(self.location_street),
            location_locality: some(self.location_locality),
            location_region: some(self.location_region),
            location_country: some(self.location_country),
            location_postal_code: some(self.location_postal_code),
        }
    }

    /// Maps the wire shape onto [`actions::EventParams`] for a create.
    ///
    /// Derived from `into_patch` over an empty sidecar rather than spelled out
    /// again: `EventPatch::apply` already owns the defaults (a `free`,
    /// `CONFIRMED`, offline event), so a new event field needs adding in one
    /// place, not three.
    fn into_params(self) -> actions::EventParams {
        self.into_patch()
            .apply(&plamenu_db::status_event::StatusEvent::empty(0))
    }
}

#[derive(Deserialize)]
pub struct PollBody {
    #[serde(default)]
    pub options: Vec<String>,
    /// Seconds; clients send strings or numbers.
    pub expires_in: Option<Value>,
    pub multiple: Option<Value>,
    pub hide_totals: Option<Value>,
}

/// Recovers Rails-style nested `poll[...]` keys from a form body, which
/// `serde_urlencoded` cannot map onto a nested struct.
fn poll_from_form(headers: &HeaderMap, body: &[u8]) -> Option<PollBody> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return None;
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).ok()?;
    let mut poll = PollBody {
        options: Vec::new(),
        expires_in: None,
        multiple: None,
        hide_totals: None,
    };
    let mut seen = false;
    for (key, value) in pairs {
        match key.as_str() {
            "poll[options][]" => poll.options.push(value),
            "poll[expires_in]" => poll.expires_in = Some(Value::String(value)),
            "poll[multiple]" => poll.multiple = Some(Value::String(value)),
            "poll[hide_totals]" => poll.hide_totals = Some(Value::String(value)),
            _ => continue,
        }
        seen = true;
    }
    seen.then_some(poll)
}

/// Recovers Rails-style nested `event[...]` keys from a form body, which
/// `serde_urlencoded` cannot map onto a nested struct — the same treatment
/// `poll_from_form` gives polls.
///
/// Returns `None` unless at least one `event[...]` key is present, so a form that
/// mentions no event never produces one: the post kind must be chosen, never
/// inferred (an `Event` is truncated to a stub by Mastodon and its forks).
fn event_from_form(headers: &HeaderMap, body: &[u8]) -> Option<EventBody> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return None;
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).ok()?;
    let mut event = EventBody::default();
    let mut seen = false;
    for (key, value) in pairs {
        let Some(field) = key.strip_prefix("event[").and_then(|k| k.strip_suffix(']')) else {
            continue;
        };
        match field {
            "start_time" => event.start_time = Some(value),
            "end_time" => event.end_time = Some(value),
            "timezone" => event.timezone = Some(value),
            "join_mode" => event.join_mode = Some(value),
            "external_participation_url" => event.external_participation_url = Some(value),
            "max_attendees" => event.max_attendees = Some(Value::String(value)),
            "status" => event.status = Some(value),
            "is_online" => event.is_online = Some(Value::String(value)),
            "location" => event.location = Some(value),
            "location_street" => event.location_street = Some(value),
            "location_locality" => event.location_locality = Some(value),
            "location_region" => event.location_region = Some(value),
            "location_country" => event.location_country = Some(value),
            "location_postal_code" => event.location_postal_code = Some(value),
            _ => continue,
        }
        seen = true;
    }
    seen.then_some(event)
}

/// Recovers Rails-style repeated `media_ids[]` keys from a form body —
/// `serde_urlencoded` cannot collect repeated keys into a Vec.
fn media_ids_from_form(headers: &HeaderMap, body: &[u8]) -> Option<Vec<Value>> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return None;
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).ok()?;
    let ids: Vec<Value> = pairs
        .into_iter()
        .filter(|(key, _)| key == "media_ids[]")
        .map(|(_, value)| Value::String(value))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// Maps the wire-shape poll body onto [`actions::PollParams`].
fn poll_params(body: PollBody) -> Result<actions::PollParams, ApiError> {
    let expires_in = match &body.expires_in {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) if !s.is_empty() => Some(s.parse::<i64>().map_err(|_| {
            ApiError::Unprocessable("Validation failed: Expires at can't be blank".into())
        })?),
        _ => None,
    };
    Ok(actions::PollParams {
        options: body.options,
        expires_in,
        multiple: bool_param(body.multiple.as_ref()),
        hide_totals: bool_param(body.hide_totals.as_ref()),
    })
}

fn id_param(value: &Value) -> Result<i64, ApiError> {
    match value {
        Value::String(s) => s.parse().map_err(|_| ApiError::NotFound),
        Value::Number(n) => n.as_i64().ok_or(ApiError::NotFound),
        _ => Err(ApiError::NotFound),
    }
}

fn bool_param(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => super::params::truthy(Some(s)),
        _ => false,
    }
}

/// Maps a `quote_approval_policy` string onto the stored bitmap, rejecting an
/// unrecognized value like Mastodon's `Api::InteractionPoliciesConcern`.
fn resolve_quote_policy(value: &str) -> Result<i32, ApiError> {
    plamenu_ap::quote_policy::from_client_string(value)
        .ok_or_else(|| ApiError::Unprocessable("Validation failed: Quote policy is invalid".into()))
}

/// `POST /api/v1/statuses`.
#[allow(clippy::too_many_lines)]
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let mut params: CreateParams = parse_body(&headers, &body)?;
    if params.media_ids.is_empty()
        && let Some(ids) = media_ids_from_form(&headers, &body)
    {
        params.media_ids = ids;
    }
    // Media-only posts have no text; post_status validates the combination.
    let text = params.status.as_deref().unwrap_or("");
    let in_reply_to_id = params.in_reply_to_id.as_ref().map(id_param).transpose()?;
    let quoted_status_id = params.quoted_status_id.as_ref().map(id_param).transpose()?;
    let media_ids = params
        .media_ids
        .iter()
        .map(id_param)
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|_| ApiError::Unprocessable("Validation failed: Media is invalid".into()))?;
    let poll = params
        .poll
        .or_else(|| poll_from_form(&headers, &body))
        .map(poll_params)
        .transpose()?;
    let settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    let visibility = params
        .visibility
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.resolved_visibility(current.account.locked));
    let sensitive = params
        .sensitive
        .as_ref()
        .map_or(settings.posting_default_sensitive, |value| {
            bool_param(Some(value))
        });
    let language = params
        .language
        .as_deref()
        .filter(|language| !language.is_empty())
        .or_else(|| settings.default_language());
    // Param or the user's stored default format, like the other posting
    // defaults; unsupported values fall back to plain (Pleroma's behavior).
    let content_type = crate::compose::PostFormat::from_media_type(
        params
            .content_type
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| settings.posting_default_content_type.as_str()),
    );
    // Param or the user's stored default, resolved to the bitmap up front —
    // Mastodon's `Api::InteractionPoliciesConcern#quote_approval_policy`.
    let quote_approval_policy = resolve_quote_policy(
        params
            .quote_approval_policy
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| settings.posting_default_quote_policy.as_str()),
    )?;

    let kind = post_kind_param(params.post_kind.as_deref())?;

    // A far-enough-future `scheduled_at` queues the post and returns a
    // ScheduledStatus instead of publishing now (Mastodon: a value at or
    // before now is ignored, between now and +5min is rejected as too soon).
    if let Some(raw) = params
        .scheduled_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let scheduled_at = super::scheduled_statuses::parse_scheduled_at(raw)?;
        if scheduled_at > time::OffsetDateTime::now_utc() {
            if params.group_id.is_some() || params.external_url.is_some() {
                return Err(ApiError::Unprocessable(
                    "Validation failed: Group posts can't be scheduled".into(),
                ));
            }
            if params.event.is_some()
                || event_from_form(&headers, &body).is_some()
                || kind == plamenu_ap::activity::PostKind::Event
            {
                return Err(ApiError::Unprocessable(
                    "Validation failed: Events can't be scheduled".into(),
                ));
            }
            actions::validate_quote_target(&state, &current.account, quoted_status_id).await?;
            return super::scheduled_statuses::create_scheduled(
                &state,
                current.account.id,
                scheduled_at,
                super::scheduled_statuses::ScheduledDraft {
                    kind,
                    title: params.title.as_deref(),
                    text,
                    content_type: content_type.media_type(),
                    visibility,
                    in_reply_to_id,
                    quoted_status_id,
                    media_ids: &media_ids,
                    spoiler_text: params.spoiler_text.as_deref().unwrap_or(""),
                    sensitive,
                    language,
                    application_id: Some(current.app_id),
                    poll: poll.as_ref(),
                    quote_approval_policy,
                },
            )
            .await
            .map(Json);
        }
    }

    let (stored, _) = actions::post_status_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            text,
            visibility,
            in_reply_to_id,
            media_ids: &media_ids,
            quoted_status_id,
            spoiler_text: params.spoiler_text.as_deref().unwrap_or(""),
            sensitive,
            language,
            content_type,
            poll,
            quote_approval_policy: Some(quote_approval_policy),
            group_id: params.group_id.as_ref().map(id_param).transpose()?,
            title: params.title.as_deref(),
            external_url: params.external_url.as_deref(),
            event: params
                .event
                .or_else(|| event_from_form(&headers, &body))
                .map(EventBody::into_params),
            kind,
        },
        current.app_id,
    )
    .await?;
    // A new local status is an interaction (admin `interactions` measure).
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &stored,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/preview` — render a draft to its final sanitized
/// HTML without posting it. A **Plamenu extension** (Mastodon has no such
/// route; Pleroma folds preview into `POST /api/v1/statuses` behind a
/// `preview: true` param). It pairs with the P4 `content_type` field so a
/// client can display exactly the Markdown/HTML the server will store instead
/// of re-implementing the render and drifting. Read-only: mentions resolve
/// against known accounts only (never the network) and nothing is persisted,
/// so it is safe to call on every keystroke. Accepts the same body as `create`
/// and runs the same spoiler/length/emptiness validation, so a blank or
/// over-long draft 422s here exactly as it would on post.
pub async fn preview(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let params: CreateParams = parse_body(&headers, &body)?;
    let text = params.status.as_deref().unwrap_or("");
    let in_reply_to_id = params.in_reply_to_id.as_ref().map(id_param).transpose()?;
    let has_quote = params.quoted_status_id.is_some();
    let settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    let visibility = params
        .visibility
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.resolved_visibility(current.account.locked));
    let sensitive = params
        .sensitive
        .as_ref()
        .map_or(settings.posting_default_sensitive, |value| {
            bool_param(Some(value))
        });
    let language = params
        .language
        .as_deref()
        .filter(|language| !language.is_empty())
        .or_else(|| settings.default_language());
    let content_type = crate::compose::PostFormat::from_media_type(
        params
            .content_type
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| settings.posting_default_content_type.as_str()),
    );
    let quote_approval_policy = resolve_quote_policy(
        params
            .quote_approval_policy
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| settings.posting_default_quote_policy.as_str()),
    )?;

    // Media the client already uploaded (unattached ids); the preview renders
    // their attachment entities, and `preview_status_json` drops any the viewer
    // doesn't own. Rails `media_ids[]` form recovery mirrors `create`.
    let media_ids = params
        .media_ids
        .iter()
        .map(id_param)
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|_| ApiError::Unprocessable("Validation failed: Media is invalid".into()))?;

    let spoiler = params.spoiler_text.as_deref().unwrap_or("");
    let (text, spoiler_text, sensitive) =
        actions::apply_spoiler_rules(text, spoiler, sensitive, has_quote);
    let limits = state.settings_cache.get(&state.pool).await?;
    actions::ensure_within_character_limit(text, spoiler_text, limits.max_characters)?;
    let composed = crate::compose::compose_preview(&state, text, content_type).await?;
    // The generic preview endpoint carries no title/link, so nothing stands in
    // for a blank body here.
    actions::ensure_status_has_content(&composed.html, &media_ids, false)?;

    Ok(Json(
        preview_status_json(
            &state,
            &current.account,
            &composed,
            PreviewStatusOpts {
                spoiler_text,
                sensitive,
                visibility,
                language,
                in_reply_to_id,
                quote_approval_policy,
                media_ids: &media_ids,
                // The generic Mastodon preview endpoint has no title/link params
                // (those are the group composer's, handled in the web layer).
                title: None,
                external_url: None,
                // The generic endpoint previews an ordinary post; the long-form
                // kind is chosen at post time (`post_kind`), and the web composer
                // has its own preview that carries the kind through.
                kind: plamenu_ap::activity::PostKind::Note,
            },
        )
        .await?,
    ))
}

/// The editable fields of `PUT /api/v1/statuses/{id}`. Absent fields keep
/// their current value, so everything is optional — including `media_ids`,
/// where absent and empty differ.
#[derive(Deserialize)]
pub struct UpdateParams {
    pub status: Option<String>,
    pub spoiler_text: Option<String>,
    pub sensitive: Option<Value>,
    pub language: Option<String>,
    pub media_ids: Option<Vec<Value>>,
    /// Applied only when present — no preference fallback on edit, like
    /// Mastodon's `update_options[:quote_approval_policy] ... if present`.
    pub quote_approval_policy: Option<String>,
    /// Per-attachment alt-text/focus updates, Mastodon's
    /// `media_attributes[][id]` / `[][description]` / `[][focus]`.
    #[serde(default)]
    pub media_attributes: Vec<MediaAttributeParams>,
    /// Pleroma's rich-text format (P4). Applied only when present — absent
    /// keeps the stored format, like the other edit fields.
    pub content_type: Option<String>,
    /// Plamenu extension (E4): rewrite the event fields. This is how an event is
    /// moved and how it is cancelled (`status: "CANCELLED"`) — the two changes
    /// attendees are notified about. Ignored on a status that is not an event.
    pub event: Option<EventBody>,
    /// Plamenu extension: a new headline for a post that already has one (a
    /// long-form `Article`, a group `Page`, an `Event`). Absent keeps the stored
    /// title; a status without one refuses it, since the post kind is not
    /// editable.
    pub title: Option<String>,
}

/// One `media_attributes` entry of the edit endpoint.
#[derive(Deserialize)]
pub struct MediaAttributeParams {
    pub id: Value,
    pub description: Option<String>,
    pub focus: Option<String>,
}

/// `PUT /api/v1/statuses/{id}` — edit one's own status.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let mut params: UpdateParams = parse_body(&headers, &body)?;
    if params.event.is_none() {
        params.event = event_from_form(&headers, &body);
    }
    if params.media_ids.is_none() {
        params.media_ids = media_ids_from_form(&headers, &body);
    }
    let media_ids = params
        .media_ids
        .as_ref()
        .map(|ids| ids.iter().map(id_param).collect::<Result<Vec<i64>, _>>())
        .transpose()
        .map_err(|_| ApiError::Unprocessable("Validation failed: Media is invalid".into()))?;
    let quote_approval_policy = params
        .quote_approval_policy
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(resolve_quote_policy)
        .transpose()?;
    let media_attributes = params
        .media_attributes
        .iter()
        .map(|attrs| {
            Ok(actions::MediaEditAttributes {
                id: id_param(&attrs.id)?,
                description: attrs.description.clone(),
                focus: attrs
                    .focus
                    .as_deref()
                    .filter(|raw| !raw.trim().is_empty())
                    .map(super::media::parse_focus),
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()
        .map_err(|_| ApiError::Unprocessable("Validation failed: Media is invalid".into()))?;
    let item = actions::edit_status(
        &state,
        &current.account,
        status_id,
        EditParams {
            title: params.title.as_deref(),
            text: params.status.as_deref(),
            content_type: params
                .content_type
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(crate::compose::PostFormat::from_media_type),
            spoiler_text: params.spoiler_text.as_deref(),
            sensitive: params.sensitive.as_ref().map(|v| bool_param(Some(v))),
            language: params.language.as_deref().filter(|l| !l.is_empty()),
            media_ids,
            quote_approval_policy,
            media_attributes,
            event: params.event.map(EventBody::into_patch),
        },
    )
    .await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `GET /api/v1/statuses/{id}/history` — past versions, oldest first.
/// Public statuses need no auth, like Mastodon's `authorize_if_got_token!`.
pub async fn history(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(Value::Array(
        render_status_history(&state.pool, &state.config.domain, &stored, viewer_id).await?,
    )))
}

/// `GET /api/v1/statuses/{id}/source` — the raw text for the edit composer.
pub async fn source(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, Some(current.account.id)).await? {
        return Err(ApiError::NotFound);
    }
    let source = status::source_of(&state.pool, stored.id)
        .await?
        .unwrap_or_default();
    Ok(Json(json!({
        "id": stored.id.to_string(),
        "text": source.text,
        "spoiler_text": stored.spoiler_text,
        // Pleroma extension (P4): the format the text was authored in, so
        // rich-text-aware clients can preset their edit composer.
        "content_type": source.content_type,
    })))
}

/// `POST /api/v1/statuses/{id}/translate` — Mastodon's
/// `Api::V1::Statuses::TranslationsController`. Translates a distributable
/// status into the requester's locale through the operator-configured backend
/// (M25). A private/DM status the requester owns is 403 (not distributable);
/// one they can't see is 404; no backend is 404.
pub async fn translate(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, Some(current.account.id)).await? {
        return Err(ApiError::NotFound);
    }
    let target = target_locale(&headers);
    Ok(Json(
        crate::translation::translate_status(&state, &stored, &target, current.account.id).await?,
    ))
}

/// The locale to translate into, standing in for Mastodon's `I18n.locale`: the
/// request's `Accept-Language` first tag (region preserved so `pt-BR` survives),
/// falling back to English.
fn target_locale(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.split(',').next())
        .and_then(|first| first.split(';').next())
        .map(str::trim)
        .filter(|tag| !tag.is_empty() && *tag != "*")
        .unwrap_or("en")
        .to_owned()
}

/// `GET /api/v1/statuses/{id}` — visibility-gated; public needs no auth.
/// Mastodon's `DEFAULT_STATUSES_LIMIT` for the batch fetch endpoint.
const MAX_BATCH_STATUSES: usize = 20;

/// `GET /api/v1/statuses?id[]=…` — batch fetch. Returns the requested statuses
/// the viewer is allowed to see, in request order; unknown ids and statuses
/// hidden by visibility, a block (either direction) or a mute are silently
/// dropped (Mastodon's `permitted_statuses_from_ids`). Over the 20-id cap is a
/// validation error.
pub async fn index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(query.as_deref().unwrap_or(""))
            .map_err(|e| ApiError::BadRequest(format!("invalid query string: {e}")))?;
    // Requested ids, deduplicated but kept in first-seen order.
    let mut requested: Vec<i64> = Vec::new();
    for (key, value) in pairs {
        if key != "id[]" && key != "id" {
            continue;
        }
        if let Ok(id) = value.parse::<i64>()
            && !requested.contains(&id)
        {
            requested.push(id);
        }
    }
    if requested.len() > MAX_BATCH_STATUSES {
        return Err(ApiError::Unprocessable("Validation failed".to_owned()));
    }
    let fetched = status::find_by_ids(&state.pool, &requested).await?;
    // Authors hidden from the viewer by a block (either direction) or a mute,
    // matching the per-thread filtering in `context`.
    let hidden: HashSet<i64> = match viewer_id {
        Some(viewer_id) => {
            let authors: Vec<i64> = fetched.iter().map(|s| s.account_id).collect();
            plamenu_db::block::hidden_authors(&state.pool, viewer_id, &authors)
                .await?
                .into_iter()
                .collect()
        }
        None => HashSet::new(),
    };
    // Drop muted/blocked authors (batched `hidden`), then apply the batched
    // visibility predicate once instead of a per-item `can_view`.
    let mut candidates = fetched;
    candidates.retain(|item| !hidden.contains(&item.account_id));
    let mut visible = filter_viewable(&state.pool, &candidates, viewer_id).await?;
    // Reorder to match the request, like Mastodon's `stable: true` callers.
    visible.sort_by_key(|s| requested.iter().position(|id| *id == s.id));
    Ok(Json(Value::Array(
        render_statuses(&state.pool, &state.config.domain, &visible, viewer_id).await?,
    )))
}

pub async fn show(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    // A client asking for one status by id is looking at it; if it carries a
    // live broadcast, make its state exact before answering (throttled per
    // broadcast, no-op for everything else). Timelines deliberately do not do
    // this — a followed channel's transitions arrive as Updates, and a client
    // that opens the post gets the fresh state here.
    crate::live_refresh::refresh_statuses(&state, &[stored.id]).await;
    Ok(Json(
        render_status(&state.pool, &state.config.domain, &stored, viewer_id).await?,
    ))
}

/// `GET /api/v1/statuses/{id}/context` — ancestors and descendants, shaped by
/// the viewer's thread-order preference: `tree` (Mastodon's chain + depth-first
/// replies with self-replies promoted) or `flat` (Pleroma's whole conversation
/// in arrival order, split at this status). Anonymous viewers get `tree`.
pub async fn context(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    // A signed-in viewer opening a remote thread fills either missing direction:
    // the orphaned parent and replies we never received over federation. Both
    // run in the background; their results land in a later `/context` request.
    if viewer_id.is_some() {
        crate::reply_fetch::on_thread_open(&state, &stored).await?;
    }
    let thread_order = match &viewer {
        Some(current) => user::settings_by_user_id(&state.pool, current.user.id)
            .await?
            .map(|settings| settings.thread_order)
            .unwrap_or_default(),
        None => user::ThreadOrder::default(),
    };
    let (mut ancestors, mut descendants) = match thread_order {
        user::ThreadOrder::Tree => (
            status::ancestors(&state.pool, status_id).await?,
            status::descendants(&state.pool, status_id).await?,
        ),
        user::ThreadOrder::Flat => status::thread_flat(&state.pool, status_id).await?,
    };
    // Mastodon promotes self-replies over the unfiltered tree (its predicate
    // reads a denormalized parent-author column), so compute the set before
    // visibility filtering. Flat mode (Pleroma) never reorders.
    let self_replies = match thread_order {
        user::ThreadOrder::Tree => {
            status::self_reply_ids(status_id, stored.account_id, &descendants)
        }
        user::ThreadOrder::Flat => HashSet::new(),
    };
    // Drop thread members the viewer is not allowed to see, plus authors
    // hidden by a block (either direction) or a mute (Mastodon's
    // `StatusFilter`).
    let hidden: HashSet<i64> = match viewer_id {
        Some(viewer_id) => {
            let authors: Vec<i64> = ancestors
                .iter()
                .chain(descendants.iter())
                .map(|s| s.account_id)
                .collect();
            plamenu_db::block::hidden_authors(&state.pool, viewer_id, &authors)
                .await?
                .into_iter()
                .collect()
        }
        None => HashSet::new(),
    };
    // Drop muted/blocked authors (batched `hidden`), then apply the batched
    // visibility predicate once per list instead of a per-item `can_view`.
    ancestors.retain(|item| !hidden.contains(&item.account_id));
    descendants.retain(|item| !hidden.contains(&item.account_id));
    let keep_ancestors = filter_viewable(&state.pool, &ancestors, viewer_id).await?;
    let mut keep_descendants = filter_viewable(&state.pool, &descendants, viewer_id).await?;
    status::promote_self_replies(&mut keep_descendants, &self_replies);
    Ok(Json(json!({
        "ancestors":
            render_statuses(&state.pool, &state.config.domain, &keep_ancestors, viewer_id)
                .await?,
        "descendants":
            render_statuses(&state.pool, &state.config.domain, &keep_descendants, viewer_id)
                .await?,
    })))
}

/// `DELETE /api/v1/statuses/{id}`.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    // Render before deletion so the client receives the final representation.
    let stored = status::find_local(&state.pool, current.account.id, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut rendered = render_status(
        &state.pool,
        &state.config.domain,
        &stored,
        Some(current.account.id),
    )
    .await?;
    // Mastodon serializes the DELETE response with `source_requested`: the
    // raw `text` replaces `content` for delete-and-redraft.
    apply_redraft_source(&state.pool, &stored, &mut rendered).await?;
    actions::delete_status(
        &state,
        &current.account,
        status_id,
        actions::DeleteMode::Stub,
    )
    .await?;
    Ok(Json(rendered))
}

/// `POST /api/v1/statuses/{id}/favourite`.
pub async fn favourite(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    let item = actions::favourite_status(&state, &current.account, status_id).await?;
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unfavourite`.
pub async fn unfavourite(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    let item = actions::unfavourite_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/downvote` — a documented Plamenu extension
/// (group votes): downvotes a group post; 422 anywhere else. The upvote verb
/// is `favourite`, as everywhere in the ecosystem.
pub async fn downvote(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    let item = actions::downvote_status(&state, &current.account, status_id).await?;
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/undownvote` — retracts a downvote.
pub async fn undownvote(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    let item = actions::undownvote_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

#[derive(Deserialize, Default)]
pub struct RsvpBody {
    /// The attendee's note to the organizer (`participationMessage`). Only ever
    /// reaches a `restricted` event's moderator in practice, but it is sent
    /// regardless — the origin decides whether to show it.
    pub message: Option<String>,
}

/// `POST /api/v1/statuses/{id}/participate` — RSVP to an event (E2).
///
/// A Plamenu extension: Mastodon has no event API at all. Idempotent, so a
/// double-tapped button cannot produce two participations on the origin.
pub async fn participate(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    // The body is optional: a bare POST is a plain RSVP with no message.
    let params: RsvpBody = if body.is_empty() {
        RsvpBody::default()
    } else {
        parse_body(&headers, &body)?
    };
    crate::events::rsvp(
        &state,
        &current.account,
        status_id,
        params.message.as_deref(),
    )
    .await?;
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unparticipate` — withdraw an RSVP (emits
/// `Leave`, never `Undo(Join)`). Idempotent.
pub async fn unparticipate(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    crate::events::cancel_rsvp(&state, &current.account, status_id).await?;
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `GET /api/v1/statuses/{id}/participants` — the attendee list of an event (E3).
///
/// Organizer-only (or a moderator of a local group the event belongs to): a guest
/// list is not public information, and unlike `favourited_by` there is no
/// ecosystem convention making it so. Each entry pairs the account with its RSVP
/// state, so a `restricted` event's approval queue is one request.
pub async fn participants(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::events::may_moderate_event(&state, &current.account, &item).await? {
        return Err(ApiError::Forbidden(
            "You are not the organizer of this event".into(),
        ));
    }
    let rows = plamenu_db::status_participation::for_status(&state.pool, item.id).await?;
    let account_ids: Vec<i64> = rows.iter().map(|row| row.account_id).collect();
    let accounts = crate::entities::render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &account_ids,
        Some(current.account.id),
    )
    .await?;
    let entries: Vec<Value> = rows
        .iter()
        .zip(accounts)
        .map(|(row, account)| {
            json!({
                "account": account,
                "state": row.state.as_str(),
                "message": row.message,
            })
        })
        .collect();
    Ok(Json(json!(entries)))
}

/// `POST /api/v1/statuses/{id}/participants/{account_id}/approve` — accept a
/// pending RSVP to an event we host, emitting `Accept(Join)`.
pub async fn approve_participant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((status_id, account_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let row =
        crate::events::decide_rsvp(&state, &current.account, status_id, account_id, true).await?;
    Ok(Json(json!({"state": row.state.as_str()})))
}

/// `POST /api/v1/statuses/{id}/participants/{account_id}/reject` — refuse a
/// pending RSVP, emitting `Reject(Join)`.
pub async fn reject_participant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((status_id, account_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let row =
        crate::events::decide_rsvp(&state, &current.account, status_id, account_id, false).await?;
    Ok(Json(json!({"state": row.state.as_str()})))
}

/// `POST /api/v1/statuses/{id}/bookmark`.
pub async fn bookmark(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:bookmarks")?;
    let item = actions::bookmark_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unbookmark`.
pub async fn unbookmark(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:bookmarks")?;
    let item = actions::unbookmark_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/pin`.
pub async fn pin(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let item = actions::pin_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unpin`.
pub async fn unpin(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let item = actions::unpin_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/mute` — mutes the status' conversation, so the
/// whole thread stops notifying and renders `muted`. Mastodon keys this on
/// the status' conversation; we ensure one exists (creating it, inheriting the
/// reply parent's) so any visible thread can be muted, not only DMs.
pub async fn mute(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:mutes")?;
    let item = actions::mute_conversation(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unmute`.
pub async fn unmute(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:mutes")?;
    let item = actions::unmute_conversation(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// Mastodon's `DEFAULT_STATUSES_LIMIT` and its doubled hard cap.
const BOOKMARKS_LIMIT: i64 = 20;
const BOOKMARKS_MAX_LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct BookmarksQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/bookmarks` — bookmarked statuses, newest bookmark first,
/// keyset-paginated by bookmark row id (the `Link` header ids are bookmark
/// ids, not status ids, like Mastodon).
pub async fn bookmarks_index(
    State(state): State<AppState>,
    current: CurrentUser,
    axum::extract::Query(query): axum::extract::Query<BookmarksQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    current.require_scope("read:bookmarks")?;
    let limit = query
        .limit
        .unwrap_or(BOOKMARKS_LIMIT)
        .clamp(1, BOOKMARKS_MAX_LIMIT);
    let entries = plamenu_db::bookmark::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    // Fetch and visibility-filter the page as sets. The former singular loop
    // added one lookup per saved row and, for boosts, two more for the target.
    let ids: Vec<i64> = entries.iter().map(|entry| entry.status_id).collect();
    let fetched = status::find_by_ids(&state.pool, &ids).await?;
    let mut items = filter_viewable(&state.pool, &fetched, Some(current.account.id)).await?;
    items.sort_by_key(|item| ids.iter().position(|id| *id == item.id));
    let entities = render_statuses(
        &state.pool,
        &state.config.domain,
        &items,
        Some(current.account.id),
    )
    .await?;
    let mut links = Vec::new();
    if entries.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = entries.last()
    {
        links.push(format!(
            "<https://{}/api/v1/bookmarks?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.row_id
        ));
    }
    if let Some(first) = entries.first() {
        links.push(format!(
            "<https://{}/api/v1/bookmarks?limit={limit}&min_id={}>; rel=\"prev\"",
            state.config.domain, first.row_id
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(axum::http::header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/favourites` — favourited statuses, newest favourite first,
/// keyset-paginated by favourite row id like the bookmarks listing (the
/// `Link` header ids are favourite ids, not status ids).
pub async fn favourites_index(
    State(state): State<AppState>,
    current: CurrentUser,
    axum::extract::Query(query): axum::extract::Query<BookmarksQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    current.require_scope("read:favourites")?;
    let limit = query
        .limit
        .unwrap_or(BOOKMARKS_LIMIT)
        .clamp(1, BOOKMARKS_MAX_LIMIT);
    let entries = plamenu_db::favourite::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    let ids: Vec<i64> = entries.iter().map(|entry| entry.status_id).collect();
    let fetched = status::find_by_ids(&state.pool, &ids).await?;
    let mut items = filter_viewable(&state.pool, &fetched, Some(current.account.id)).await?;
    items.sort_by_key(|item| ids.iter().position(|id| *id == item.id));
    let entities = render_statuses(
        &state.pool,
        &state.config.domain,
        &items,
        Some(current.account.id),
    )
    .await?;
    let mut links = Vec::new();
    if entries.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = entries.last()
    {
        links.push(format!(
            "<https://{}/api/v1/favourites?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.row_id
        ));
    }
    if let Some(first) = entries.first() {
        links.push(format!(
            "<https://{}/api/v1/favourites?limit={limit}&min_id={}>; rel=\"prev\"",
            state.config.domain, first.row_id
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(axum::http::header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// Mastodon's `DEFAULT_ACCOUNTS_LIMIT` (and its doubled hard cap).
const ACCOUNTS_LIMIT: i64 = 40;
const ACCOUNTS_MAX_LIMIT: i64 = 80;

#[derive(Deserialize)]
pub struct AccountsPageQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/statuses/{id}/favourited_by` — who favourited a status.
/// Public for visible statuses like Mastodon's (`authorize_if_got_token!`);
/// a provided token must still carry `read`.
pub async fn favourited_by(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
    axum::extract::Query(query): axum::extract::Query<AccountsPageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = plamenu_db::favourite::favers_of(
        &state.pool,
        status_id,
        viewer_id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries.iter().map(|e| (e.row_id, e.account_id)).collect();
    let path = format!("/api/v1/statuses/{status_id}/favourited_by");
    super::accounts_api::render_account_page(&state, &path, limit, &pairs, viewer_id).await
}

/// `GET /api/v1/statuses/{id}/reblogged_by` — who boosted a status (only
/// public/unlisted boosts are listed, like Mastodon's
/// `distributable_visibility` scope). Same access rules as `favourited_by`.
pub async fn reblogged_by(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
    axum::extract::Query(query): axum::extract::Query<AccountsPageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = status::rebloggers_of(
        &state.pool,
        status_id,
        viewer_id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries.iter().map(|e| (e.row_id, e.account_id)).collect();
    let path = format!("/api/v1/statuses/{status_id}/reblogged_by");
    super::accounts_api::render_account_page(&state, &path, limit, &pairs, viewer_id).await
}

/// `GET /api/v1/statuses/{id}/quotes` — the statuses that quote this one
/// (accepted FEP-044f quotes only), newest quote first. Keyset-paginated by
/// quote row id, so the `Link` header ids are quote ids, not status ids, like
/// Mastodon. Same access rules as `favourited_by`; statuses the viewer can't
/// see (visibility, blocks) are dropped from the page.
pub async fn quotes(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
    axum::extract::Query(query): axum::extract::Query<AccountsPageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = plamenu_db::quote::accepted_quotes_of(
        &state.pool,
        status_id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    // A viewable quoting status: posts since hidden or not visible to the
    // viewer are skipped, never an error.
    let ids: Vec<i64> = entries.iter().map(|e| e.status_id).collect();
    let fetched = status::find_by_ids(&state.pool, &ids).await?;
    let mut items = filter_viewable(&state.pool, &fetched, viewer_id).await?;
    items.sort_by_key(|s| ids.iter().position(|id| *id == s.id));
    let entities = render_statuses(&state.pool, &state.config.domain, &items, viewer_id).await?;
    let mut links = Vec::new();
    if entries.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = entries.last()
    {
        links.push(format!(
            "<https://{}/api/v1/statuses/{status_id}/quotes?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.quote_id
        ));
    }
    if let Some(first) = entries.first() {
        links.push(format!(
            "<https://{}/api/v1/statuses/{status_id}/quotes?limit={limit}&since_id={}>; rel=\"prev\"",
            state.config.domain, first.quote_id
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(axum::http::header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

#[derive(Deserialize)]
pub struct InteractionPolicyParams {
    /// `public` | `followers` | `nobody` — the only values Mastodon accepts
    /// (all automatic; manual policies arrive only over federation).
    pub quote_approval_policy: Option<String>,
}

/// `PATCH /api/v1/statuses/{id}/interaction_policy` — sets who may quote the
/// status. Owner-only; re-federates the post so remotes learn the new policy.
pub async fn interaction_policy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let params: InteractionPolicyParams = parse_body(&headers, &body)?;
    let value = params.quote_approval_policy.as_deref().unwrap_or("public");
    let policy = plamenu_ap::quote_policy::from_client_string(value).ok_or_else(|| {
        ApiError::Unprocessable("Validation failed: Quote policy is invalid".into())
    })?;
    let item = actions::set_interaction_policy(&state, &current.account, status_id, policy).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/quotes/{quote_id}/revoke` — the quoted author
/// withdraws a previously-granted quote authorization. `quote_id` is the
/// *quoting* status' id, like Mastodon (`@status.quotes.find_by(status_id:)`).
pub async fn revoke_quote(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((status_id, quoting_status_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let quoting =
        actions::revoke_quote(&state, &current.account, status_id, quoting_status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &quoting,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/reblog` — returns the boost wrapper.
pub async fn reblog(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let boost = actions::reblog_status(&state, &current.account, status_id).await?;
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &boost,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `POST /api/v1/statuses/{id}/unreblog` — returns the original.
pub async fn unreblog(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let item = actions::unreblog_status(&state, &current.account, status_id).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}
