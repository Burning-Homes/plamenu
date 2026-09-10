//! Personal custom emoji management for the built-in web client.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::custom_emoji::{self, ManagedCustomEmoji, PersonalCreateOutcome};
use plamenu_db::role::permission;
use plamenu_db::{account, poll, reaction, status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::session::{WebUser, csrf_rejection};
use crate::AppState;
use crate::media_processing::validate_emoji_image;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    saved: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BorrowQuery {
    error: Option<String>,
}

/// Authenticated catalog used by the built-in composer and reaction pickers.
/// The public Mastodon-compatible endpoint intentionally remains the
/// instance-wide catalog until an interoperable personal-emoji API exists.
pub async fn catalog(
    State(state): State<AppState>,
    user: WebUser,
) -> Result<Json<Value>, Response> {
    let listed = custom_emoji::listed_for_account(&state.pool, user.current.account.id)
        .await
        .map_err(api_err)?;
    let entities: Vec<Value> = listed
        .iter()
        .map(|(emoji, personal)| {
            let mut entity = crate::emoji::custom_emoji_json(&state.config.domain, emoji, false);
            entity["_personal"] = json!(personal);
            entity
        })
        .collect();
    Ok(Json(json!(entities)))
}

fn error_message(code: Option<&str>) -> Option<&'static str> {
    match code {
        Some("permission") => Some("Your role does not allow uploading or borrowing custom emoji."),
        Some("limit") => Some(
            "You have reached this server's personal custom emoji limit. Delete one before adding another.",
        ),
        Some("shortcode") => Some(
            "That shortcode is already used by one of your custom emoji. Choose a different shortcode.",
        ),
        Some("origin") => {
            Some("That emoji is already in your personal collection or is available instance-wide.")
        }
        Some("immutable") => Some(
            "Borrowed emoji cannot be renamed or recategorized. You can delete it and borrow it again.",
        ),
        Some("missing") => Some("That custom emoji no longer exists."),
        Some("invalid-shortcode") => {
            Some("Use 2–128 letters, numbers, or underscores for the shortcode.")
        }
        Some("borrow") => Some(
            "The custom emoji could not be borrowed. Return to its source and try again; if this continues, contact an administrator.",
        ),
        _ => None,
    }
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, Response> {
    let emojis = custom_emoji::list_personal(&state.pool, user.current.account.id)
        .await
        .map_err(api_err)?;
    let emoji_settings = custom_emoji::settings(&state.pool).await.map_err(api_err)?;
    let limit = emoji_settings.personal_limit;
    let can_add = user.can(permission::UPLOAD_CUSTOM_EMOJIS);
    let body = html! {
        div.settings-stack {
            @if query.saved.is_some() {
                p.settings__saved role="status" { "Custom emoji saved." }
            }
            @if let Some(message) = error_message(query.error.as_deref()) {
                p.settings__error role="alert" { (message) }
            }
            p.custom-emoji__lead {
                "Personal emoji appear before server emoji in composers and reaction pickers. "
                @if limit == 0 { "This server has no personal emoji limit." }
                @else { (emojis.len()) " of " (limit) " slots used." }
            }
            @if can_add {
                form.settings-form method="post" action="/web/settings/custom-emojis" enctype="multipart/form-data" {
                    input type="hidden" name="csrf" value=(&user.csrf);
                    fieldset.settings-form__group {
                        legend { "Upload a custom emoji" }
                        label.settings-field {
                            span.settings-field__label { "Shortcode" }
                            span.settings-field__hint { "Use 2–128 letters, numbers, or underscores." }
                            input type="text" name="shortcode" pattern="[A-Za-z0-9_]{2,128}" required;
                        }
                        label.settings-field {
                            span.settings-field__label { "Category" }
                            span.settings-field__hint { "Optional. This groups the emoji in your picker." }
                            input type="text" name="category" maxlength="100" placeholder="Uncategorized";
                        }
                        label.settings-field {
                            span.settings-field__label { "Image" }
                            span.settings-field__hint {
                                "PNG, GIF, or WebP, up to " (emoji_settings.max_file_size_kb) " KiB."
                            }
                            input type="file" name="image" accept="image/png,image/gif,image/webp" required;
                        }
                        div.settings-form__actions {
                            button type="submit" { "Upload" }
                        }
                    }
                }
            } @else {
                p.settings__error { "Your role cannot add new personal emoji. You can still manage or delete the ones already here." }
            }
            section.settings-stack__item.custom-emoji__collection {
                h3.settings-subhead { "Your custom emoji" }
                @if emojis.is_empty() { p.empty { "You have no personal custom emoji." } }
                @for emoji in &emojis { (personal_row(&state, &user, emoji)) }
            }
        }
    };
    Ok(super::settings::settings_shell(
        &user,
        "/settings/custom-emojis",
        "Custom emoji",
        &body,
    ))
}

fn personal_row(state: &AppState, user: &WebUser, emoji: &ManagedCustomEmoji) -> Markup {
    let image = crate::emoji::client_image_url(&state.config.domain, &emoji.as_emoji(), false);
    let edit_form_id = format!("personal-emoji-edit-{}", emoji.id);
    html! {
        article.admin-record.admin-emoji {
            div.admin-record__head {
                div.admin-emoji__identity {
                    img.admin-emoji__preview src=(image) alt=(format!(":{}:", emoji.shortcode));
                    div {
                        strong { ":" (emoji.shortcode) ":" }
                        @if emoji.borrowed { span.admin-badge { "Borrowed" } }
                        @if emoji.borrowed {
                            @if let Some(category) = &emoji.category {
                                span.admin-table__sub { (category) }
                            }
                        }
                    }
                }
            }
            @if emoji.borrowed {
                p.admin-record__stats { "Borrowed emoji keep their original name and category." }
            } @else {
                form.admin-emoji__moderation id=(&edit_form_id) method="post" action=(format!("/web/settings/custom-emojis/{}/update", emoji.id)) {
                    input type="hidden" name="csrf" value=(&user.csrf);
                    label.admin-emoji__field {
                        span { "Shortcode" }
                        input type="text" name="shortcode" value=(&emoji.shortcode) pattern="[A-Za-z0-9_]{2,128}" required;
                    }
                    label.admin-emoji__field {
                        span { "Category" }
                        input type="text" name="category" value=(emoji.category.as_deref().unwrap_or("")) maxlength="100" placeholder="Uncategorized";
                    }
                }
            }
            div.admin-actions.admin-emoji__actions {
                @if !emoji.borrowed {
                    button type="submit" form=(&edit_form_id) { "Save" }
                }
                form.settings-inline-form method="post" action=(format!("/web/settings/custom-emojis/{}/delete", emoji.id)) data-confirm="Delete this personal emoji? Existing posts keep their historical image." {
                    input type="hidden" name="csrf" value=(&user.csrf);
                    button.admin-danger type="submit" { "Delete" }
                }
            }
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    mut multipart: Multipart,
) -> Result<Response, Response> {
    require_add(&user)?;
    let mut csrf = String::new();
    let mut shortcode = String::new();
    let mut category = String::new();
    let mut image = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("The upload form could not be read: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "shortcode" => shortcode = field.text().await.unwrap_or_default(),
            "category" => category = field.text().await.unwrap_or_default(),
            "image" => {
                image = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(format!("The image upload was interrupted: {e}")))?
                    .to_vec();
            }
            _ => {}
        }
    }
    if !user.csrf_ok(&csrf) {
        return Err(csrf_rejection());
    }
    let shortcode = shortcode.trim();
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Ok(redirect("error=invalid-shortcode"));
    }
    if image.is_empty() {
        return Err(bad_request("Choose an image to upload.".into()));
    }
    let max_bytes = custom_emoji::settings(&state.pool)
        .await
        .map_err(api_err)?
        .max_file_size_bytes();
    let (content_type, extension) = validate_emoji_image(&image, max_bytes)
        .map_err(|e| bad_request(format!("The emoji image was rejected: {e}")))?;
    let file_name = format!("{}.{}", plamenu_db::id::next(), extension);
    let size = i64::try_from(image.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, image)
        .await
        .map_err(|e| bad_request(format!("The emoji image could not be stored: {e}")))?;
    let category = clean_category(&category);
    let result = match custom_emoji::create_personal_upload(
        &state.pool,
        user.current.account.id,
        shortcode,
        &file_name,
        content_type,
        size,
        category.as_deref(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let _ = state.media.delete(&file_name).await;
            return Err(bad_request(format!(
                "The personal emoji could not be recorded: {error}"
            )));
        }
    };
    finish_create(&state, &file_name, result).await
}

#[derive(Deserialize)]
pub struct UpdateForm {
    csrf: String,
    shortcode: String,
    category: String,
}

pub async fn update(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, Response> {
    if !user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let shortcode = form.shortcode.trim();
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Ok(redirect("error=invalid-shortcode"));
    }
    let Some(existing) = custom_emoji::find_managed_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .filter(|e| e.owner_account_id == Some(user.current.account.id) && !e.retired)
    else {
        return Ok(redirect("error=missing"));
    };
    if existing.borrowed {
        return Ok(redirect("error=immutable"));
    }
    if custom_emoji::list_personal(&state.pool, user.current.account.id)
        .await
        .map_err(api_err)?
        .iter()
        .any(|e| e.id != id && e.shortcode == shortcode)
    {
        return Ok(redirect("error=shortcode"));
    }
    custom_emoji::update_personal(
        &state.pool,
        user.current.account.id,
        id,
        shortcode,
        clean_category(&form.category).as_deref(),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect("saved=1"))
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

#[derive(Deserialize)]
pub struct BorrowForm {
    csrf: String,
    #[serde(default)]
    return_to: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    if !user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let retired = custom_emoji::retire_personal(&state.pool, user.current.account.id, id)
        .await
        .map_err(api_err)?;
    Ok(redirect(if retired { "saved=1" } else { "error=missing" }))
}

pub async fn borrow(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<BorrowForm>,
) -> Result<Response, Response> {
    let back = borrow_return(form.return_to.as_deref());
    let fail = |message: String| borrow_error(back.as_deref(), &message);
    if !user.can(permission::UPLOAD_CUSTOM_EMOJIS) {
        return Err(fail(
            "Your role does not allow uploading or borrowing custom emoji.".into(),
        ));
    }
    if !user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let source = custom_emoji::find_managed_by_id(&state.pool, id)
        .await
        .map_err(|e| fail(format!("The source emoji could not be checked: {e}")))?
        .filter(|e| !e.disabled && !e.retired)
        .ok_or_else(|| fail("That source emoji is no longer available.".into()))?;
    let bytes = if let Some(domain) = source.domain.as_deref() {
        // A remote row's local file is only a display-cache rendition and may
        // have been transcoded (for example, to AVIF). Borrow the canonical
        // asset advertised by its origin instead of copying that derivative.
        let url = source.image_remote_url.as_deref().ok_or_else(|| {
            fail("The source server did not advertise an image for this emoji.".into())
        })?;
        if plamenu_db::instance_policy::domain_rejects_media(&state.pool, domain)
            .await
            .map_err(|e| fail(format!("The server media policy could not be checked: {e}")))?
        {
            return Err(fail(format!(
                "Images from {domain} are blocked by this server's media policy."
            )));
        }
        state
            .federation
            .fetch_media(url)
            .await
            .map_err(|e| {
                fail(format!(
                    "The source server's emoji image could not be downloaded: {e}"
                ))
            })?
            .bytes
    } else if let Some(file) = &source.image_file_name {
        state
            .media
            .get(file)
            .await
            .map_err(|e| fail(format!("The source emoji image could not be read: {e}")))?
    } else {
        return Err(fail("The source emoji has no usable image.".into()));
    };
    let max_bytes = custom_emoji::settings(&state.pool)
        .await
        .map_err(|e| {
            fail(format!(
                "The custom emoji file limit could not be checked: {e}"
            ))
        })?
        .max_file_size_bytes();
    let (content_type, extension) = validate_emoji_image(&bytes, max_bytes)
        .map_err(|e| fail(format!("The borrowed emoji image was rejected: {e}")))?;
    let file_name = format!("{}.{}", plamenu_db::id::next(), extension);
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, bytes)
        .await
        .map_err(|e| fail(format!("The borrowed emoji image could not be stored: {e}")))?;
    let result = match custom_emoji::create_personal_borrow(
        &state.pool,
        user.current.account.id,
        &source,
        &file_name,
        content_type,
        size,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let _ = state.media.delete(&file_name).await;
            return Err(fail(format!(
                "The borrowed emoji could not be recorded: {error}"
            )));
        }
    };
    finish_create(&state, &file_name, result).await
}

pub async fn borrow_account(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<BorrowQuery>,
) -> Result<Markup, Response> {
    require_add(&user)?;
    let source = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| bad_request("That account no longer exists.".into()))?;
    let text = crate::entities::account_emojifiable_text(&source);
    let codes = crate::emoji::shortcodes_of(&[&text]);
    let emojis = if let Some(domain) = source.domain.as_deref() {
        custom_emoji::lookup(&state.pool, &codes, Some(domain))
            .await
            .map_err(api_err)?
    } else {
        custom_emoji::lookup_local_for_account(&state.pool, source.id, &codes)
            .await
            .map_err(api_err)?
    };
    borrow_page(
        &state,
        &user,
        "Emoji used on this profile",
        &format!("/settings/custom-emojis/borrow/account/{id}"),
        query.error.as_deref(),
        emojis,
    )
    .await
}

pub async fn borrow_status(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<BorrowQuery>,
) -> Result<Markup, Response> {
    require_add(&user)?;
    let mut item = status::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| bad_request("That post no longer exists.".into()))?;
    if let Some(target) = item.reblog_of_id {
        item = status::find_by_id(&state.pool, target)
            .await
            .map_err(api_err)?
            .ok_or_else(|| bad_request("The boosted post no longer exists.".into()))?;
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| bad_request("The post's author no longer exists.".into()))?;
    let found_poll = poll::find_by_status(&state.pool, item.id)
        .await
        .map_err(api_err)?;
    let mut text = vec![item.content.as_str(), item.spoiler_text.as_str()];
    if let Some(found_poll) = &found_poll {
        text.extend(found_poll.options.iter().map(String::as_str));
    }
    let codes = crate::emoji::shortcodes_of(&text);
    let mut emojis = if let Some(domain) = author.domain.as_deref() {
        custom_emoji::lookup(&state.pool, &codes, Some(domain))
            .await
            .map_err(api_err)?
    } else {
        custom_emoji::lookup_local_for_account(&state.pool, author.id, &codes)
            .await
            .map_err(api_err)?
    };
    let groups = reaction::for_statuses(&state.pool, &[item.id])
        .await
        .map_err(api_err)?
        .remove(&item.id)
        .unwrap_or_default();
    let urls: Vec<String> = groups
        .iter()
        .filter_map(|r| r.custom_emoji_url.clone())
        .collect();
    let reaction_ids: Vec<i64> = groups.iter().filter_map(|r| r.custom_emoji_id).collect();
    for managed in custom_emoji::find_managed_by_ids(&state.pool, &reaction_ids)
        .await
        .map_err(api_err)?
    {
        let emoji = managed.as_emoji();
        if !emojis.iter().any(|existing| existing.id == emoji.id) {
            emojis.push(emoji);
        }
    }
    for emoji in custom_emoji::find_by_remote_image_urls(&state.pool, &urls)
        .await
        .map_err(api_err)?
    {
        if !emojis.iter().any(|e| e.id == emoji.id) {
            emojis.push(emoji);
        }
    }
    borrow_page(
        &state,
        &user,
        "Emoji used in this post",
        &format!("/settings/custom-emojis/borrow/status/{id}"),
        query.error.as_deref(),
        emojis,
    )
    .await
}

async fn borrow_page(
    state: &AppState,
    user: &WebUser,
    title: &str,
    current: &str,
    error: Option<&str>,
    emojis: Vec<plamenu_db::custom_emoji::CustomEmoji>,
) -> Result<Markup, Response> {
    let ids: Vec<i64> = emojis.iter().map(|e| e.id).collect();
    let mut positions = HashMap::new();
    for (position, id) in ids.iter().enumerate() {
        positions.entry(*id).or_insert(position);
    }
    let mut managed = custom_emoji::find_managed_by_ids(&state.pool, &ids)
        .await
        .map_err(api_err)?;
    managed.retain(|emoji| !emoji.disabled && !emoji.retired);
    managed.sort_by_key(|emoji| positions.get(&emoji.id).copied().unwrap_or(usize::MAX));
    let mut origins = HashSet::new();
    managed.retain(|emoji| origins.insert(emoji.origin_id));
    let body = html! {
        div.settings-stack {
            (super::settings::error_flash(error))
            p.custom-emoji__lead { "Borrow custom emoji to add immutable copies to your collection. Each borrowed emoji counts toward your personal limit." }
            @if managed.is_empty() { p.empty { "No custom emoji are available from this source." } }
            @if !managed.is_empty() {
                section.settings-stack__item.custom-emoji__candidates {
                    @for emoji in &managed {
                        article.admin-record.admin-emoji {
                        div.admin-record__head {
                            div.admin-emoji__identity {
                                img.admin-emoji__preview src=(crate::emoji::client_image_url(&state.config.domain, &emoji.as_emoji(), false)) alt=(format!(":{}:", emoji.shortcode));
                                div {
                                    strong { ":" (&emoji.shortcode) ":" }
                                    @if let Some(category) = &emoji.category {
                                        span.admin-table__sub { (category) }
                                    }
                                }
                            }
                            form.settings-inline-form method="post" action=(format!("/web/settings/custom-emojis/{}/borrow", emoji.id)) {
                                input type="hidden" name="csrf" value=(&user.csrf);
                                input type="hidden" name="return_to" value=(current);
                                button type="submit" { "Borrow" }
                            }
                        }
                    }
                }
            }
        }
        }
    };
    Ok(super::settings::settings_shell(
        user,
        "/settings/custom-emojis",
        title,
        &body,
    ))
}

async fn finish_create(
    state: &AppState,
    file_name: &str,
    outcome: PersonalCreateOutcome,
) -> Result<Response, Response> {
    let destination = match outcome {
        PersonalCreateOutcome::Created(_) => "saved=1",
        PersonalCreateOutcome::LimitReached => "error=limit",
        PersonalCreateOutcome::ShortcodeTaken => "error=shortcode",
        PersonalCreateOutcome::OriginAlreadyOwned => "error=origin",
    };
    if !matches!(outcome, PersonalCreateOutcome::Created(_))
        && let Err(error) = state.media.delete(file_name).await
    {
        tracing::warn!(%error, %file_name, "failed to clean rejected personal emoji upload");
    }
    Ok(redirect(destination))
}

fn clean_category(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty()).then(|| value.chars().take(100).collect())
}

fn require_add(user: &WebUser) -> Result<(), Response> {
    if user.can(permission::UPLOAD_CUSTOM_EMOJIS) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "Your role does not allow uploading or borrowing custom emoji.",
        )
            .into_response())
    }
}

fn redirect(query: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/settings/custom-emojis?{query}"))],
    )
        .into_response()
}

/// Only the two pages that render borrow failures are valid return targets.
/// This keeps the form from becoming an open redirect or injecting a flash on
/// an unrelated local page when a client tampers with the hidden field.
fn borrow_return(path: Option<&str>) -> Option<String> {
    let path = path?;
    let id = path
        .strip_prefix("/settings/custom-emojis/borrow/account/")
        .or_else(|| path.strip_prefix("/settings/custom-emojis/borrow/status/"))?;
    id.parse::<i64>().ok().map(|_| path.to_owned())
}

fn borrow_error(return_to: Option<&str>, message: &str) -> Response {
    let Some(base) = return_to else {
        return redirect("error=borrow");
    };
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("{base}?{query}"))],
    )
        .into_response()
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}
fn api_err(error: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(error).into_response()
}
