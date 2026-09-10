//! Write actions for the web UI: compose, the status toggles (favourite,
//! boost, bookmark) and follow/unfollow.
//!
//! All of them are POST-only and follow the POST/Redirect/GET pattern: each
//! form carries the session CSRF token and a `return_to` path, the handler
//! performs the action through the shared `crate::actions` layer, then 303s
//! back to where the user was. Nothing here needs JavaScript.

use axum::body::{Body, Bytes};
use axum::extract::{Form, FromRequest, Multipart, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::{conversation, media, poll, status, user};
use serde::Deserialize;
use serde_json::Value;

use super::pages;
use super::reactions;
use super::session::{WebUser, csrf_rejection};
use crate::actions::{self, PollParams, PostParams};
use crate::error::ApiError;
use crate::state::AppState;

/// Fields shared by every state-changing form.
#[derive(Deserialize)]
pub struct ActionForm {
    csrf: String,
    return_to: Option<String>,
}

/// The composer's event fields, mapped for [`actions::EventParams`] — or `None`
/// when the author did not pick the event post kind.
///
fn event_params_of(
    user: &WebUser,
    fields: &ComposeFields,
) -> Result<Option<actions::EventParams>, ApiError> {
    if fields.post_kind.trim() != "event" {
        return Ok(None);
    }
    let some = |value: &str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    let zone = if fields.event_timezone.trim().is_empty() {
        user.clock.name()
    } else {
        crate::time_zones::normalize(&fields.event_timezone).ok_or_else(|| {
            ApiError::Unprocessable(user.locale.text("compose-event-invalid-zone"))
        })?
    };
    let rfc3339 = crate::entities::rfc3339;
    let start = super::clock::event_instant(&fields.event_start, zone, user.locale)?;
    let end = some(&fields.event_end)
        .map(|raw| super::clock::event_instant(&raw, zone, user.locale))
        .transpose()?;
    Ok(Some(actions::EventParams {
        start_time: rfc3339(start)?,
        end_time: end.map(rfc3339).transpose()?,
        timezone: Some(zone.to_owned()),
        join_mode: some(&fields.event_join_mode).unwrap_or_else(|| "free".to_owned()),
        external_participation_url: some(&fields.event_external_url),
        max_attendees: fields.event_capacity.trim().parse().ok(),
        status: some(&fields.event_status).unwrap_or_else(|| "CONFIRMED".to_owned()),
        is_online: fields.event_online,
        location_name: some(&fields.event_location),
        location_street: some(&fields.event_street),
        location_locality: some(&fields.event_locality),
        location_region: some(&fields.event_region),
        location_country: some(&fields.event_country),
        location_postal_code: some(&fields.event_postal_code),
    }))
}

/// The explicit post kind used by the preview.
fn preview_kind(fields: &ComposeFields) -> plamenu_ap::activity::PostKind {
    match fields.post_kind.trim() {
        "article" => plamenu_ap::activity::PostKind::Article,
        "event" => plamenu_ap::activity::PostKind::Event,
        _ => plamenu_ap::activity::PostKind::Note,
    }
}

/// A `return_to` is only honoured when it is a local, absolute path — never a
/// protocol-relative `//host` — so the forms can't become open redirects.
pub(super) fn safe_return(return_to: Option<&str>, fallback: &str) -> String {
    match return_to {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => path.to_owned(),
        _ => fallback.to_owned(),
    }
}

fn redirect_to(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path.to_owned())]).into_response()
}

/// Parses an `application/x-www-form-urlencoded` body into pairs, preserving
/// repeated keys (which `serde` can't fold into a `Vec`) — needed for the
/// poll-option and vote-choice forms.
fn form_pairs(body: &Bytes) -> Vec<(String, String)> {
    serde_urlencoded::from_bytes(body).unwrap_or_default()
}

/// Looks up a single field's value among parsed form pairs.
fn field<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn truthy(value: &str) -> bool {
    !matches!(
        value,
        "" | "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"
    )
}

/// The compose form, including its optional poll. A reply, a quote, a content
/// warning, and a poll are all driven from one `/web/compose` endpoint so the
/// inline box and the full compose page share a handler.
#[derive(Default)]
struct ComposeFields {
    csrf: String,
    status: String,
    visibility: String,
    sensitive: bool,
    language: Option<String>,
    spoiler_text: String,
    in_reply_to_id: Option<String>,
    quoted_status_id: Option<String>,
    poll_options: Vec<String>,
    poll_expires_in: Option<i64>,
    poll_multiple: bool,
    quote_policy: String,
    content_type: String,
    /// Group submission fields — the group page's post form.
    group_id: Option<String>,
    title: String,
    external_url: String,
    /// Only the explicit selector chooses the post kind.
    post_kind: String,
    event_start: String,
    event_end: String,
    /// The selected zone used to interpret both event times.
    event_timezone: String,
    event_join_mode: String,
    event_external_url: String,
    event_capacity: String,
    event_status: String,
    event_online: bool,
    event_location: String,
    event_street: String,
    event_locality: String,
    event_region: String,
    event_country: String,
    event_postal_code: String,
    /// A `datetime-local` value (the viewer's preference time zone); non-empty
    /// queues the post as a scheduled status instead of publishing.
    scheduled_at: String,
}

/// Maps one submitted compose-form pair onto [`ComposeFields`].
fn apply_compose_pair(fields: &mut ComposeFields, key: &str, value: &str) {
    match key {
        "csrf" => value.clone_into(&mut fields.csrf),
        "status" => value.clone_into(&mut fields.status),
        "visibility" => value.clone_into(&mut fields.visibility),
        "sensitive" => {
            fields.sensitive = truthy(value);
        }
        "language" if !value.trim().is_empty() => fields.language = Some(value.to_owned()),
        "spoiler_text" => value.clone_into(&mut fields.spoiler_text),
        "in_reply_to_id" => fields.in_reply_to_id = Some(value.to_owned()),
        "quoted_status_id" => fields.quoted_status_id = Some(value.to_owned()),
        "poll_options[]" | "poll_options" if !value.trim().is_empty() => {
            fields.poll_options.push(value.to_owned());
        }
        "poll_expires_in" => fields.poll_expires_in = value.parse().ok(),
        "poll_multiple" => fields.poll_multiple = matches!(value, "on" | "true" | "1"),
        "quote_policy" if !value.trim().is_empty() => value.clone_into(&mut fields.quote_policy),
        "content_type" if !value.trim().is_empty() => value.clone_into(&mut fields.content_type),
        "group_id" if !value.trim().is_empty() => fields.group_id = Some(value.to_owned()),
        "title" if !value.trim().is_empty() => value.clone_into(&mut fields.title),
        "external_url" => value.clone_into(&mut fields.external_url),
        "scheduled_at" => value.clone_into(&mut fields.scheduled_at),
        // Preserve conflicting duplicate values for validation.
        "post_kind" if !value.trim().is_empty() => {
            let value = value.trim();
            if fields.post_kind.is_empty() {
                value.clone_into(&mut fields.post_kind);
            } else if fields.post_kind != value {
                fields.post_kind = format!("{},{value}", fields.post_kind);
            }
        }
        // Event fields (E4).
        "event_start" => value.clone_into(&mut fields.event_start),
        "event_end" => value.clone_into(&mut fields.event_end),
        "event_timezone" => value.clone_into(&mut fields.event_timezone),
        "event_join_mode" if !value.trim().is_empty() => {
            value.clone_into(&mut fields.event_join_mode);
        }
        "event_external_url" => value.clone_into(&mut fields.event_external_url),
        "event_capacity" => value.clone_into(&mut fields.event_capacity),
        "event_status" if !value.trim().is_empty() => {
            value.clone_into(&mut fields.event_status);
        }
        "event_online" => fields.event_online = truthy(value),
        "event_location" => value.clone_into(&mut fields.event_location),
        "event_street" => value.clone_into(&mut fields.event_street),
        "event_locality" => value.clone_into(&mut fields.event_locality),
        "event_region" => value.clone_into(&mut fields.event_region),
        "event_country" => value.clone_into(&mut fields.event_country),
        "event_postal_code" => value.clone_into(&mut fields.event_postal_code),
        _ => {}
    }
}

fn parse_id(raw: Option<&str>) -> Result<Option<i64>, ()> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.parse::<i64>().map(Some).map_err(|_| ()),
        None => Ok(None),
    }
}

/// One uploaded file from the compose form, with its optional alt text. The
/// text field carrying the description (`media_alt[]`) precedes its file part
/// in the form, so it is buffered and attached to the next file seen.
struct UploadPart {
    bytes: Vec<u8>,
    description: Option<String>,
}

/// Reads the compose multipart body into ordinary `(name, value)` text pairs
/// (so the existing url-encoded parsing logic is reused unchanged) plus the
/// uploaded files. A `media_alt[]` text field applies to the `media[]` file
/// part that follows it.
async fn read_compose(
    mut multipart: Multipart,
) -> Result<(Vec<(String, String)>, Vec<UploadPart>), ApiError> {
    let mut pairs = Vec::new();
    let mut files = Vec::new();
    let mut pending_alt: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "media[]" | "media" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("invalid upload: {e}")))?;
                // An untouched file input submits an empty part — skip it so an
                // attachment-free post still goes through.
                if bytes.is_empty() {
                    pending_alt = None;
                } else {
                    files.push(UploadPart {
                        bytes: bytes.to_vec(),
                        description: pending_alt.take().filter(|d| !d.trim().is_empty()),
                    });
                }
            }
            "media_alt[]" | "media_alt" => {
                let text = field.text().await.unwrap_or_default();
                pending_alt = Some(text);
            }
            _ => {
                let value = field.text().await.unwrap_or_default();
                pairs.push((name, value));
            }
        }
    }
    Ok((pairs, files))
}

/// Stores the compose form's attachments synchronously (even video/audio, so
/// the media is finished and attachable by the time the status is created) and
/// returns their ids. Refuses up front if the post would exceed the attachment
/// cap that `post_status` enforces on the resulting ids.
async fn store_compose_media(
    state: &AppState,
    account_id: i64,
    files: Vec<UploadPart>,
) -> Result<Vec<i64>, ApiError> {
    let limits = state.settings_cache.get(&state.pool).await?;
    actions::ensure_media_count(files.len(), limits.max_media_attachments)?;
    let mut media_ids = Vec::with_capacity(files.len());
    for part in files {
        let (_, entity) = crate::routes::media::store_upload(
            state,
            account_id,
            part.bytes,
            part.description,
            None,
            false,
        )
        .await?;
        if let Some(id) = entity
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok())
        {
            media_ids.push(id);
        }
    }
    Ok(media_ids)
}

/// Extracts the compose multipart form under a body limit derived from the
/// *current* instance settings — the configurable `max_media_attachments` at the
/// audio/video per-file ceiling — rather than a fixed router layer that can't
/// track the runtime setting. The composer submits every attachment in one
/// multipart body, so this is its aggregate memory guard; the exact per-post
/// count and per-file caps are still enforced downstream (`ensure_media_count`,
/// `store_upload`). The compose route disables axum's default 2 MB limit so this
/// per-request limit is the only one in force (see `web::compose_route`).
async fn compose_multipart(state: &AppState, request: Request) -> Result<Multipart, Response> {
    let max_attachments = state
        .settings_cache
        .get(&state.pool)
        .await
        .map_err(|e| ApiError::from(e).into_response())?
        .max_media_attachments;
    let body_limit = usize::try_from(max_attachments)
        .unwrap_or(0)
        .saturating_mul(crate::media_processing::MAX_AV_UPLOAD_BYTES)
        .saturating_add(1024 * 1024);
    // Fast, clear rejection when the client announces an oversized body; the
    // `Limited` wrapper below is the hard guard for chunked or lying lengths.
    let announced = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if announced.is_some_and(|len| len > body_limit) {
        return Err(ApiError::Unprocessable(
            "Validation failed: attachments exceed the size limit".into(),
        )
        .into_response());
    }
    let (parts, body) = request.into_parts();
    let limited = Body::new(http_body_util::Limited::new(body, body_limit));
    Multipart::from_request(Request::from_parts(parts, limited), state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")).into_response())
}

/// `POST /web/compose` — publish a new status, or render a server-side
/// preview of the draft without posting. `op=preview` renders the draft to its
/// final HTML and shows it below the composer; `op=post` (or Enter) publishes.
/// The body is `multipart/form-data` so files ride along with the text fields.
/// A JS preview sends the `X-Compose-Preview` header and gets back just the
/// rendered card; a no-JS preview (or any validation error) re-renders the whole
/// page with the form exactly as the user left it.
#[allow(clippy::too_many_lines)] // one compose endpoint: parse, media, dispatch
pub async fn compose(State(state): State<AppState>, user: WebUser, request: Request) -> Response {
    let fragment = request.headers().contains_key("x-compose-preview");
    let multipart = match compose_multipart(&state, request).await {
        Ok(parsed) => parsed,
        Err(resp) => return resp,
    };
    let (pairs, files) = match read_compose(multipart).await {
        Ok(parsed) => parsed,
        Err(err) => return err.into_response(),
    };
    let settings = match user::settings_by_user_id(&state.pool, user.current.user.id).await {
        Ok(Some(settings)) => settings,
        Ok(None) => user::UserSettings::default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let mut fields = ComposeFields {
        visibility: settings
            .resolved_visibility(user.current.account.locked)
            .to_owned(),
        sensitive: settings.posting_default_sensitive,
        language: settings.default_language().map(str::to_owned),
        quote_policy: settings.posting_default_quote_policy.as_str().to_owned(),
        content_type: settings.posting_default_content_type.as_str().to_owned(),
        ..ComposeFields::default()
    };
    for (key, value) in &pairs {
        apply_compose_pair(&mut fields, key, value);
    }

    if !user.csrf_ok(&fields.csrf) {
        return csrf_rejection();
    }
    // Attachments kept from a prior preview (ids) plus any new file uploads. A
    // no-JS preview uploads first so media survives the round-trip as ids.
    let Ok(media_keep) = parse_media_keep(&pairs) else {
        return (StatusCode::BAD_REQUEST, "invalid attachment").into_response();
    };
    let media_ids =
        match reconcile_compose_media(&state, user.current.account.id, &pairs, &media_keep, files)
            .await
        {
            Ok(ids) => ids,
            Err(err) => return err.into_response(),
        };

    if fields.in_reply_to_id.is_some() && !matches!(fields.post_kind.trim(), "" | "note") {
        return render_composer_response(
            &state,
            &user,
            &fields,
            &media_ids,
            None,
            Some(&user.locale.text("compose-reply-note-only")),
        )
        .await;
    }

    if field(&pairs, "op") == Some("change_kind") {
        return render_composer_response(&state, &user, &fields, &media_ids, None, None).await;
    }

    if field(&pairs, "op") == Some("preview") {
        // A JS preview composites its local thumbnails client-side rather than
        // uploading, so it flags media-only drafts to skip the blank check.
        let has_local_media = field(&pairs, "preview_has_media") == Some("1");
        return compose_preview_response(
            &state,
            &user,
            &fields,
            &media_ids,
            has_local_media,
            fragment,
        )
        .await;
    }

    let (Ok(in_reply_to_id), Ok(quoted_status_id), Ok(group_id)) = (
        parse_id(fields.in_reply_to_id.as_deref()),
        parse_id(fields.quoted_status_id.as_deref()),
        parse_id(fields.group_id.as_deref()),
    ) else {
        return (StatusCode::BAD_REQUEST, "invalid target").into_response();
    };
    let poll = (!fields.poll_options.is_empty()).then(|| PollParams {
        options: fields.poll_options.clone(),
        // Default to a day when the form omits a duration.
        expires_in: Some(fields.poll_expires_in.unwrap_or(86_400)),
        multiple: fields.poll_multiple,
        hide_totals: false,
    });
    let Some(quote_approval_policy) =
        plamenu_ap::quote_policy::from_client_string(&fields.quote_policy)
    else {
        return (StatusCode::BAD_REQUEST, "invalid quote policy").into_response();
    };
    let spoiler = fields.spoiler_text.trim();

    // A filled Schedule field queues the draft instead of publishing: the
    // wall-clock reading is resolved in the viewer's preference zone and the
    // draft stored exactly as `POST /api/v1/statuses` would store it. Group
    // posts can't be scheduled — membership must hold at publish time (the
    // same rule the API enforces).
    if !fields.scheduled_at.trim().is_empty() {
        if group_id.is_some() {
            return render_composer_response(
                &state,
                &user,
                &fields,
                &media_ids,
                None,
                Some(&user.locale.text("compose-error-group-scheduled")),
            )
            .await;
        }
        if !matches!(fields.post_kind.trim(), "" | "note" | "article") {
            return render_composer_response(
                &state,
                &user,
                &fields,
                &media_ids,
                None,
                Some(&user.locale.text("compose-error-kind-scheduled")),
            )
            .await;
        }
        if let Err(err) =
            crate::actions::validate_quote_target(&state, &user.current.account, quoted_status_id)
                .await
        {
            return match banner_message(&err) {
                Some(msg) => {
                    render_composer_response(&state, &user, &fields, &media_ids, None, Some(&msg))
                        .await
                }
                None => err.into_response(),
            };
        }
        let result =
            match super::scheduled::resolve_schedule_input(&state, &user, &fields.scheduled_at)
                .await
            {
                Ok(scheduled_at) => {
                    crate::routes::scheduled_statuses::create_scheduled(
                        &state,
                        user.current.account.id,
                        scheduled_at,
                        crate::routes::scheduled_statuses::ScheduledDraft {
                            kind: preview_kind(&fields),
                            title: (!fields.title.trim().is_empty()).then(|| fields.title.trim()),
                            text: fields.status.trim(),
                            content_type: crate::compose::PostFormat::from_media_type(
                                &fields.content_type,
                            )
                            .media_type(),
                            visibility: &fields.visibility,
                            in_reply_to_id,
                            quoted_status_id,
                            media_ids: &media_ids,
                            spoiler_text: spoiler,
                            sensitive: fields.sensitive || !spoiler.is_empty(),
                            language: fields.language.as_deref(),
                            application_id: None,
                            poll: poll.as_ref(),
                            quote_approval_policy,
                        },
                    )
                    .await
                }
                Err(err) => Err(err),
            };
        return match result {
            Ok(_) => redirect_to("/settings/scheduled?saved=scheduled"),
            Err(err) => match banner_message(&err) {
                Some(msg) => {
                    render_composer_response(&state, &user, &fields, &media_ids, None, Some(&msg))
                        .await
                }
                None => err.into_response(),
            },
        };
    }

    // Reject conflicting or unknown kinds submitted by a malformed form.
    let kind = match fields.post_kind.trim() {
        "" | "note" => plamenu_ap::activity::PostKind::Note,
        "article" => plamenu_ap::activity::PostKind::Article,
        "event" => plamenu_ap::activity::PostKind::Event,
        _ => {
            return render_composer_response(
                &state,
                &user,
                &fields,
                &media_ids,
                None,
                Some(&user.locale.text("compose-error-two-kinds")),
            )
            .await;
        }
    };

    // Resolved before the post so a bad date is a composer banner, not a 500.
    let event = match event_params_of(&user, &fields) {
        Ok(event) => event,
        Err(err) => {
            return match banner_message(&err) {
                Some(msg) => {
                    render_composer_response(&state, &user, &fields, &media_ids, None, Some(&msg))
                        .await
                }
                None => err.into_response(),
            };
        }
    };
    let result = actions::post_status(
        &state,
        PostParams {
            username: &user.current.account.username,
            text: fields.status.trim(),
            visibility: &fields.visibility,
            in_reply_to_id,
            media_ids: &media_ids,
            quoted_status_id,
            spoiler_text: spoiler,
            sensitive: fields.sensitive || !spoiler.is_empty(),
            language: fields.language.as_deref(),
            content_type: crate::compose::PostFormat::from_media_type(&fields.content_type),
            poll,
            quote_approval_policy: Some(quote_approval_policy),
            group_id,
            title: Some(fields.title.as_str()),
            external_url: Some(fields.external_url.as_str()),
            event,
            kind,
        },
    )
    .await;
    match result {
        // Land on the new post's thread so the author sees it in context.
        Ok((stored, _)) => redirect_to(&format!(
            "/@{}/{}",
            user.current.account.username, stored.id
        )),
        // A validation failure re-renders the composer with a banner and the
        // user's state intact, instead of the old bare error page.
        Err(err) => match banner_message(&err) {
            Some(msg) => {
                render_composer_response(&state, &user, &fields, &media_ids, None, Some(&msg)).await
            }
            None => err.into_response(),
        },
    }
}

/// The `media_keep[]` ids (attachments from a prior preview to keep). `Err` on
/// a malformed id — a bad request, not something to silently drop.
fn parse_media_keep(pairs: &[(String, String)]) -> Result<Vec<i64>, ()> {
    let mut keep = Vec::new();
    for (key, value) in pairs {
        if key == "media_keep[]" || key == "media_keep" {
            keep.push(value.trim().parse::<i64>().map_err(|_| ())?);
        }
    }
    Ok(keep)
}

/// Reconciles the composer's media into the final ordered id list: kept
/// attachments (with any edited alt text applied) followed by freshly uploaded
/// files. The total is capped before anything new is stored so an over-cap
/// submit uploads nothing.
async fn reconcile_compose_media(
    state: &AppState,
    account_id: i64,
    pairs: &[(String, String)],
    media_keep: &[i64],
    files: Vec<UploadPart>,
) -> Result<Vec<i64>, ApiError> {
    let limits = state.settings_cache.get(&state.pool).await?;
    actions::ensure_media_count(media_keep.len() + files.len(), limits.max_media_attachments)?;
    // One statement for the whole kept set. `update_attributes_many` only
    // touches the caller's own unattached uploads, so a forged `media_keep`
    // id changes nothing here (and fails at post).
    let mut set_description = Vec::with_capacity(media_keep.len());
    let mut descriptions: Vec<Option<String>> = Vec::with_capacity(media_keep.len());
    for id in media_keep {
        let description = field(pairs, &format!("media_alt_{id}"))
            .map(|alt| (!alt.trim().is_empty()).then(|| alt.to_owned()));
        set_description.push(description.is_some());
        descriptions.push(description.flatten());
    }
    media::update_attributes_many(
        &state.pool,
        account_id,
        media_keep,
        &set_description,
        &descriptions,
    )
    .await?;
    let new_ids = store_compose_media(state, account_id, files).await?;
    let mut media_ids = media_keep.to_vec();
    media_ids.extend(new_ids);
    Ok(media_ids)
}

/// The user-facing message of a validation error, for the composer's banner;
/// `None` for internal errors that should surface as a plain error response.
fn banner_message(err: &ApiError) -> Option<String> {
    match err {
        ApiError::BadRequest(msg) | ApiError::Unprocessable(msg) | ApiError::Forbidden(msg) => {
            Some(msg.clone())
        }
        _ => None,
    }
}

/// Handles an `op=preview` submit: build the rendered card, then either return
/// it as a fragment (JS) or re-render the whole page with it below the composer
/// (no-JS). A validation failure becomes a banner rather than an error page.
async fn compose_preview_response(
    state: &AppState,
    user: &WebUser,
    fields: &ComposeFields,
    media_ids: &[i64],
    has_local_media: bool,
    fragment: bool,
) -> Response {
    match build_compose_preview_card(state, user, fields, media_ids, has_local_media).await {
        Ok(card) if fragment => html! {
            h2.compose__preview-heading { (user.locale.text("compose-preview")) }
            (card)
        }
        .into_response(),
        Ok(card) => {
            render_composer_response(state, user, fields, media_ids, Some(card), None).await
        }
        Err(err) => match banner_message(&err) {
            Some(msg) if fragment => (
                StatusCode::UNPROCESSABLE_ENTITY,
                html! { p.compose__error role="alert" { (msg) } },
            )
                .into_response(),
            Some(msg) => {
                render_composer_response(state, user, fields, media_ids, None, Some(&msg)).await
            }
            None => err.into_response(),
        },
    }
}

/// Runs the same validation as posting, then renders the draft as the timeline
/// card. Read-only — nothing is persisted (the media rows already exist as
/// unattached uploads).
async fn build_compose_preview_card(
    state: &AppState,
    user: &WebUser,
    fields: &ComposeFields,
    media_ids: &[i64],
    has_local_media: bool,
) -> Result<Markup, ApiError> {
    let limits = state.settings_cache.get(&state.pool).await?;
    let (text, spoiler_text, sensitive) = actions::apply_spoiler_rules(
        fields.status.trim(),
        fields.spoiler_text.trim(),
        fields.sensitive,
        fields.quoted_status_id.is_some(),
    );
    // A long-form draft previews against its own limit, or the preview would
    // refuse a post the composer will happily publish.
    let kind = preview_kind(fields);
    actions::ensure_within_character_limit(
        text,
        spoiler_text,
        if kind == plamenu_ap::activity::PostKind::Article {
            limits.max_characters_long_form
        } else {
            limits.max_characters
        },
    )?;
    let content_type = crate::compose::PostFormat::from_media_type(&fields.content_type);
    let composed = crate::compose::compose_preview(state, text, content_type).await?;
    // A JS preview carries its media client-side (not uploaded), so `media_ids`
    // is empty even when there are attachments — trust its flag for the blank
    // check. The real post still validates media for real on `op=post`.
    let has_typed_content = (fields.group_id.is_some()
        || kind != plamenu_ap::activity::PostKind::Note)
        && (!fields.title.trim().is_empty() || !fields.external_url.trim().is_empty());
    if !has_local_media {
        actions::ensure_status_has_content(&composed.html, media_ids, has_typed_content)?;
    }
    let quote_approval_policy = plamenu_ap::quote_policy::from_client_string(&fields.quote_policy)
        .ok_or_else(|| ApiError::BadRequest("invalid quote policy".into()))?;
    let in_reply_to_id = parse_id(fields.in_reply_to_id.as_deref()).ok().flatten();
    let mut value = crate::entities::preview_status_json(
        state,
        &user.current.account,
        &composed,
        crate::entities::PreviewStatusOpts {
            spoiler_text,
            sensitive,
            visibility: &fields.visibility,
            language: fields.language.as_deref(),
            in_reply_to_id,
            quote_approval_policy,
            media_ids,
            kind,
            // A titled post previews with its title/link folded in, like the
            // posted status: a group submission (`group_id` set), a long-form
            // post or an event. An empty title/url folds to nothing.
            title: (fields.group_id.is_some() || kind != plamenu_ap::activity::PostKind::Note)
                .then(|| fields.title.trim())
                .filter(|t| !t.is_empty()),
            external_url: fields
                .group_id
                .as_deref()
                .and(Some(fields.external_url.trim()))
                .filter(|u| !u.is_empty()),
        },
    )
    .await?;
    if let Some(event) = event_params_of(user, fields)? {
        actions::validate_event(&event)?;
        value["event"] = serde_json::json!({
            "start_time": event.start_time,
            "end_time": event.end_time,
            "timezone": event.timezone,
            "status": event.status,
            "join_mode": event.join_mode,
            "external_participation_url": event.external_participation_url,
            "max_attendees": event.max_attendees,
            "is_online": event.is_online,
            "location": event.location_name,
            "location_street": event.location_street,
            "location_locality": event.location_locality,
            "location_region": event.location_region,
            "location_country": event.location_country,
            "location_postal_code": event.location_postal_code,
        });
    }
    if let Some((id, uri, name)) = crate::webxdc::invitation_in_text(&state.pool, text).await? {
        value["webxdc_invitation"] = crate::webxdc::invitation_entity(Some(id), &uri, &name);
    }
    pages::render_preview_card(state, user, &value).await
}

/// Re-renders the whole compose page from the submitted fields — used by the
/// no-JS preview and the validation-error path so nothing the user entered is
/// lost.
async fn render_composer_response(
    state: &AppState,
    user: &WebUser,
    fields: &ComposeFields,
    media_ids: &[i64],
    preview: Option<Markup>,
    error: Option<&str>,
) -> Response {
    let media = match crate::entities::preview_media_json(
        &state.pool,
        &state.config.domain,
        media_ids,
        Some(user.current.account.id),
    )
    .await
    {
        Ok(media) => media,
        Err(err) => return err.into_response(),
    };
    let echo = pages::ComposerEcho {
        kind: &fields.post_kind,
        event: super::view::EventCompose {
            start: &fields.event_start,
            end: &fields.event_end,
            timezone: &fields.event_timezone,
            join_mode: &fields.event_join_mode,
            external_url: &fields.event_external_url,
            capacity: &fields.event_capacity,
            status: &fields.event_status,
            location: &fields.event_location,
            street: &fields.event_street,
            locality: &fields.event_locality,
            region: &fields.event_region,
            country: &fields.event_country,
            postal_code: &fields.event_postal_code,
            online: fields.event_online,
        },
        text: fields.status.trim(),
        spoiler_text: fields.spoiler_text.trim(),
        visibility: &fields.visibility,
        sensitive: fields.sensitive,
        language: fields.language.as_deref(),
        content_type: &fields.content_type,
        quote_policy: &fields.quote_policy,
        poll_options: &fields.poll_options,
        poll_expires_in: fields.poll_expires_in,
        poll_multiple: fields.poll_multiple,
        reply: fields.in_reply_to_id.as_deref(),
        quote: fields.quoted_status_id.as_deref(),
        group_id: fields.group_id.as_deref(),
        title: fields.title.trim(),
        external_url: fields.external_url.trim(),
        scheduled_at: fields.scheduled_at.trim(),
        media: &media,
        preview,
        error,
    };
    match pages::render_composer(state, user, echo).await {
        Ok(markup) => markup.into_response(),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/statuses/{id}/vote` — cast the viewer's poll choices.
pub async fn vote(
    State(state): State<AppState>,
    user: WebUser,
    Path(status_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body);
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let back = safe_return(field(&pairs, "return_to"), "/");
    let choices: Vec<Value> = pairs
        .iter()
        .filter(|(k, _)| k == "choices[]" || k == "choices")
        .map(|(_, v)| Value::String(v.clone()))
        .collect();
    let poll = match poll::find_by_status(&state.pool, status_id).await {
        Ok(Some(poll)) => poll,
        Ok(None) => return redirect_to(&back),
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) =
        crate::polls::cast_vote(&state, &user.current.account, poll.id, &choices).await
    {
        return err.into_response();
    }
    redirect_to(&back)
}

/// `POST /web/statuses/{id}/delete` — delete one's own status.
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    // After a delete the post is gone, so a `return_to` pointing at it would
    // 404 — fall back home for the thread permalink case.
    let back = safe_return(form.return_to.as_deref(), "/");
    let back = if back.contains(&format!("/{id}")) {
        "/"
    } else {
        &back
    };
    if let Err(err) =
        actions::delete_status(&state, &user.current.account, id, actions::DeleteMode::Stub).await
    {
        return err.into_response();
    }
    redirect_to(back)
}

pub async fn favourite(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::favourite_status(state, actor, id)
    })
    .await
}

pub async fn unfavourite(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unfavourite_status(state, actor, id)
    })
    .await
}

/// The group-post vote buttons. Upvote == favourite — one store, one
/// wire shape — the separate verbs exist so the vote cluster's toggles flip
/// to their own `un`-prefixed endpoints like every `action_form`.
pub async fn upvote(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::favourite_status(state, actor, id)
    })
    .await
}

pub async fn unupvote(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unfavourite_status(state, actor, id)
    })
    .await
}

pub async fn downvote(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::downvote_status(state, actor, id)
    })
    .await
}

pub async fn undownvote(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::undownvote_status(state, actor, id)
    })
    .await
}

/// The event RSVP buttons (E2). Distinct from the vote/favourite family because
/// an RSVP is a negotiation with an organizer, not a toggle: `participate` may
/// land as *pending* and stay there indefinitely, and the reply — if it ever
/// comes — arrives asynchronously as an `Accept`/`Reject`.
#[derive(serde::Deserialize)]
pub struct RsvpForm {
    csrf: String,
    return_to: Option<String>,
    /// Optional note to the organizer.
    message: Option<String>,
}

pub async fn participate(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<RsvpForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    if let Err(err) =
        crate::events::rsvp(&state, &user.current.account, id, form.message.as_deref()).await
    {
        return err.into_response();
    }
    redirect_to(&back)
}

pub async fn unparticipate(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    if let Err(err) = crate::events::cancel_rsvp(&state, &user.current.account, id).await {
        return err.into_response();
    }
    redirect_to(&back)
}

/// The organizer's verdict on a pending RSVP to their own event (E3): emits
/// `Accept(Join)` / `Reject(Join)`.
pub async fn decide_participant(
    State(state): State<AppState>,
    user: WebUser,
    Path((status_id, account_id)): Path<(i64, i64)>,
    Form(form): Form<ActionForm>,
    approve: bool,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(
        form.return_to.as_deref(),
        &format!("/web/statuses/{status_id}"),
    );
    if let Err(err) = crate::events::decide_rsvp(
        &state,
        &user.current.account,
        status_id,
        account_id,
        approve,
    )
    .await
    {
        return err.into_response();
    }
    redirect_to(&back)
}

pub async fn approve_participant(
    state: State<AppState>,
    user: WebUser,
    path: Path<(i64, i64)>,
    form: Form<ActionForm>,
) -> Response {
    decide_participant(state, user, path, form, true).await
}

pub async fn reject_participant(
    state: State<AppState>,
    user: WebUser,
    path: Path<(i64, i64)>,
    form: Form<ActionForm>,
) -> Response {
    decide_participant(state, user, path, form, false).await
}

/// `POST /web/statuses/{id}/cancel-event` — the organizer calls off their own
/// event (E4).
///
/// Its own action rather than a field in the edit form: cancelling is the single
/// most consequential thing an organizer can say about an event (every attendee is
/// notified and every consumer must stop advertising it), and burying it in a form
/// full of address fields invites doing it by accident. Flips `ical:status` to
/// `CANCELLED` and federates the `Update(Event)`, keeping every other field — the
/// post stays readable so people can see *what* was called off.
pub async fn cancel_event(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), &format!("/web/statuses/{id}"));
    // A patch of exactly one field: this action says one thing, and every other
    // fact about the event is carried over by the merge in `edit_status`.
    let params = actions::EventPatch {
        status: Some("CANCELLED".to_owned()),
        ..actions::EventPatch::default()
    };
    if let Err(err) = actions::edit_status(
        &state,
        &user.current.account,
        id,
        actions::EditParams {
            title: None,
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: Some(params),
        },
    )
    .await
    {
        return err.into_response();
    }
    redirect_to(&back)
}

pub async fn reblog(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::reblog_status(state, actor, id)
    })
    .await
}

pub async fn unreblog(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unreblog_status(state, actor, id)
    })
    .await
}

pub async fn bookmark(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::bookmark_status(state, actor, id)
    })
    .await
}

pub async fn unbookmark(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unbookmark_status(state, actor, id)
    })
    .await
}

/// `POST /web/statuses/{id}/translate` — the per-post Translate control.
/// Plain form submits 303 to the thread permalink with `?translate=1` (the
/// server-rendered translated view); the JS enhancement sends
/// `X-Requested-With: fetch` and gets the translation as JSON to swap in
/// place. Both paths go through the same cached `translate_status`.
pub async fn translate(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let viewer = user.current.account.id;
    let stored = match status::find_by_id(&state.pool, id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    match crate::entities::can_view(&state.pool, &stored, Some(viewer)).await {
        Ok(true) => {}
        Ok(false) => return ApiError::NotFound.into_response(),
        Err(err) => return err.into_response(),
    }
    let permalink = match account::find_by_id(&state.pool, stored.account_id).await {
        Ok(Some(author)) => format!(
            "/@{}/{id}",
            crate::entities::account_acct(&state.config.domain, &author)
        ),
        _ => "/".to_owned(),
    };
    if headers
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        != Some("fetch")
    {
        return redirect_to(&format!("{permalink}?translate=1"));
    }

    let settings = match user::settings_by_user_id(&state.pool, user.current.user.id).await {
        Ok(settings) => settings.unwrap_or_default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let target = settings.translate_language();
    let translation =
        match crate::translation::translate_status(&state, &stored, target, viewer).await {
            Ok(translation) => translation,
            Err(err) => return err.into_response(),
        };
    let text = |key: &str| {
        translation
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    // The page renders the *folded* content (title heading + body + external
    // link pill), so the in-place swap gets the same shape — a translated
    // title rides inside `content` and the JS stays a plain innerHTML swap.
    let folded_content = {
        let translated_title = Some(text("title")).filter(|t| !t.is_empty());
        let title = translated_title.or(stored.title.as_deref());
        crate::entities::fold_typed_content(text("content"), title, stored.external_url.as_deref())
    };
    let provider = translation
        .get("provider")
        .and_then(Value::as_str)
        .map_or_else(
            || user.locale.text("status-translation-service"),
            str::to_owned,
        );
    let source = translation
        .get("detected_source_language")
        .and_then(Value::as_str)
        .map(crate::web::view::language_label);
    let mut attribution_args = FluentArgs::new();
    attribution_args.set("provider", provider.as_str());
    if let Some(language) = source.as_deref() {
        attribution_args.set("language", language);
    }
    let attribution = user.locale.text_with(
        if source.is_some() {
            "status-translated-from"
        } else {
            "status-translated"
        },
        &attribution_args,
    );
    let poll_options: Vec<&str> = translation
        .get("poll")
        .and_then(|poll| poll.get("options"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.get("title").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    axum::Json(serde_json::json!({
        "content": folded_content,
        "spoiler_text": text("spoiler_text"),
        "poll_options": poll_options,
        "media_attachments": translation.get("media_attachments").cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "attribution": attribution,
    }))
    .into_response()
}

pub async fn pin(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::pin_status(state, actor, id)
    })
    .await
}

pub async fn unpin(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unpin_status(state, actor, id)
    })
    .await
}

/// `POST /web/statuses/{id}/mute` — mutes the status' conversation.
pub async fn mute(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::mute_conversation(state, actor, id)
    })
    .await
}

/// `POST /web/statuses/{id}/unmute`.
pub async fn unmute(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unmute_conversation(state, actor, id)
    })
    .await
}

/// The Private-mentions row verbs: `POST /web/conversations/{id}/read`,
/// `/unread` and `/remove` — the web face of the conversations API, keyed on
/// the viewer's own `account_conversations` row id (owner-scoped, 404 for
/// anyone else's).
async fn act_on_conversation(
    state: &AppState,
    user: &WebUser,
    form: &ActionForm,
    row_id: i64,
    verb: &str,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/conversations");
    let account_id = user.current.account.id;
    let outcome = match verb {
        "read" => conversation::set_unread(&state.pool, account_id, row_id, false)
            .await
            .map(|row| row.is_some()),
        "unread" => conversation::set_unread(&state.pool, account_id, row_id, true)
            .await
            .map(|row| row.is_some()),
        _ => conversation::delete_account_conversation(&state.pool, account_id, row_id).await,
    };
    match outcome {
        Ok(true) => redirect_to(&back),
        Ok(false) => ApiError::NotFound.into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

pub async fn conversation_read(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_conversation(&state, &user, &form, id, "read").await
}

pub async fn conversation_unread(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_conversation(&state, &user, &form, id, "unread").await
}

pub async fn conversation_remove(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_conversation(&state, &user, &form, id, "remove").await
}

/// `POST /web/statuses/{id}/react/{emoji}` — adds a Pleroma emoji reaction
/// (the web face of `PUT /api/v1/pleroma/statuses/{id}/reactions/{emoji}`).
pub async fn react(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, emoji)): Path<(i64, String)>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::react_with_emoji(state, actor, id, &emoji)
    })
    .await
}

/// `POST /web/statuses/{id}/react` — applies the submit button selected on
/// the full no-JavaScript picker page.
pub async fn react_selected(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<reactions::PickerForm>,
) -> Response {
    let action_form = ActionForm {
        csrf: form.csrf,
        return_to: form.return_to,
    };
    act_on_status(&state, &user, &action_form, |state, actor| {
        actions::react_with_emoji(state, actor, id, &form.emoji)
    })
    .await
}

/// `POST /web/statuses/{id}/unreact/{emoji}`.
pub async fn unreact(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, emoji)): Path<(i64, String)>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::unreact_with_emoji(state, actor, id, &emoji)
    })
    .await
}

/// `POST /web/statuses/{id}/quotes/{quote_id}/revoke` — the quoted author
/// withdraws a quote from the web quotes list. `quote_id` is the *quoting*
/// status' id, like the API endpoint.
pub async fn revoke_quote(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, quote_id)): Path<(i64, i64)>,
    Form(form): Form<ActionForm>,
) -> Response {
    act_on_status(&state, &user, &form, |state, actor| {
        actions::revoke_quote(state, actor, id, quote_id)
    })
    .await
}

/// The overflow menu's "who can quote" form.
#[derive(Deserialize)]
pub struct QuotePolicyForm {
    csrf: String,
    return_to: Option<String>,
    policy: String,
}

/// `POST /web/statuses/{id}/quote_policy` — changes who may quote one's own
/// post after the fact (the web face of `PATCH interaction_policy`).
pub async fn quote_policy(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<QuotePolicyForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    let Some(policy) = plamenu_ap::quote_policy::from_client_string(&form.policy) else {
        return (StatusCode::BAD_REQUEST, "invalid quote policy").into_response();
    };
    if let Err(err) =
        actions::set_interaction_policy(&state, &user.current.account, id, policy).await
    {
        return err.into_response();
    }
    redirect_to(&back)
}

/// Builds the read-only card for a prepared edit. Mutable fields come from the
/// dry-run entity; immutable context (poll, quote, event and group attribution)
/// is copied from the currently rendered status so editing never makes those
/// parts disappear from Preview.
async fn build_edit_preview_card(
    state: &AppState,
    user: &WebUser,
    mut prepared: actions::PreparedStatusEdit,
) -> Result<(Markup, Vec<Value>), ApiError> {
    let current = crate::entities::render_status(
        &state.pool,
        &state.config.domain,
        &prepared.stored,
        Some(user.current.account.id),
    )
    .await?;
    let quote_is_embedded = current
        .get("quote")
        .and_then(Value::as_object)
        .is_some_and(|quote| {
            quote.get("state").and_then(Value::as_str) == Some("accepted")
                && quote
                    .get("quoted_status")
                    .is_some_and(|status| !status.is_null())
        });
    // Save stores the quote compatibility link. The ordinary entity renderer
    // hides it only when an accepted native quote card is actually embedded.
    if prepared.quote_row.is_some() && !quote_is_embedded {
        prepared.composed.html.clone_from(&prepared.html);
    }

    let media_ids: Vec<i64> = prepared.kept_media.iter().map(|item| item.id).collect();
    let has_poll = current.get("poll").is_some_and(|poll| !poll.is_null());
    let kind = plamenu_ap::activity::PostKind::of_stored(
        prepared.stored.object_type.as_deref(),
        has_poll,
        prepared.effective_title.is_some(),
    );
    let mut value = crate::entities::preview_status_json(
        state,
        &user.current.account,
        &prepared.composed,
        crate::entities::PreviewStatusOpts {
            spoiler_text: &prepared.spoiler_text,
            sensitive: prepared.sensitive,
            visibility: &prepared.stored.visibility,
            language: prepared.language.as_deref(),
            in_reply_to_id: prepared.stored.in_reply_to_id,
            quote_approval_policy: prepared.quote_approval_policy,
            media_ids: &media_ids,
            title: prepared.effective_title.as_deref(),
            external_url: prepared.stored.external_url.as_deref(),
            kind,
        },
    )
    .await?;

    // The preview serializer reads current media rows. Overlay submitted alt
    // text in memory so Preview shows what Save changes would publish without
    // updating the attachment itself.
    if let Some(media) = value
        .get_mut("media_attachments")
        .and_then(Value::as_array_mut)
    {
        for attrs in &prepared.media_attributes {
            let id = attrs.id.to_string();
            let Some(item) = media
                .iter_mut()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
            else {
                continue;
            };
            if let Some(description) = &attrs.description {
                item["description"] = if description.is_empty() {
                    Value::Null
                } else {
                    Value::String(description.clone())
                };
            }
        }
    }

    for key in [
        "in_reply_to_account_id",
        "quote",
        "poll",
        "groups",
        "event",
        "group_post",
        "group_locked",
        "downvotes_count",
        "downvoted",
    ] {
        if let Some(context) = current.get(key) {
            value[key] = context.clone();
        }
    }
    // Link-preview cards survive media/alt-only edits, but a changed body is
    // recrawled after Save and must not show the old URL's stale card.
    if prepared.html == prepared.stored.content
        && let Some(card) = current.get("card")
    {
        value["card"] = card.clone();
    }
    let media = value
        .get("media_attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let card = pages::render_preview_card(state, user, &value).await?;
    Ok((card, media))
}

struct EditPreviewPage<'a> {
    pairs: &'a [(String, String)],
    sensitive: bool,
    media: Option<&'a [Value]>,
    preview: Option<Markup>,
    error: Option<&'a str>,
}

async fn render_edit_preview_page(
    state: &AppState,
    user: &WebUser,
    stored: &status::Status,
    page: EditPreviewPage<'_>,
) -> Response {
    let source = match status::source_of(&state.pool, stored.id).await {
        Ok(Some(source)) => source,
        Ok(None) => status::StatusSource::default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let text = field(page.pairs, "status").unwrap_or(&source.text).trim();
    let spoiler_text = field(page.pairs, "spoiler_text")
        .unwrap_or(&stored.spoiler_text)
        .trim();
    let content_type = field(page.pairs, "content_type")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&source.content_type);
    let language = field(page.pairs, "language").filter(|value| !value.trim().is_empty());
    let quote_policy = field(page.pairs, "quote_policy").filter(|value| !value.trim().is_empty());
    match pages::render_edit_composer(
        state,
        user,
        stored,
        pages::EditComposerEcho {
            text,
            spoiler_text,
            sensitive: page.sensitive,
            language,
            content_type,
            quote_policy,
            media: page.media,
            preview: page.preview,
            error: page.error,
        },
    )
    .await
    {
        Ok(markup) => markup.into_response(),
        Err(err) => err.into_response(),
    }
}

/// Rebuilds the edit form's attachment cards from submitted keep/alt fields
/// after preview validation failed. This is presentation-only: media rows are
/// read and their draft descriptions are overlaid on JSON in memory.
async fn edit_media_echo(
    state: &AppState,
    user: &WebUser,
    media_ids: &[i64],
    pairs: &[(String, String)],
) -> Result<Vec<Value>, ApiError> {
    let mut media = crate::entities::preview_media_json(
        &state.pool,
        &state.config.domain,
        media_ids,
        Some(user.current.account.id),
    )
    .await?;
    for item in &mut media {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(description) = field(pairs, &format!("media_alt_{id}")) else {
            continue;
        };
        item["description"] = if description.trim().is_empty() {
            Value::Null
        } else {
            Value::String(description.trim().to_owned())
        };
    }
    Ok(media)
}

/// `POST /web/statuses/{id}/edit` — previews or applies the edit composer:
/// text, content warning, sensitivity, language, quote policy, and the kept
/// attachments with their alt text. Preview is a dry run through the same
/// preparation as `edit_status`; Save lands back on the post's thread.
#[allow(clippy::too_many_lines)] // parse once, then branch between dry-run rendering and save
pub async fn edit(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body);
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let mut media_keep = Vec::new();
    for (key, value) in &pairs {
        if key == "media_keep[]" || key == "media_keep" {
            match value.trim().parse::<i64>() {
                Ok(media_id) => media_keep.push(media_id),
                Err(_) => return (StatusCode::BAD_REQUEST, "invalid attachment").into_response(),
            }
        }
    }
    let media_attributes = media_keep
        .iter()
        .map(|media_id| actions::MediaEditAttributes {
            id: *media_id,
            description: field(&pairs, &format!("media_alt_{media_id}")).map(str::to_owned),
            focus: None,
        })
        .collect();
    // The sensitive checkbox rides over a hidden `false`, so the last value
    // wins; no field at all (a post without media) keeps the current flag.
    let sensitive = pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "sensitive")
        .map(|(_, v)| truthy(v));
    let quote_approval_policy = match field(&pairs, "quote_policy") {
        Some(policy) => match plamenu_ap::quote_policy::from_client_string(policy) {
            Some(resolved) => Some(resolved),
            None => return (StatusCode::BAD_REQUEST, "invalid quote policy").into_response(),
        },
        None => None,
    };
    let params = actions::EditParams {
        // The edit form carries the headline of a titled post (long-form or a
        // group thread); an untitled post submits no such field.
        title: field(&pairs, "title").filter(|t| !t.trim().is_empty()),
        text: Some(field(&pairs, "status").unwrap_or_default().trim()),
        content_type: field(&pairs, "content_type")
            .filter(|value| !value.trim().is_empty())
            .map(crate::compose::PostFormat::from_media_type),
        spoiler_text: Some(field(&pairs, "spoiler_text").unwrap_or_default().trim()),
        sensitive,
        language: field(&pairs, "language").filter(|l| !l.trim().is_empty()),
        media_ids: Some(media_keep.clone()),
        quote_approval_policy,
        media_attributes,
        // Event edits ride their own action (`/web/statuses/{id}/event`), so
        // the ordinary edit form never rewrites event fields by omission.
        event: None,
    };

    if field(&pairs, "op") == Some("preview") {
        let fragment = headers.contains_key("x-compose-preview");
        let stored = match status::find_local(&state.pool, user.current.account.id, id).await {
            Ok(Some(stored)) if stored.reblog_of_id.is_none() => stored,
            Ok(_) => return ApiError::NotFound.into_response(),
            Err(err) => return ApiError::from(err).into_response(),
        };
        return match actions::prepare_status_edit(
            &state,
            &user.current.account,
            stored.clone(),
            params,
        )
        .await
        {
            Ok(prepared) => {
                let sensitive = prepared.sensitive;
                match build_edit_preview_card(&state, &user, prepared).await {
                    Ok((card, _)) if fragment => html! {
                        h2.compose__preview-heading { (user.locale.text("compose-preview")) }
                        (card)
                    }
                    .into_response(),
                    Ok((card, media)) => {
                        render_edit_preview_page(
                            &state,
                            &user,
                            &stored,
                            EditPreviewPage {
                                pairs: &pairs,
                                sensitive,
                                media: Some(&media),
                                preview: Some(card),
                                error: None,
                            },
                        )
                        .await
                    }
                    Err(err) => err.into_response(),
                }
            }
            Err(err) => match banner_message(&err) {
                Some(message) if fragment => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    html! { p.compose__error role="alert" { (message) } },
                )
                    .into_response(),
                Some(message) => {
                    let media = match edit_media_echo(&state, &user, &media_keep, &pairs).await {
                        Ok(media) => media,
                        Err(err) => return err.into_response(),
                    };
                    render_edit_preview_page(
                        &state,
                        &user,
                        &stored,
                        EditPreviewPage {
                            pairs: &pairs,
                            sensitive: sensitive.unwrap_or(stored.sensitive),
                            media: Some(&media),
                            preview: None,
                            error: Some(&message),
                        },
                    )
                    .await
                }
                None => err.into_response(),
            },
        };
    }

    let result = actions::edit_status(&state, &user.current.account, id, params).await;
    match result {
        Ok(_) => redirect_to(&format!("/@{}/{id}", user.current.account.username)),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/statuses/{id}/report` — files the report the form at the same
/// URL collects through the same `create_report` the API's
/// `POST /api/v1/reports` uses, then lands on the confirmation view. The
/// same gates as the form page: boost wrappers, own posts and posts the
/// viewer can't see 404.
pub async fn report(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body);
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let stored = match status::find_by_id(&state.pool, id).await {
        Ok(Some(s)) if s.reblog_of_id.is_none() && s.account_id != user.current.account.id => s,
        Ok(_) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    match crate::entities::can_view(&state.pool, &stored, Some(user.current.account.id)).await {
        Ok(true) => {}
        Ok(false) => return ApiError::NotFound.into_response(),
        Err(err) => return err.into_response(),
    }
    let target = match account::find_by_id(&state.pool, stored.account_id).await {
        Ok(Some(target)) => target,
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let mut status_ids = Vec::new();
    let mut rule_ids = Vec::new();
    for (key, value) in &pairs {
        let ids = match key.as_str() {
            "status_ids[]" => &mut status_ids,
            "rule_ids[]" => &mut rule_ids,
            _ => continue,
        };
        match value.parse::<i64>() {
            Ok(parsed) => ids.push(parsed),
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid id").into_response(),
        }
    }
    let result = actions::create_report(
        &state,
        &user.current.account,
        &target,
        actions::ReportParams {
            comment: field(&pairs, "comment").unwrap_or_default(),
            category: field(&pairs, "category"),
            forward: field(&pairs, "forward").is_some_and(truthy),
            status_ids: &status_ids,
            rule_ids: (!rule_ids.is_empty()).then_some(rule_ids.as_slice()),
        },
    )
    .await;
    match result {
        Ok(_) => {
            let back = safe_return(field(&pairs, "return_to"), "/");
            let query = serde_urlencoded::to_string([("done", "1"), ("return_to", &back)])
                .unwrap_or_default();
            redirect_to(&format!("/web/statuses/{id}/report?{query}"))
        }
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/report` — files a report against the account from
/// its profile (no single post leads it), through the same `create_report` as
/// the status and API paths, then lands on the shared confirmation view.
/// Reporting oneself 404s.
pub async fn report_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body);
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let target = match account::find_by_id(&state.pool, id).await {
        Ok(Some(target)) if target.id != user.current.account.id => target,
        Ok(_) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let mut status_ids = Vec::new();
    let mut rule_ids = Vec::new();
    for (key, value) in &pairs {
        let ids = match key.as_str() {
            "status_ids[]" => &mut status_ids,
            "rule_ids[]" => &mut rule_ids,
            _ => continue,
        };
        match value.parse::<i64>() {
            Ok(parsed) => ids.push(parsed),
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid id").into_response(),
        }
    }
    let result = actions::create_report(
        &state,
        &user.current.account,
        &target,
        actions::ReportParams {
            comment: field(&pairs, "comment").unwrap_or_default(),
            category: field(&pairs, "category"),
            forward: field(&pairs, "forward").is_some_and(truthy),
            status_ids: &status_ids,
            rule_ids: (!rule_ids.is_empty()).then_some(rule_ids.as_slice()),
        },
    )
    .await;
    match result {
        Ok(_) => {
            let back = safe_return(field(&pairs, "return_to"), "/");
            let query = serde_urlencoded::to_string([("done", "1"), ("return_to", &back)])
                .unwrap_or_default();
            redirect_to(&format!("/web/accounts/{id}/report?{query}"))
        }
        Err(err) => err.into_response(),
    }
}

/// `POST /web/statuses/{id}/redraft` — Mastodon's delete-and-redraft: the
/// post is deleted and the composer reopens prefilled with its source text,
/// content warning, visibility and reply target.
pub async fn redraft(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let stored = match status::find_local(&state.pool, user.current.account.id, id).await {
        Ok(Some(stored)) if stored.reblog_of_id.is_none() => stored,
        Ok(_) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    // The source must be read before the delete cascades it away.
    let source = match status::source_of(&state.pool, stored.id).await {
        Ok(source) => source.unwrap_or_default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) =
        actions::delete_status(&state, &user.current.account, id, actions::DeleteMode::Stub).await
    {
        return err.into_response();
    }
    let mut params = vec![("text", source.text)];
    if !stored.spoiler_text.is_empty() {
        params.push(("cw", stored.spoiler_text));
    }
    params.push(("visibility", stored.visibility));
    params.push(("format", source.content_type));
    if let Some(parent) = stored.in_reply_to_id {
        params.push(("reply", parent.to_string()));
    }
    let query = serde_urlencoded::to_string(&params).unwrap_or_default();
    redirect_to(&format!("/compose?{query}"))
}

/// CSRF-checks the form, runs a status action, then redirects back. The action
/// closure receives the borrowed state and actor and returns the action's own
/// future, so each toggle is a one-liner.
async fn act_on_status<'a, F, Fut>(
    state: &'a AppState,
    user: &'a WebUser,
    form: &ActionForm,
    action: F,
) -> Response
where
    F: FnOnce(&'a AppState, &'a Account) -> Fut,
    Fut: Future<Output = Result<plamenu_db::status::Status, ApiError>>,
{
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    if let Err(err) = action(state, &user.current.account).await {
        return err.into_response();
    }
    redirect_to(&back)
}

/// `POST /web/accounts/{id}/follow`.
pub async fn follow(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::follow_account(&state, &user.current.account, &target).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

#[derive(Deserialize)]
pub struct RemoteHistoryForm {
    csrf: String,
    return_to: Option<String>,
    mode: String,
}

/// Explicit remote-history request from a profile. HTML clients follow PRG;
/// the JS enhancement gets JSON and polls the local state endpoint.
pub async fn remote_history(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    headers: axum::http::HeaderMap,
    Form(form): Form<RemoteHistoryForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    let target = match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(error) => return ApiError::from(error).into_response(),
    };
    let kind = match form.mode.as_str() {
        "initial" => plamenu_db::remote_history::JobKind::Initial,
        "refresh" => plamenu_db::remote_history::JobKind::Refresh,
        "older" => plamenu_db::remote_history::JobKind::Older,
        _ => return ApiError::Unprocessable("invalid history mode".into()).into_response(),
    };
    let outcome =
        match crate::remote_history::request(&state, &target, kind, Some(user.current.account.id))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return error.into_response(),
        };
    if headers
        .get("x-requested-with")
        .and_then(|value| value.to_str().ok())
        == Some("fetch")
    {
        let state_name = match outcome {
            plamenu_db::remote_history::EnqueueOutcome::Enqueued
            | plamenu_db::remote_history::EnqueueOutcome::Coalesced => "queued",
            plamenu_db::remote_history::EnqueueOutcome::Fresh => "fresh",
            plamenu_db::remote_history::EnqueueOutcome::Disabled => "disabled",
            plamenu_db::remote_history::EnqueueOutcome::Backoff(_)
            | plamenu_db::remote_history::EnqueueOutcome::AutomaticCooldown(_)
            | plamenu_db::remote_history::EnqueueOutcome::OriginBusy(_) => "backoff",
            plamenu_db::remote_history::EnqueueOutcome::RateLimited(_) => "rate_limited",
        };
        return axum::Json(serde_json::json!({"state": state_name})).into_response();
    }
    redirect_to(&back)
}

/// Session-authenticated local state used by the profile enhancement. No
/// cursor or remote URL is exposed.
pub async fn remote_history_state(
    State(state): State<AppState>,
    _user: WebUser,
    Path(account_id): Path<i64>,
) -> Response {
    match plamenu_db::remote_history::snapshot(&state.pool, account_id).await {
        Ok(Some(snapshot)) => axum::Json(serde_json::json!({
            "enabled": snapshot.hydration_enabled,
            "state": snapshot.state,
            "available_statuses": snapshot.available_statuses,
            "last_success_at": snapshot.last_success_at,
        }))
        .into_response(),
        Ok(None) => ApiError::NotFound.into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `POST /web/accounts/{id}/unfollow`.
pub async fn unfollow(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::unfollow_account(&state, &user.current.account, &target).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/follow_settings` — the per-follow settings
/// (M32 plus the replies switch): notify on new posts, show boosts, show
/// replies, language filter. The checkboxes are absent-when-off and the
/// languages select always submits, so the form replaces all four settings
/// each time. Not following is a silent no-op (the profile only renders the
/// form while a follow edge exists).
pub async fn follow_settings(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body);
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let back = safe_return(field(&pairs, "return_to"), "/");
    let show_reblogs = field(&pairs, "show_reblogs").is_some_and(truthy);
    let with_replies = field(&pairs, "with_replies").is_some_and(truthy);
    let notify = field(&pairs, "notify").is_some_and(truthy);
    let languages: Vec<String> = pairs
        .iter()
        .filter(|(key, code)| key == "languages" && actions::is_language_code(code))
        .map(|(_, code)| code.clone())
        .collect();
    match plamenu_db::follow::update_settings(
        &state.pool,
        user.current.account.id,
        account_id,
        Some(show_reblogs),
        Some(with_replies),
        Some(notify),
        Some(&languages),
    )
    .await
    {
        Ok(_) => redirect_to(&back),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/tags/{tag}/follow` — follow a hashtag: its public posts
/// join the home timeline. Purely local, like the API; creates the tag row if
/// it has never been used here.
pub async fn tag_follow(
    State(state): State<AppState>,
    user: WebUser,
    Path(name): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let Some(name) = crate::routes::tags::normalize_hashtag(&name) else {
        return ApiError::NotFound.into_response();
    };
    let back = safe_return(form.return_to.as_deref(), &format!("/tags/{name}"));
    let result = async {
        let tag_id = plamenu_db::tag::ensure(&state.pool, &name).await?;
        plamenu_db::tag::follow(&state.pool, user.current.account.id, tag_id).await
    }
    .await;
    match result {
        Ok(()) => redirect_to(&back),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/tags/{tag}/unfollow` — idempotent; a never-followed or unknown
/// tag just redirects back.
pub async fn tag_unfollow(
    State(state): State<AppState>,
    user: WebUser,
    Path(name): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let Some(name) = crate::routes::tags::normalize_hashtag(&name) else {
        return ApiError::NotFound.into_response();
    };
    let back = safe_return(form.return_to.as_deref(), &format!("/tags/{name}"));
    let result = async {
        match plamenu_db::tag::find_by_name(&state.pool, &name).await? {
            Some(tag) => plamenu_db::tag::unfollow(&state.pool, user.current.account.id, tag.id)
                .await
                .map(|_| ()),
            None => Ok(()),
        }
    }
    .await;
    match result {
        Ok(()) => redirect_to(&back),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/suggestions/{id}/dismiss` — dismiss a follow suggestion so it
/// never resurfaces, mirroring `DELETE /api/v1/suggestions/{id}`.
pub async fn dismiss_suggestion(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/people");
    match plamenu_db::suggestion::suppress(&state.pool, user.current.account.id, account_id).await {
        Ok(()) => redirect_to(&back),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The private-note form's fields — [`ActionForm`] plus the note text.
#[derive(Deserialize)]
pub struct NoteForm {
    csrf: String,
    return_to: Option<String>,
    comment: Option<String>,
}

/// `POST /web/accounts/{id}/note` — saves the viewer's private note about an
/// account. API semantics: a whitespace-only comment clears the note,
/// anything else is stored verbatim.
pub async fn account_note(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<NoteForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    let target = match load_target(&state, account_id).await {
        Ok(Some(target)) => target,
        Ok(None) => return redirect_to(&back),
        Err(err) => return err.into_response(),
    };
    let comment = form.comment.as_deref().unwrap_or("");
    let stored = if comment.trim().is_empty() {
        ""
    } else {
        comment
    };
    match plamenu_db::account_note::set(&state.pool, user.current.account.id, target.id, stored)
        .await
    {
        Ok(()) => redirect_to(&back),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/accounts/{id}/mute` — mutes the account (posts *and*
/// notifications, indefinitely — the API's defaults).
pub async fn mute_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::mute_account(&state, &user.current.account, &target, true, 0).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/block`.
pub async fn block_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::block_account(&state, &user.current.account, &target).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/unmute`.
pub async fn unmute_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::unmute_account(&state, &user.current.account, &target).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/unblock`.
pub async fn unblock_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match actions::unblock_account(&state, &user.current.account, &target).await {
                Ok(()) => redirect_to(&back),
                Err(err) => err.into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/endorse` — pins the account to the viewer's own
/// profile ("Featured on profile", Mastodon's `account_pins`). Purely local,
/// so no federation; mirrors `routes::accounts_api::endorse`.
pub async fn endorse(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match plamenu_db::endorsement::endorse(&state.pool, user.current.account.id, target.id)
                .await
            {
                Ok(()) => redirect_to(&back),
                Err(err) => ApiError::from(err).into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/accounts/{id}/unendorse`.
pub async fn unendorse(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    match load_target(&state, account_id).await {
        Ok(Some(target)) => {
            match plamenu_db::endorsement::unendorse(
                &state.pool,
                user.current.account.id,
                target.id,
            )
            .await
            {
                Ok(()) => redirect_to(&back),
                Err(err) => ApiError::from(err).into_response(),
            }
        }
        Ok(None) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// The overflow menu's domain-block form.
#[derive(Deserialize)]
pub struct DomainBlockForm {
    csrf: String,
    return_to: Option<String>,
    domain: Option<String>,
}

/// `POST /web/domains/block` — blocks a whole remote server for the viewer
/// (the web face of `POST /api/v1/domain_blocks`).
pub async fn block_domain(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<DomainBlockForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    let domain = match crate::routes::domain_blocks::normalize_domain(form.domain.as_deref()) {
        Ok(domain) => domain,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = actions::block_domain(&state, &user.current.account, &domain).await {
        return err.into_response();
    }
    redirect_to(&back)
}

/// `POST /web/domains/unblock` — reverses a viewer's domain block (the web face
/// of `DELETE /api/v1/domain_blocks`).
pub async fn unblock_domain(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<DomainBlockForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/");
    let domain = match crate::routes::domain_blocks::normalize_domain(form.domain.as_deref()) {
        Ok(domain) => domain,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = actions::unblock_domain(&state, &user.current.account, &domain).await {
        return err.into_response();
    }
    redirect_to(&back)
}

async fn load_target(state: &AppState, account_id: i64) -> Result<Option<Account>, ApiError> {
    Ok(account::find_by_id(&state.pool, account_id).await?)
}
