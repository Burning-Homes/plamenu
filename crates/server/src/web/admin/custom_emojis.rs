//! Custom emoji management for the admin dashboard. This is the web
//! counterpart to the `plamenu emoji` CLI plus the flag controls Mastodon's
//! admin UI exposes. All pages require `MANAGE_CUSTOM_EMOJIS`.

use std::collections::{HashMap, HashSet};

use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use maud::{Markup, html};
use plamenu_db::custom_emoji::{self, CustomEmoji};
use plamenu_db::role::permission;
use plamenu_db::{account, poll, reaction, status};
use serde::Deserialize;
use serde_json::Value;

use super::{WebAdmin, admin_shell};
use crate::media_processing::validate_emoji_image;
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
    borrowed: Option<usize>,
    skipped: Option<usize>,
    failed: Option<usize>,
    reason: Option<String>,
}

/// `GET /admin/custom-emojis` — list local custom emoji and expose upload,
/// flag update and delete forms.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;

    let emojis = custom_emoji::list_local(&state.pool)
        .await
        .map_err(api_err)?;
    let emoji_settings = custom_emoji::settings(&state.pool).await.map_err(api_err)?;
    // Distinct categories, A→Z — the datalist behind every category input,
    // so existing names autocomplete instead of drifting apart on typos.
    let mut categories: Vec<&str> = emojis
        .iter()
        .filter_map(|e| e.category.as_deref())
        .collect();
    categories.sort_unstable();
    categories.dedup();
    // The listing groups like the picker: named categories A→Z, then the
    // uncategorized bucket last.
    let mut groups: Vec<(Option<&str>, Vec<&custom_emoji::CustomEmoji>)> = Vec::new();
    for category in &categories {
        groups.push((
            Some(category),
            emojis
                .iter()
                .filter(|e| e.category.as_deref() == Some(*category))
                .collect(),
        ));
    }
    let uncategorized: Vec<&custom_emoji::CustomEmoji> =
        emojis.iter().filter(|e| e.category.is_none()).collect();
    if !uncategorized.is_empty() {
        groups.push((None, uncategorized));
    }
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That emoji change could not be saved."))
        (borrow_result_banner(&query))
        (personal_controls(&admin))
        datalist #emoji-categories {
            @for category in &categories { option value=(category) {} }
        }
        section.admin-form {
            h3 { "Upload emoji" }
            form method="post" action="/web/admin/custom-emojis" enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-form__grid {
                    label {
                        "Shortcode"
                        input type="text" name="shortcode" pattern="[A-Za-z0-9_]{2,128}" required;
                    }
                    label {
                        "Category"
                        input type="text" name="category" maxlength="100"
                            list="emoji-categories" placeholder="Uncategorized";
                    }
                    label {
                        "Image"
                        input type="file" name="image" accept="image/png,image/gif,image/webp" required;
                        span.settings-field__hint {
                            "PNG, GIF or WebP, up to " (emoji_settings.max_file_size_kb) " KiB."
                        }
                    }
                }
                div.admin-actions {
                    button type="submit" { "Upload" }
                }
            }
        }
        section.admin-list {
            h3 { "Local emoji" }
            @if emojis.is_empty() {
                p.empty { "No local custom emoji." }
            }
            @for (category, members) in &groups {
                @if groups.len() > 1 || category.is_some() {
                    h4.admin-emoji__category { (category.unwrap_or("Uncategorized")) }
                }
                @for emoji in members {
                    (emoji_row(&state.config.domain, emoji, csrf))
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/custom-emojis", "Custom emoji", &body).into_response())
}

fn personal_controls(admin: &WebAdmin) -> Markup {
    html! {
        section.admin-form.admin-emoji-overview {
            div.admin-record__head {
                h3 { "Personal emoji" }
            }
            p.admin__lead {
                "Moderate a member's collection from their local account page, or review emoji gaining use across the server."
            }
            div.admin-actions {
                a.pill-button href="/admin/custom-emojis/trending" { "Review trending custom emoji" }
                @if admin.user.can(permission::MANAGE_USERS) {
                    a.pill-button href="/admin/accounts?origin=local" { "Find a local member" }
                }
                @if admin.user.can(permission::MANAGE_SETTINGS) {
                    a.pill-button href="/admin/settings?open=custom-emoji" { "Custom emoji settings" }
                }
            }
        }
    }
}

fn borrow_result_banner(query: &IndexQuery) -> Markup {
    let Some(borrowed) = query.borrowed else {
        return Markup::default();
    };
    let skipped = query.skipped.unwrap_or_default();
    let failed = query.failed.unwrap_or_default();
    html! {
        @if borrowed == 0 && failed > 0 {
            p.admin-flash.is-error role="alert" {
                (borrowed) " custom emoji imported."
                @if skipped > 0 { " " (skipped) " already existed and were skipped." }
                " " (failed) " could not be imported."
                @if let Some(reason) = &query.reason { " " (reason) }
            }
        } @else {
            p.admin-flash role="status" {
                (borrowed) " custom emoji imported."
                @if skipped > 0 { " " (skipped) " already existed and were skipped." }
                @if failed > 0 { " " (failed) " could not be imported." }
                @if let Some(reason) = &query.reason { " " (reason) }
            }
        }
    }
}

struct BorrowSource {
    label: String,
    href: String,
    domain: String,
    emojis: Vec<CustomEmoji>,
}

struct BorrowCandidate {
    emoji: CustomEmoji,
    category: String,
    category_from_source: bool,
    already_local: bool,
}

/// `GET /admin/custom-emojis/borrow/account/{id}` — extract the custom emoji
/// referenced by a remote account's display name, bio and profile fields.
pub async fn borrow_account_index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    let account = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(not_found)?;
    let Some(domain) = account.domain.clone() else {
        return Err(not_found());
    };
    let text = crate::entities::account_emojifiable_text(&account);
    let emojis = custom_emoji::lookup(
        &state.pool,
        &crate::emoji::shortcodes_of(&[&text]),
        Some(&domain),
    )
    .await
    .map_err(api_err)?;
    let prefix = if account.is_group() { '!' } else { '@' };
    let acct = format!("{}@{domain}", account.username);
    borrow_index(
        &state,
        &admin,
        BorrowSource {
            label: format!("{prefix}{acct}"),
            href: format!("/{prefix}{acct}"),
            domain,
            emojis,
        },
    )
    .await
}

/// `GET /admin/custom-emojis/borrow/status/{id}` — extract the custom emoji
/// referenced by a remote post's content warning, body, poll options and its
/// custom-emoji reaction chips.
pub async fn borrow_status_index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    let mut item = status::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(not_found)?;
    if let Some(target_id) = item.reblog_of_id {
        item = status::find_by_id(&state.pool, target_id)
            .await
            .map_err(api_err)?
            .ok_or_else(not_found)?;
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(not_found)?;
    let Some(domain) = author.domain.clone() else {
        return Err(not_found());
    };
    let poll = poll::find_by_status(&state.pool, item.id)
        .await
        .map_err(api_err)?;
    let mut texts = vec![item.spoiler_text.as_str(), item.content.as_str()];
    if let Some(poll) = &poll {
        texts.extend(poll.options.iter().map(String::as_str));
    }
    let mut emojis = custom_emoji::lookup(
        &state.pool,
        &crate::emoji::shortcodes_of(&texts),
        Some(&domain),
    )
    .await
    .map_err(api_err)?;
    // Reaction emoji belong to the reactor's server, not necessarily the
    // post author's. Match by the federated image URL stored on the reaction,
    // then merge those rows with the author's text emoji without duplicates.
    let reaction_urls: Vec<String> = reaction::for_statuses(&state.pool, &[item.id])
        .await
        .map_err(api_err)?
        .remove(&item.id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|group| group.custom_emoji_url)
        .collect();
    for emoji in custom_emoji::find_by_remote_image_urls(&state.pool, &reaction_urls)
        .await
        .map_err(api_err)?
    {
        if !emojis.iter().any(|existing| existing.id == emoji.id) {
            emojis.push(emoji);
        }
    }
    let acct = format!("{}@{domain}", author.username);
    borrow_index(
        &state,
        &admin,
        BorrowSource {
            label: format!("post by @{acct}"),
            href: format!("/@{acct}/{}", item.id),
            domain,
            emojis,
        },
    )
    .await
}

async fn borrow_index(
    state: &AppState,
    admin: &WebAdmin,
    source: BorrowSource,
) -> Result<Response, Response> {
    let category_catalogs = source_category_catalogs(state, &source.emojis).await;
    let local: HashSet<String> = custom_emoji::list_local(&state.pool)
        .await
        .map_err(api_err)?
        .into_iter()
        .map(|emoji| emoji.shortcode)
        .collect();
    let candidates: Vec<BorrowCandidate> = source
        .emojis
        .into_iter()
        .map(|emoji| {
            let domain = emoji.domain.as_deref().unwrap_or(&source.domain);
            let advertised = category_catalogs
                .get(domain)
                .and_then(|categories| categories.get(&emoji.shortcode))
                .cloned();
            BorrowCandidate {
                already_local: local.contains(&emoji.shortcode),
                category_from_source: advertised.is_some(),
                category: advertised.unwrap_or_else(|| domain.to_owned()),
                emoji,
            }
        })
        .collect();
    let available = candidates
        .iter()
        .filter(|candidate| !candidate.already_local)
        .count();
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        p.admin-back { a href=(source.href) { "← Back to " (source.label) } }
        p {
            "Choose which referenced emoji to add to this server. Categories are read from each emoji's source server when it advertises them; otherwise that source's domain is used."
        }
        form.admin-form method="post" action="/web/admin/custom-emojis/borrow" {
            input type="hidden" name="csrf" value=(csrf);
            @if candidates.is_empty() {
                p.empty { "No borrowable custom emoji were found in this source." }
            } @else {
                div.admin-borrow-emojis {
                    @for candidate in &candidates {
                        article.admin-record.admin-emoji.admin-borrow-emoji {
                            div.admin-record__head {
                                label.admin-check.admin-borrow-emoji__choice {
                                    input type="checkbox" name="emoji_id" value=(candidate.emoji.id)
                                        checked[!candidate.already_local]
                                        disabled[candidate.already_local];
                                    div.admin-emoji__identity {
                                        img.admin-emoji__preview
                                            src=(crate::emoji::client_image_url(
                                                &state.config.domain, &candidate.emoji, false))
                                            alt="" aria-hidden="true";
                                        div {
                                            strong { ":" (candidate.emoji.shortcode) ":" }
                                            span.admin-table__sub {
                                                @if candidate.category_from_source {
                                                    "Category from source"
                                                } @else {
                                                    "Fallback category"
                                                }
                                            }
                                        }
                                    }
                                }
                                @if candidate.already_local {
                                    span.admin-badge.is-disabled { "Already local" }
                                }
                            }
                            label {
                                span { "Category" }
                                input type="text"
                                    name=(format!("category_{}", candidate.emoji.id))
                                    maxlength="100"
                                    value=(candidate.category)
                                    disabled[candidate.already_local];
                            }
                        }
                    }
                }
                div.admin-actions {
                    button type="submit" disabled[available == 0] {
                        "Import selected"
                    }
                }
            }
        }
    };
    Ok(admin_shell(admin, "/admin/custom-emojis", "Borrow custom emoji", &body).into_response())
}

async fn source_category_catalogs(
    state: &AppState,
    emojis: &[CustomEmoji],
) -> HashMap<String, HashMap<String, String>> {
    let mut domains: Vec<String> = emojis
        .iter()
        .filter_map(|emoji| emoji.domain.clone())
        .collect();
    domains.sort_unstable();
    domains.dedup();
    stream::iter(domains.into_iter().map(|domain| async move {
        let categories = source_categories(state, &domain).await;
        (domain, categories)
    }))
    .buffer_unordered(4)
    .collect()
    .await
}

/// Best-effort category discovery. `ActivityPub` Emoji objects have no category
/// field, but the public Mastodon-compatible catalog carries one on Mastodon,
/// `GoToSocial`, Pleroma/Akkoma and Plamenu. Misskey/Sharkey's public GET form
/// of `/api/emojis` is the fallback. A valid Mastodon-shaped response is
/// authoritative even when individual entries are uncategorized.
async fn source_categories(state: &AppState, domain: &str) -> HashMap<String, String> {
    let mastodon_url = format!("https://{domain}/api/v1/custom_emojis");
    if let Ok(page) = state
        .federation
        .fetch_page(&mastodon_url, "application/json")
        .await
        && let Some(categories) = mastodon_categories(&page.body)
    {
        return categories;
    }
    let misskey_url = format!("https://{domain}/api/emojis");
    state
        .federation
        .fetch_page(&misskey_url, "application/json")
        .await
        .ok()
        .and_then(|page| misskey_categories(&page.body))
        .unwrap_or_default()
}

fn mastodon_categories(body: &str) -> Option<HashMap<String, String>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let entries = value.as_array()?;
    Some(category_map(entries, "shortcode"))
}

fn misskey_categories(body: &str) -> Option<HashMap<String, String>> {
    let value: Value = serde_json::from_str(body).ok()?;
    Some(category_map(value.get("emojis")?.as_array()?, "name"))
}

fn category_map(entries: &[Value], name_key: &str) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get(name_key)?.as_str()?;
            let category = entry.get("category")?.as_str()?.trim();
            (!name.is_empty() && !category.is_empty()).then(|| {
                (
                    name.to_owned(),
                    category.chars().take(100).collect::<String>(),
                )
            })
        })
        .collect()
}

fn emoji_row(domain: &str, emoji: &CustomEmoji, csrf: &str) -> Markup {
    let edit_form_id = format!("instance-emoji-edit-{}", emoji.id);
    html! {
        article.admin-record.admin-emoji {
            div.admin-record__head {
                div.admin-emoji__identity {
                    img.admin-emoji__preview src=(crate::emoji::client_image_url(domain, emoji, false)) alt=(format!(":{}:", emoji.shortcode));
                    div {
                        strong { ":" (emoji.shortcode) ":" }
                        span.admin-table__sub { (emoji.image_file_name.as_deref().unwrap_or("")) }
                    }
                }
                div.admin-actions {
                    @if emoji.disabled { span.admin-badge.is-disabled { "Disabled" } }
                    @if !emoji.visible_in_picker { span.admin-badge.is-pending { "Hidden" } }
                }
            }
            form.admin-emoji__edit id=(&edit_form_id) method="post" action=(format!("/web/admin/custom-emojis/{}/update", emoji.id)) {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-form__checks {
                    label.admin-check {
                        input type="checkbox" name="visible_in_picker" value="1" checked[emoji.visible_in_picker];
                        span { "Visible in picker" }
                    }
                    label.admin-check {
                        input type="checkbox" name="disabled" value="1" checked[emoji.disabled];
                        span { "Disabled" }
                    }
                }
                label.admin-emoji__field {
                    span { "Category" }
                    input type="text" name="category" maxlength="100"
                        value=(emoji.category.as_deref().unwrap_or(""))
                        list="emoji-categories" placeholder="Uncategorized";
                }
            }
            div.admin-actions.admin-emoji__actions {
                button type="submit" form=(&edit_form_id) { "Save" }
                form.settings-inline-form method="post" action=(format!("/web/admin/custom-emojis/{}/delete", emoji.id)) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Delete" }
                }
            }
        }
    }
}

/// `POST /web/admin/custom-emojis` — upload and create a local custom emoji.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    mut multipart: Multipart,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;

    let mut csrf = String::new();
    let mut shortcode = String::new();
    let mut category = String::new();
    let mut image = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("invalid multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "shortcode" => shortcode = field.text().await.unwrap_or_default(),
            "category" => category = field.text().await.unwrap_or_default(),
            "image" => {
                image = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(format!("upload failed: {e}")))?
                    .to_vec();
            }
            _ => {}
        }
    }

    if !admin.user.csrf_ok(&csrf) {
        return Err(csrf_rejection());
    }
    let shortcode = shortcode.trim();
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Err(bad_request(
            "Use 2–128 letters, numbers, or underscores for the shortcode.".into(),
        ));
    }
    if image.is_empty() {
        return Err(bad_request("Choose an emoji image to upload.".into()));
    }
    let max_bytes = custom_emoji::settings(&state.pool)
        .await
        .map_err(api_err)?
        .max_file_size_bytes();
    let (content_type, extension) = validate_emoji_image(&image, max_bytes)
        .map_err(|error| bad_request(format!("The emoji image was rejected: {error}")))?;
    let file_name = format!("{}.{extension}", plamenu_db::id::next());
    let file_size = i64::try_from(image.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, image)
        .await
        .map_err(|e| bad_request(format!("upload failed: {e}")))?;
    let category = category.trim();
    let category: Option<String> =
        (!category.is_empty()).then(|| category.chars().take(100).collect());
    let created = custom_emoji::create_local(
        &state.pool,
        shortcode,
        &file_name,
        content_type,
        file_size,
        category.as_deref(),
    )
    .await
    .map_err(api_err)?;
    let Some(emoji) = created else {
        let _ = state.media.delete(&file_name).await;
        return Err(bad_request(format!(
            "The instance shortcode :{shortcode}: is already in use."
        )));
    };
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "create",
        &admin_log::Target::custom_emoji(emoji.id, &emoji.shortcode),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_emojis("applied"))
}

/// `POST /web/admin/custom-emojis/borrow` — download the selected cached
/// remote emoji as independent local uploads. The federation media client
/// applies the ordinary redirect/SSRF guard and the same configured format
/// and size validation as a manual upload. Four downloads at a time avoids
/// turning a large post into either a serial timeout ladder or a burst against
/// its host.
#[allow(
    clippy::too_many_lines,
    reason = "one bounded parse, fetch, validate, store and audit import pipeline"
)]
pub async fn borrow(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    let mut csrf = String::new();
    let mut selected = Vec::new();
    let mut categories: HashMap<i64, String> = HashMap::new();
    for (key, value) in pairs {
        if key == "csrf" {
            csrf = value;
        } else if key == "emoji_id" {
            if let Ok(id) = value.parse::<i64>()
                && selected.len() < 20
                && !selected.contains(&id)
            {
                selected.push(id);
            }
        } else if let Some(raw_id) = key.strip_prefix("category_")
            && let Ok(id) = raw_id.parse::<i64>()
        {
            categories.insert(id, value);
        }
    }
    if !admin.user.csrf_ok(&csrf) {
        return Err(csrf_rejection());
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut reasons: Vec<String> = Vec::new();
    let max_bytes = custom_emoji::settings(&state.pool)
        .await
        .map_err(api_err)?
        .max_file_size_bytes();
    let mut local: HashSet<String> = custom_emoji::list_local(&state.pool)
        .await
        .map_err(api_err)?
        .into_iter()
        .map(|emoji| emoji.shortcode)
        .collect();
    let mut pending_downloads = Vec::new();
    for id in selected {
        let Some(emoji) = custom_emoji::find_managed_by_id(&state.pool, id)
            .await
            .map_err(api_err)?
            .filter(|emoji| emoji.domain.is_some() && !emoji.disabled)
        else {
            failed += 1;
            reasons.push(format!(
                "Emoji {id} is missing, disabled, or is not remote."
            ));
            continue;
        };
        if local.contains(&emoji.shortcode) {
            skipped += 1;
            continue;
        }
        let Some(remote_url) = emoji.image_remote_url.clone() else {
            failed += 1;
            reasons.push(format!(
                ":{}: has no downloadable source image.",
                emoji.shortcode
            ));
            continue;
        };
        if plamenu_db::instance_policy::domain_rejects_media(
            &state.pool,
            emoji.domain.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(api_err)?
        {
            failed += 1;
            reasons.push(format!(
                "Images from {} are blocked by the server media policy.",
                emoji.domain.as_deref().unwrap_or("that server")
            ));
            continue;
        }
        pending_downloads.push((emoji, remote_url));
    }

    let federation = state.federation.clone();
    let downloads = stream::iter(pending_downloads.into_iter().map(|(emoji, remote_url)| {
        let federation = federation.clone();
        async move {
            let download = federation.fetch_media(&remote_url).await;
            (emoji, download)
        }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;

    for (emoji, download) in downloads {
        let download = match download {
            Ok(download) => download,
            Err(error) => {
                failed += 1;
                reasons.push(format!(
                    ":{}: could not be downloaded: {error}",
                    emoji.shortcode
                ));
                continue;
            }
        };
        let (content_type, extension) = match validate_emoji_image(&download.bytes, max_bytes) {
            Ok(valid) => valid,
            Err(error) => {
                failed += 1;
                reasons.push(format!(
                    ":{}: downloaded image was rejected: {error}",
                    emoji.shortcode
                ));
                continue;
            }
        };
        let file_name = format!("{}.{extension}", plamenu_db::id::next());
        let file_size = i64::try_from(download.bytes.len()).unwrap_or(i64::MAX);
        if let Err(error) = state.media.put(&file_name, download.bytes).await {
            tracing::warn!(%error, %file_name, "failed to store borrowed custom emoji");
            failed += 1;
            reasons.push(format!(
                ":{}: image storage failed: {error}",
                emoji.shortcode
            ));
            continue;
        }
        let fallback = emoji.domain.as_deref().unwrap_or_default();
        let category = categories
            .get(&emoji.id)
            .map(String::as_str)
            .map(str::trim)
            .filter(|category| !category.is_empty())
            .unwrap_or(fallback)
            .chars()
            .take(100)
            .collect::<String>();
        let created = custom_emoji::create_global_borrow(
            &state.pool,
            &emoji,
            &file_name,
            content_type,
            file_size,
            Some(&category),
        )
        .await;
        let created = match created {
            Ok(Some(created)) => created,
            Ok(None) => {
                let _ = state.media.delete(&file_name).await;
                skipped += 1;
                continue;
            }
            Err(error) => {
                let _ = state.media.delete(&file_name).await;
                return Err(api_err(error));
            }
        };
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "create",
            &admin_log::Target::custom_emoji(created.id, &created.shortcode),
        )
        .await
        .map_err(api_err)?;
        local.insert(created.shortcode);
        imported += 1;
    }

    Ok(redirect_borrow_result(imported, skipped, failed, &reasons))
}

fn redirect_borrow_result(
    imported: usize,
    skipped: usize,
    failed: usize,
    reasons: &[String],
) -> Response {
    let reason = reasons
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let mut query = format!("borrowed={imported}&skipped={skipped}&failed={failed}");
    if !reason.is_empty() {
        let encoded = serde_urlencoded::to_string([("reason", reason)]).unwrap_or_default();
        query.push('&');
        query.push_str(&encoded);
    }
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/custom-emojis?{query}"))],
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct UpdateForm {
    csrf: String,
    disabled: Option<String>,
    visible_in_picker: Option<String>,
    category: Option<String>,
}

/// `POST /web/admin/custom-emojis/{id}/update` — update picker visibility,
/// disabled state and picker category (blank clears it).
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let category = form
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let updated = custom_emoji::update_local_flags(
        &state.pool,
        id,
        form.disabled.is_some(),
        form.visible_in_picker.is_some(),
        category,
    )
    .await
    .map_err(api_err)?;
    if let Some(emoji) = &updated {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "update",
            &admin_log::Target::custom_emoji(emoji.id, &emoji.shortcode),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_emojis(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/custom-emojis/{id}/delete` — delete the local emoji row and
/// remove its stored media file when present.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let deleted = custom_emoji::delete_local_by_id(&state.pool, id)
        .await
        .map_err(api_err)?;
    let Some(emoji) = deleted else {
        return Ok(redirect_emojis("error"));
    };
    if let Some(file_name) = &emoji.image_file_name
        && let Err(error) = state.media.delete(file_name).await
    {
        tracing::warn!(%error, %file_name, "failed to remove deleted emoji media");
    }
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "destroy",
        &admin_log::Target::custom_emoji(emoji.id, &emoji.shortcode),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_emojis("applied"))
}

fn redirect_emojis(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/custom-emojis?flash={flash}"),
        )],
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct PersonalAdminQuery {
    error: Option<String>,
    saved: Option<String>,
}

pub async fn user_emojis(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<PersonalAdminQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    let owner = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .filter(|account| account.domain.is_none())
        .ok_or_else(|| bad_request("No local account has that ID.".into()))?;
    let emojis = custom_emoji::list_personal_for_moderation(&state.pool, owner.id)
        .await
        .map_err(api_err)?;
    let error = match query.error.as_deref() {
        Some("shortcode") => Some("That user already has an active emoji with that shortcode."),
        Some("promotion-shortcode") => Some(
            "An instance-wide emoji already has that shortcode. Choose another before promoting.",
        ),
        Some("missing") => Some("That personal emoji no longer exists."),
        _ => None,
    };
    let body = html! {
        p.admin-back { a href="/admin/custom-emojis" { "← Custom emoji" } }
        div.admin-record__head {
            h3 { "Personal emoji for @" (&owner.username) }
            a.admin-record__open href="/admin/custom-emojis/trending" { "Review trending emoji" }
        }
        p.admin__lead { "Changes here affect this member's personal collection only." }
        @if query.saved.is_some() { p.admin-flash role="status" { "Emoji change saved." } }
        @if let Some(error) = error { p.admin-flash.is-error role="alert" { (error) } }
        section.admin-list {
            @if emojis.is_empty() { p.empty { "This user has no current or retired personal emoji." } }
            @for emoji in &emojis {
                (personal_moderation_row(&state.config.domain, &admin, &owner, emoji))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/custom-emojis", "User custom emoji", &body).into_response())
}

fn personal_moderation_row(
    domain: &str,
    admin: &WebAdmin,
    owner: &plamenu_db::account::Account,
    emoji: &plamenu_db::custom_emoji::ManagedCustomEmoji,
) -> Markup {
    let edit_form_id = format!("personal-moderation-edit-{}", emoji.id);
    html! {
        article.admin-record.admin-emoji {
            div.admin-record__head {
                div.admin-emoji__identity {
                    img.admin-emoji__preview src=(crate::emoji::client_image_url(domain, &emoji.as_emoji(), false)) alt=(format!(":{}:", emoji.shortcode));
                    div {
                        strong { ":" (&emoji.shortcode) ":" }
                        @if let Some(category) = &emoji.category {
                            span.admin-table__sub { (category) }
                        }
                    }
                }
                div.admin-actions.admin-emoji__badges {
                    @if emoji.retired { span.admin-badge.is-disabled { "Retired" } }
                    @if emoji.borrowed { span.admin-badge { "Borrowed" } }
                    @if emoji.disabled && !emoji.retired { span.admin-badge.is-disabled { "Disabled" } }
                }
            }
            @if emoji.retired {
                p.admin-record__stats { "This emoji has been removed from the member's collection." }
            } @else {
                form.admin-emoji__moderation id=(&edit_form_id) method="post" action=(format!("/web/admin/custom-emojis/{}/moderate", emoji.id)) {
                    input type="hidden" name="csrf" value=(&admin.user.csrf);
                    input type="hidden" name="owner_id" value=(owner.id);
                    label.admin-emoji__field {
                        span { "Shortcode" }
                        input type="text" name="shortcode" value=(&emoji.shortcode) pattern="[A-Za-z0-9_]{2,128}" required;
                    }
                    label.admin-emoji__field {
                        span { "Category" }
                        input type="text" name="category" value=(emoji.category.as_deref().unwrap_or("")) maxlength="100" placeholder="Uncategorized";
                    }
                    label.admin-check {
                        input type="checkbox" name="disabled" value="1" checked[emoji.disabled];
                        span { "Disabled" }
                    }
                }
                div.admin-actions.admin-emoji__actions {
                    button type="submit" form=(&edit_form_id) { "Save moderation" }
                    @let retire_action = format!("/web/admin/custom-emojis/{}/retire", emoji.id);
                    @let retire_message = "Remove this emoji from the user's collection?";
                    form.settings-inline-form method="post" action=(crate::web::view::CONFIRM_PATH)
                        data-confirm=(retire_message) data-confirm-action=(&retire_action) {
                        input type="hidden" name="csrf" value=(&admin.user.csrf);
                        input type="hidden" name="owner_id" value=(owner.id);
                        input type="hidden" name="return_to" value=(format!("/admin/custom-emojis/users/{}", owner.id));
                        (crate::web::view::confirmation_fields(&retire_action, Some(retire_message)))
                        button.admin-danger type="submit" { "Remove" }
                    }
                }
                div.admin-emoji__promotion {
                    strong { "Make available to everyone" }
                    p.admin-record__stats { "Promotion removes this emoji from the member's personal limit." }
                    form.admin-emoji__promote method="post" action=(format!("/web/admin/custom-emojis/{}/promote", emoji.id)) {
                        input type="hidden" name="csrf" value=(&admin.user.csrf);
                        input type="hidden" name="return_owner" value=(owner.id);
                        label.admin-emoji__field {
                            span { "Instance shortcode" }
                            input type="text" name="shortcode" value=(&emoji.shortcode) pattern="[A-Za-z0-9_]{2,128}" required;
                        }
                        label.admin-emoji__field {
                            span { "Instance category" }
                            input type="text" name="category" value=(emoji.category.as_deref().unwrap_or("")) maxlength="100" placeholder="Uncategorized";
                        }
                        button type="submit" { "Promote instance-wide" }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ModeratePersonalForm {
    csrf: String,
    owner_id: i64,
    shortcode: String,
    category: String,
    disabled: Option<String>,
}

pub async fn moderate_personal(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<ModeratePersonalForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let shortcode = form.shortcode.trim();
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Err(bad_request(
            "Use 2–128 letters, numbers, or underscores for the shortcode.".into(),
        ));
    }
    if custom_emoji::list_personal(&state.pool, form.owner_id)
        .await
        .map_err(api_err)?
        .iter()
        .any(|emoji| emoji.id != id && emoji.shortcode == shortcode)
    {
        return Ok(redirect_user(form.owner_id, "error=shortcode"));
    }
    let category = form.category.trim();
    let updated = custom_emoji::moderate_personal(
        &state.pool,
        id,
        shortcode,
        (!category.is_empty()).then_some(category),
        form.disabled.is_some(),
    )
    .await
    .map_err(api_err)?;
    if let Some(emoji) = &updated {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "update",
            &admin_log::Target::custom_emoji(emoji.id, &emoji.shortcode),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_user(
        form.owner_id,
        if updated.is_some() {
            "saved=1"
        } else {
            "error=missing"
        },
    ))
}

#[derive(Debug, Deserialize)]
pub struct RetirePersonalForm {
    csrf: String,
    owner_id: i64,
}

pub async fn retire_personal(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<RetirePersonalForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let source = custom_emoji::find_managed_by_id(&state.pool, id)
        .await
        .map_err(api_err)?;
    let changed = custom_emoji::retire_personal_by_moderator(&state.pool, id)
        .await
        .map_err(api_err)?;
    if changed && let Some(emoji) = source {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "destroy",
            &admin_log::Target::custom_emoji(emoji.id, &emoji.shortcode),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_user(
        form.owner_id,
        if changed { "saved=1" } else { "error=missing" },
    ))
}

#[derive(Debug, Deserialize)]
pub struct PromoteForm {
    csrf: String,
    shortcode: String,
    category: String,
    return_owner: Option<i64>,
}

#[allow(
    clippy::too_many_lines,
    reason = "promotion handles local and remote sources"
)]
pub async fn promote_personal(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<PromoteForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let shortcode = form.shortcode.trim();
    if !plamenu_ap::emoji::is_valid_shortcode(shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN) {
        return Err(bad_request(
            "Use 2–128 letters, numbers, or underscores for the promoted shortcode.".into(),
        ));
    }
    let category = form.category.trim();
    let category = (!category.is_empty()).then_some(category);
    let source = custom_emoji::find_managed_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .filter(|emoji| !emoji.retired)
        .ok_or_else(|| bad_request("That custom emoji is no longer available.".into()))?;
    let query = if source.owner_account_id.is_some() {
        match custom_emoji::promote_personal(&state.pool, id, shortcode, category)
            .await
            .map_err(api_err)?
        {
            custom_emoji::PromoteOutcome::Promoted(_)
            | custom_emoji::PromoteOutcome::AlreadyPromoted(_) => "saved=1",
            custom_emoji::PromoteOutcome::ShortcodeTaken => "error=promotion-shortcode",
            custom_emoji::PromoteOutcome::NotPersonal => "error=missing",
        }
    } else if let Some(remote_domain) = source.domain.as_deref() {
        if plamenu_db::instance_policy::domain_rejects_media(&state.pool, remote_domain)
            .await
            .map_err(api_err)?
        {
            return Err(bad_request(format!(
                "Images from {remote_domain} are blocked by this server's media policy."
            )));
        }
        let remote_url = source
            .image_remote_url
            .as_deref()
            .ok_or_else(|| bad_request("The remote emoji has no downloadable image.".into()))?;
        let download = state
            .federation
            .fetch_media(remote_url)
            .await
            .map_err(|error| {
                bad_request(format!(
                    "The remote emoji image could not be downloaded: {error}"
                ))
            })?;
        let max_bytes = custom_emoji::settings(&state.pool)
            .await
            .map_err(api_err)?
            .max_file_size_bytes();
        let (content_type, extension) = validate_emoji_image(&download.bytes, max_bytes)
            .map_err(|error| bad_request(format!("The downloaded image was rejected: {error}")))?;
        let file_name = format!("{}.{}", plamenu_db::id::next(), extension);
        let file_size = i64::try_from(download.bytes.len()).unwrap_or(i64::MAX);
        state
            .media
            .put(&file_name, download.bytes)
            .await
            .map_err(|error| {
                bad_request(format!("The emoji image could not be stored: {error}"))
            })?;
        let created = custom_emoji::create_global_borrow_as(
            &state.pool,
            &source,
            shortcode,
            &file_name,
            content_type,
            file_size,
            category,
        )
        .await;
        match created {
            Ok(Some(_)) => "saved=1",
            Ok(None) => {
                let _ = state.media.delete(&file_name).await;
                "error=promotion-shortcode"
            }
            Err(error) => {
                let _ = state.media.delete(&file_name).await;
                return Err(bad_request(format!(
                    "The promoted emoji could not be recorded: {error}"
                )));
            }
        }
    } else {
        "saved=1"
    };
    if query == "saved=1" {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "promote",
            &admin_log::Target::custom_emoji(source.id, &source.shortcode),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(form.return_owner.map_or_else(
        || redirect_trending("local", query),
        |owner| redirect_user(owner, query),
    ))
}

#[derive(Debug, Deserialize)]
pub struct TrendingQuery {
    scope: Option<String>,
    error: Option<String>,
    saved: Option<String>,
}

pub async fn trending(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<TrendingQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_CUSTOM_EMOJIS)?;
    let (scope_name, scope) = match query.scope.as_deref() {
        Some("federated") => ("federated", custom_emoji::TrendingScope::Federated),
        Some("personal") => ("personal", custom_emoji::TrendingScope::PersonalOnly),
        _ => ("local", custom_emoji::TrendingScope::Local),
    };
    let rows = custom_emoji::trending(&state.pool, scope, 100)
        .await
        .map_err(api_err)?;
    let tabs = [
        crate::web::view::Tab::new(
            "/admin/custom-emojis/trending?scope=federated",
            "Federated",
            scope_name == "federated",
        ),
        crate::web::view::Tab::new(
            "/admin/custom-emojis/trending?scope=local",
            "Local",
            scope_name == "local",
        ),
        crate::web::view::Tab::new(
            "/admin/custom-emojis/trending?scope=personal",
            "Local users only",
            scope_name == "personal",
        ),
    ];
    let body = html! {
        p.admin-back { a href="/admin/custom-emojis" { "← Custom emoji" } }
        (crate::web::view::tab_strip("Trend scope", &tabs))
        p.admin__lead { "Trailing seven days, ranked by unique users and then total post and reaction uses." }
        @if query.saved.is_some() { p.admin-flash role="status" { "Emoji promoted instance-wide." } }
        @if query.error.as_deref() == Some("promotion-shortcode") { p.admin-flash.is-error role="alert" { "That instance shortcode is occupied. Enter a different shortcode." } }
        section.admin-list {
            @if rows.is_empty() { p.empty { "No custom emoji usage in this scope during the last seven days." } }
            @for row in &rows {
                (trending_row(&state.config.domain, &admin, row))
            }
        }
    };
    Ok(admin_shell(
        &admin,
        "/admin/custom-emojis",
        "Trending custom emoji",
        &body,
    )
    .into_response())
}

fn trending_row(domain: &str, admin: &WebAdmin, row: &custom_emoji::TrendingCustomEmoji) -> Markup {
    let image = row.image_file_name.as_ref().map_or_else(
        || crate::entities::media_proxy_url(domain, "emoji", row.emoji_id, false, false),
        |file| format!("https://{domain}/media/{file}"),
    );
    let people_word = if row.unique_users == 1 {
        "user"
    } else {
        "users"
    };
    let activity_word = if row.total_uses == 1 { "use" } else { "uses" };
    let source_label = match row.origin_source.as_str() {
        "federated" => "Federated",
        "local" => "Local",
        _ => row.origin_source.as_str(),
    };
    let promotable = row.owner_account_id.is_some() || row.domain.is_some();
    html! {
        article.admin-record.admin-emoji {
            div.admin-record__head {
                div.admin-emoji__identity {
                    img.admin-emoji__preview src=(image) alt=(format!(":{}:", row.shortcode));
                    div {
                        strong { ":" (&row.shortcode) ":" }
                        span.admin-table__sub {
                            (row.unique_users) " " (people_word) " · " (row.total_uses) " " (activity_word)
                        }
                    }
                }
                @if promotable {
                    span.admin-badge { (source_label) }
                } @else {
                    span.admin-badge { "Instance-wide" }
                }
            }
            @if promotable {
                div.admin-emoji__promotion {
                    strong { "Make available to everyone" }
                    form.admin-emoji__promote method="post" action=(format!("/web/admin/custom-emojis/{}/promote", row.emoji_id)) {
                        input type="hidden" name="csrf" value=(&admin.user.csrf);
                        label.admin-emoji__field {
                            span { "Instance shortcode" }
                            input type="text" name="shortcode" value=(&row.shortcode) pattern="[A-Za-z0-9_]{2,128}" required;
                        }
                        label.admin-emoji__field {
                            span { "Instance category" }
                            input type="text" name="category" value=(row.category.as_deref().unwrap_or("")) maxlength="100" placeholder="Uncategorized";
                        }
                        button type="submit" { "Promote instance-wide" }
                    }
                }
            }
        }
    }
}

fn redirect_user(owner: i64, query: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/custom-emojis/users/{owner}?{query}"),
        )],
    )
        .into_response()
}

fn redirect_trending(scope: &str, query: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/custom-emojis/trending?scope={scope}&{query}"),
        )],
    )
        .into_response()
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Custom emoji source not found.").into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_categories_from_mastodon_compatible_catalogs() {
        let categories = mastodon_categories(
            r#"[
                {"shortcode":"blobcat","category":"Blobs"},
                {"shortcode":"plain","category":null},
                {"shortcode":"blank","category":"  "}
            ]"#,
        )
        .unwrap();
        assert_eq!(categories.get("blobcat").map(String::as_str), Some("Blobs"));
        assert!(!categories.contains_key("plain"));
        assert!(!categories.contains_key("blank"));
    }

    #[test]
    fn extracts_categories_from_misskey_catalogs() {
        let categories = misskey_categories(
            r#"{"emojis":[
                {"name":"party_parrot","category":"Parrots"},
                {"name":"uncategorized","category":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            categories.get("party_parrot").map(String::as_str),
            Some("Parrots")
        );
        assert!(!categories.contains_key("uncategorized"));
    }
}
