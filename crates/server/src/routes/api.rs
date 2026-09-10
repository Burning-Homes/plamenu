//! Mastodon-compatible client API endpoints.

use axum::Json;
use axum::extract::State;
use plamenu_db::instance_settings::InstanceSettings;
use plamenu_db::{account, instance_settings, site_upload};
use serde_json::{Value, json};

use crate::entities::rfc3339;
use crate::error::ApiError;
use crate::{AppState, SOURCE_URL, compat_version};

/// The three full-table aggregates `/api/v1/instance` reports, cached.
///
/// Counting every local account, every local status and every known domain on
/// each anonymous request is not something an endpoint clients poll can afford:
/// measured together on a staging-scale database they were ~58 ms, of which
/// `status::count_local` alone was a parallel sequential scan. Both siblings
/// already mitigate — the landing page routes the same counters through this
/// cache, and nodeinfo carries a 30-minute `Cache-Control` — so this endpoint
/// was the one place left paying it per view.
///
/// Its own key rather than the landing page's: two of the three numbers
/// coincide, but Mastodon's `user_count` is every local account while the
/// landing page counts people and bots separately, and sharing a payload
/// between two endpoints that want different numbers is how one of them
/// silently starts reporting the other's.
async fn cached_stats(state: &AppState) -> Result<Value, ApiError> {
    const KEY: &str = "instance/v1/stats";
    if let Some(cached) = state.metrics_cache.get(KEY) {
        return Ok(cached);
    }
    let counted = json!({
        "user_count": account::count_public_local(&state.pool).await?,
        "status_count": plamenu_db::status::count_local(&state.pool).await?,
        "domain_count": account::count_known_domains(&state.pool).await?,
    });
    state.metrics_cache.put(KEY.to_owned(), counted.clone());
    Ok(counted)
}

pub async fn instance_v1(
    State(state): State<AppState>,
) -> Result<([(axum::http::HeaderName, &'static str); 1], Json<Value>), ApiError> {
    let counted = cached_stats(&state).await?;
    let rules = crate::entities::instance_rules_json(&state.pool).await?;
    let settings = instance_settings::get(&state.pool).await?;
    let contact_account = contact_account_json(&state, &settings).await?;
    let thumbnail = site_thumbnail(&state).await?;
    let registrations = super::registrations::registrations_json(&state).await?;
    let domain = &state.config.domain;
    let account_domain = &state.config.account_domain;
    let body = json!({
        "uri": domain,
        // GoToSocial's established split-domain extension. `uri` remains the
        // API host so clients do not try to log in at a non-Plamenu apex.
        "account_domain": account_domain,
        "title": settings.site_title,
        "short_description": settings.site_short_description,
        // Mastodon serves the legacy `site_description` setting here, which
        // its admin UI no longer edits — stays empty.
        "description": "",
        "email": settings.site_contact_email,
        "version": compat_version(),
        "urls": { "streaming_api": format!("wss://{domain}") },
        "stats": counted,
        "thumbnail": thumbnail.as_ref().map_or(Value::Null, |t| Value::String(t.url_1x.clone())),
        "languages": ["en"],
        "registrations": registrations["enabled"],
        "approval_required": registrations["approval_required"],
        // Mastodon reports `UserRole.everyone.can?(:invite_users)`; our
        // stand-in for the everyone role is the seeded default "User" role —
        // whether an ordinary member may mint invites.
        "invites_enabled": plamenu_db::role::find_by_id(&state.pool, plamenu_db::role::DEFAULT_ROLE_ID)
            .await?
            .is_some_and(|role| role.can(plamenu_db::role::permission::INVITE_USERS)),
        "configuration": {
            "accounts": accounts_config(),
            "statuses": statuses_config(&settings),
            "media_attachments": media_attachments_config(),
            "polls": polls_config(&settings),
        },
        "contact_account": contact_account,
        "rules": rules,
        // Pleroma extension block (P4): the accepted rich-text formats.
        // Pleroma-aware clients read `pleroma.metadata.post_formats` to
        // decide whether to offer a composer format selector.
        "pleroma": pleroma_metadata(),
    });
    // Matches the cache above rather than outliving it: a client told to hold
    // this for half an hour would be reporting counts the server had already
    // recomputed twice.
    Ok((
        [(axum::http::header::CACHE_CONTROL, "public, max-age=300")],
        Json(body),
    ))
}

/// The `pleroma.metadata` extension block both instance endpoints carry —
/// currently just `post_formats` (P4); other Pleroma metadata is added as
/// the features behind it land.
fn pleroma_metadata() -> Value {
    json!({
        "metadata": {
            "post_formats": crate::compose::PostFormat::ADVERTISED,
        },
    })
}

pub async fn instance_v2(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Mastodon's v2 `usage.users.active_month` is monthly-active users, not
    // the account total.
    let now = time::OffsetDateTime::now_utc();
    let active_month =
        plamenu_db::metrics::active_users_total(&state.pool, now - time::Duration::days(30), now)
            .await?;
    let rules = crate::entities::instance_rules_json(&state.pool).await?;
    let settings = instance_settings::get(&state.pool).await?;
    let contact_account = contact_account_json(&state, &settings).await?;
    let thumbnail = site_thumbnail(&state).await?;
    let app_icons = app_icon_json(&state).await?;
    let registrations = super::registrations::registrations_json(&state).await?;
    let domain = &state.config.domain;
    let account_domain = &state.config.account_domain;
    let mut body = json!({
        "domain": domain,
        "account_domain": account_domain,
        "title": settings.site_title,
        "version": compat_version(),
        // Mastodon 4.6's API revision marker; clients feature-detect
        // collections, grouped notifications & co. from it rather than the
        // version.
        "api_versions": { "mastodon": 11 },
        "source_url": SOURCE_URL,
        "description": settings.site_short_description,
        "usage": { "users": { "active_month": active_month } },
        "thumbnail": match &thumbnail {
            Some(t) => json!({
                "url": t.url_1x,
                "blurhash": t.blurhash,
                "versions": { "@1x": t.url_1x, "@2x": t.url_2x },
                "description": t.description,
            }),
            // No operator thumbnail: the static fallback Mastodon fills with
            // its bundled preview asset.
            None => json!({ "url": format!("https://{domain}/thumbnail.png") }),
        },
        "languages": ["en"],
        "configuration": {
            "urls": { "streaming": format!("wss://{domain}") },
            "accounts": accounts_config(),
            "statuses": statuses_config(&settings),
            "media_attachments": media_attachments_config(),
            "polls": polls_config(&settings),
            "translation": { "enabled": crate::translation::configured(&state) },
            "timelines_access": timelines_access_config(&settings),
            // Clients subscribe to Web Push with this key.
            "vapid": { "public_key": vapid_public_key(&state).await? },
        },
        "registrations": registrations,
        "contact": { "email": settings.site_contact_email, "account": contact_account },
        "rules": rules,
        // Pleroma extension block (P4), same shape as on v1 — Akkoma serves
        // it on both versions too.
        "pleroma": pleroma_metadata(),
    });
    // Mastodon always serves `icon`, falling back to its bundled frontend
    // icons; Plamenu bundles none, so the key appears once an app icon is
    // uploaded.
    if let Some(icons) = app_icons {
        body["icon"] = icons;
    }
    Ok(Json(body))
}

/// The operator thumbnail's rendered styles, for the instance entities and
/// the instance-level pages' `og:image`.
pub(crate) struct SiteThumbnail {
    pub(crate) url_1x: String,
    url_2x: String,
    blurhash: Option<String>,
    pub(crate) description: String,
}

pub(crate) async fn site_thumbnail(state: &AppState) -> Result<Option<SiteThumbnail>, ApiError> {
    let Some(upload) = site_upload::get(&state.pool, "thumbnail").await? else {
        return Ok(None);
    };
    let variants = site_upload::variants_for(&state.pool, "thumbnail").await?;
    let domain = &state.config.domain;
    let url_for = |style: &str| {
        variants
            .iter()
            .find(|v| v.style == style)
            .map(|v| format!("https://{domain}/media/{}", v.file_name))
    };
    // Fall back to the original if a style is somehow missing.
    let original = format!("https://{domain}/media/{}", upload.file_name);
    Ok(Some(SiteThumbnail {
        url_1x: url_for("@1x").unwrap_or_else(|| original.clone()),
        url_2x: url_for("@2x").unwrap_or(original),
        blurhash: upload.blurhash,
        description: upload.description,
    }))
}

/// The uploaded app icon rendered per Android size — the v2 instance `icon`
/// array (`REST::InstanceSerializer#icon`).
async fn app_icon_json(state: &AppState) -> Result<Option<Value>, ApiError> {
    if site_upload::get(&state.pool, "app_icon").await?.is_none() {
        return Ok(None);
    }
    let variants = site_upload::variants_for(&state.pool, "app_icon").await?;
    if variants.is_empty() {
        return Ok(None);
    }
    let domain = &state.config.domain;
    Ok(Some(Value::Array(
        variants
            .iter()
            .map(|v| {
                json!({
                    "src": format!("https://{domain}/media/{}", v.file_name),
                    "size": format!("{}x{}", v.width, v.height),
                })
            })
            .collect(),
    )))
}

/// Resolves the operator-designated contact account (Mastodon's
/// `site_contact_username` setting) to a REST `Account` entity, or `null`.
/// Mastodon tolerates a leading `@` and an `@domain` suffix; the account must
/// be local here (Plamenu has no reason to advertise a remote contact).
async fn contact_account_json(
    state: &AppState,
    settings: &InstanceSettings,
) -> Result<Value, ApiError> {
    let raw = settings.site_contact_username.trim();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    let (username, contact_domain) = match raw.split_once('@') {
        Some((name, domain)) => (name, Some(domain)),
        None => (raw, None),
    };
    if username.is_empty() || contact_domain.is_some_and(|d| !state.config.is_local_domain(d)) {
        return Ok(Value::Null);
    }
    let contact = account::find_local_by_username(&state.pool, username).await?;
    if let Some(contact) = contact
        && crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &contact,
        )
        .await?
    {
        return crate::entities::account_json(&state.pool, &state.config.domain, &contact, None)
            .await;
    }
    Ok(Value::Null)
}

/// `GET /api/v1/instance/rules` — the server's numbered rules, in display
/// order (Mastodon's `Api::V1::Instances::RulesController#index`).
pub async fn instance_rules(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rules = crate::entities::instance_rules_json(&state.pool).await?;
    Ok(Json(Value::Array(rules)))
}

/// `GET /api/v1/instance/extended_description` — long-form "about" text, the
/// `site_extended_description` setting rendered from Markdown to sanitized
/// HTML (Mastodon's `REST::ExtendedDescriptionSerializer` runs it through
/// Redcarpet). Empty state when the operator has written nothing.
pub async fn instance_extended_description(
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    if settings.site_extended_description.is_empty() {
        return Ok(Json(json!({ "updated_at": Value::Null, "content": "" })));
    }
    Ok(Json(json!({
        "updated_at": rfc3339(settings.updated_at)?,
        "content": markdown_html(&settings.site_extended_description),
    })))
}

/// Renders operator-supplied Markdown to HTML, sanitized with the same
/// allowlist applied to remote HTML.
pub(crate) fn markdown_html(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    plamenu_ap::text::sanitize_remote_html(&html)
}

/// `GET /api/v1/instance/privacy_policy` — privacy policy. Empty state until
/// operator-supplied content exists.
pub async fn instance_privacy_policy() -> Json<Value> {
    Json(json!({ "updated_at": Value::Null, "content": "" }))
}

/// `GET /api/v1/instance/terms_of_service` — the current published Terms of
/// Service (editor), rendered from Markdown like Mastodon's
/// `REST::TermsOfServiceSerializer`; 404 when none has been published (its
/// controller raises `RecordNotFound`).
pub async fn instance_terms_of_service(
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let Some(tos) = plamenu_db::terms_of_service::current(&state.pool).await? else {
        return Err(ApiError::NotFound);
    };
    let succeeded_by =
        plamenu_db::terms_of_service::next_effective_after(&state.pool, tos.id, tos.effective_date)
            .await?;
    let date_only = time::macros::format_description!("[year]-[month]-[day]");
    let effective_date = tos
        .effective_date
        .or_else(|| tos.published_at.map(time::OffsetDateTime::date))
        .and_then(|date| date.format(date_only).ok());
    // Mastodon substitutes the instance domain into `%{domain}` placeholders
    // (its ToS generator templates use them).
    let text = tos.text.replace("%{domain}", &state.config.account_domain);
    Ok(Json(json!({
        "effective_date": effective_date,
        "effective": tos.effective(),
        "succeeded_by": succeeded_by.and_then(|date| date.format(date_only).ok()),
        "content": markdown_html(&text),
    })))
}

async fn vapid_public_key(state: &AppState) -> Result<String, ApiError> {
    let vapid = crate::web_push::vapid(state)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(vapid.public_key)
}

/// `configuration.accounts` — the profile-editing limits, mirroring the
/// constants `profile.rs` validates against and the pin cap in `actions.rs`.
/// Clients size their profile editor from these: Phanpy renders exactly
/// `max_profile_fields` metadata rows and takes the description `maxlength`
/// from the two image-description limits, falling back to Mastodon's defaults
/// when a server omits them.
fn accounts_config() -> Value {
    json!({
        "max_display_name_length": crate::profile::MAX_DISPLAY_NAME_CHARS,
        "max_note_length": crate::profile::MAX_NOTE_CHARS,
        "max_avatar_description_length": MEDIA_DESCRIPTION_LIMIT,
        "max_header_description_length": MEDIA_DESCRIPTION_LIMIT,
        "max_featured_tags": 10,
        "max_pinned_statuses": crate::actions::PIN_LIMIT,
        "max_profile_fields": crate::profile::MAX_FIELDS,
        "profile_field_name_limit": crate::profile::MAX_FIELD_CHARS,
        "profile_field_value_limit": crate::profile::MAX_FIELD_CHARS,
    })
}

/// `configuration.timelines_access` (Mastodon 4.6) — whether each public feed
/// answers an anonymous request. Clients read it to decide what to offer a
/// logged-out visitor instead of walking them into a 401; the values are
/// Mastodon's `public` / `authenticated` / `disabled`.
///
/// Our per-surface preview knobs are booleans, and a disabled one still serves
/// a signed-in viewer (`MaybeUser::require_preview` 401s only anonymous
/// callers), so they map to `public`/`authenticated`. The trending surfaces
/// take no viewer at all but vanish entirely when trends are off — `disabled`
/// is the honest answer there.
fn timelines_access_config(settings: &InstanceSettings) -> Value {
    let anon = |allowed: bool| if allowed { "public" } else { "authenticated" };
    let tag = anon(settings.timeline_preview_tag);
    let trending = if settings.trends_enabled {
        "public"
    } else {
        "disabled"
    };
    json!({
        "live_feeds": {
            "local": anon(settings.timeline_preview_local),
            "remote": anon(settings.timeline_preview_federated),
        },
        "hashtag_feeds": { "local": tag, "remote": tag },
        "trending_link_feeds": { "local": trending, "remote": trending },
    })
}

fn statuses_config(settings: &InstanceSettings) -> Value {
    json!({
        "max_characters": settings.max_characters,
        "max_media_attachments": settings.max_media_attachments,
        "characters_reserved_per_url": 23,
        // Plamenu extension: the cap that applies instead when a post is
        // published as long-form (`post_kind=article`). Stock clients ignore it
        // and keep using `max_characters`, which is still the limit for
        // everything they can compose.
        "max_characters_long_form": settings.max_characters_long_form,
    })
}

/// The media types an upload to `POST /api/v{1,2}/media` is actually accepted
/// in, derived from what the two upload paths admit: the still-image decoder
/// (`media_processing::is_still_image`) and the ffprobe container allowlist
/// (`media_transcode::ACCEPTED_FORMATS`), expressed as the MIME names clients
/// use. It is Mastodon's `supported_mime_types` list minus HEIC/HEIF/AVIF,
/// which we reject on the way in, plus JPEG XL, which we accept.
///
/// This must never be empty. Clients treat an empty array as "nothing is
/// allowed" rather than "unknown": Phanpy's `isMimeTypeSupported` only skips
/// the check for a missing list, so an empty one made it refuse every file
/// with "not supported" before it ever reached us.
const SUPPORTED_MEDIA_MIME_TYPES: &[&str] = &[
    // Still images, decoded in-request.
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/jxl",
    // Video containers (ffprobe `mov,mp4,...` / `matroska,webm` / `ogg`).
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "video/ogg",
    "video/x-ms-asf",
    // Audio containers (`mp3`, `flac`, `wav`, `aac`, `ogg`, mp4 family).
    "audio/mpeg",
    "audio/mp3",
    "audio/ogg",
    "audio/vorbis",
    "audio/flac",
    "audio/aac",
    "audio/wav",
    "audio/wave",
    "audio/x-wav",
    "audio/webm",
    "audio/mp4",
    "audio/m4a",
    "audio/x-m4a",
    "audio/3gpp",
];

/// The alt-text limit every composer surface enforces (`maxlength="1500"` on
/// the web composer and the profile image-description fields).
const MEDIA_DESCRIPTION_LIMIT: u32 = 1500;

fn media_attachments_config() -> Value {
    json!({
        "supported_mime_types": SUPPORTED_MEDIA_MIME_TYPES,
        "description_limit": MEDIA_DESCRIPTION_LIMIT,
        "image_size_limit": 16_777_216,
        "image_matrix_limit": 33_177_600,
        "video_size_limit": 103_809_024,
        "video_frame_rate_limit": 120,
        "video_matrix_limit": 8_294_400,
    })
}

fn polls_config(settings: &InstanceSettings) -> Value {
    json!({
        "max_options": settings.poll_max_options,
        "max_characters_per_option": 50,
        "min_expiration": 300,
        "max_expiration": 2_629_746,
    })
}
