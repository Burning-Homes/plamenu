//! Status translation (M25) — Mastodon's status-translation feature.
//! A pluggable, operator-configured backend
//! (`DeepL`, `LibreTranslate`, or an OpenAI-compatible server such as
//! llama.cpp hosting Hy-MT2) translates a status' content, content warning,
//! poll options and media descriptions into the requesting user's locale.
//!
//! Caching is layered: a bounded in-memory
//! cache in [`crate::state::TranslationCache`] over the persistent
//! `status_translations` table, so each (status, target) pair is translated
//! once per edit ever — across restarts and across viewers. The backend's
//! `source → [target]` language map is held in memory for 7 days, standing
//! in for Mastodon's server-side language-map cache.
//!
//! Deliberate simplification vs Mastodon: custom-emoji shortcodes are *not*
//! wrapped in `<span translate="no">` before translation. Both backends run in
//! HTML mode and leave bare `:shortcode:` tokens intact, so the span surgery
//! Mastodon does around them is belt-and-suspenders we skip to keep this
//! module dependency-free.

use std::collections::BTreeMap;
use std::sync::Arc;

use plamenu_ap::text::{escape_html, sanitize_remote_html, sanitize_remote_text};
use plamenu_db::status::Status;
use plamenu_db::status_translation::{self, NewStatusTranslation, StatusTranslation};
use plamenu_db::{media, poll};
use plamenu_federation::{FederationError, HttpMethod, ServiceResponse};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{DeepLPlan, TranslationConfig};
use crate::error::ApiError;
use crate::federation::ServiceRequest;
use crate::state::AppState;

/// The auto-detect / unknown source-language key in the language map, standing
/// in for Mastodon's `nil` source key (a `BTreeMap` can't key on absence).
const AUTO_KEY: &str = "";

/// Per-request timeout for translate calls. The federation client's blanket
/// 15s killed longer posts on a CPU model backend (~26 tok/s ≈ 12s+ for a
/// long post); language-list fetches keep the default.
const TRANSLATE_TIMEOUT_SECS: u64 = 180;

/// A failure talking to the translation backend, mirroring Mastodon's
/// `TranslationService::Error` hierarchy.
#[derive(Debug)]
pub enum TranslationError {
    /// No backend configured (`translation.enabled: false`).
    NotConfigured,
    /// The backend answered 429.
    TooManyRequests,
    /// The backend answered 403 (`LibreTranslate`) / 456 (`DeepL`) — over quota.
    QuotaExceeded,
    /// A malformed response, an unexpected status, or a transport error.
    Unexpected,
}

impl From<TranslationError> for ApiError {
    fn from(err: TranslationError) -> Self {
        match err {
            // Mastodon rescues `NotConfiguredError` with `not_found`.
            TranslationError::NotConfigured => ApiError::NotFound,
            TranslationError::TooManyRequests => ApiError::ServiceUnavailable(
                "There have been too many requests to the translation service recently.".to_owned(),
            ),
            TranslationError::QuotaExceeded => ApiError::ServiceUnavailable(
                "The server-wide usage quota for the translation service has been exceeded."
                    .to_owned(),
            ),
            TranslationError::Unexpected => {
                ApiError::ServiceUnavailable("The translation service is unavailable.".to_owned())
            }
        }
    }
}

impl From<FederationError> for TranslationError {
    fn from(_: FederationError) -> Self {
        TranslationError::Unexpected
    }
}

/// One translated fragment returned by the backend.
struct Translated {
    text: String,
    detected_source_language: Option<String>,
    provider: String,
}

/// Whether a translation backend is configured at all — drives
/// `translation.enabled` in the instance document.
#[must_use]
pub fn configured(state: &AppState) -> bool {
    state.config.translation.is_some()
}

/// The backend's `source → [target]` language map, cached for 7 days. Returns
/// an empty map when no backend is configured (the instance endpoint serves
/// `{}`), so callers never special-case that.
pub async fn language_map(
    state: &AppState,
) -> Result<Arc<BTreeMap<String, Vec<String>>>, TranslationError> {
    if let Some(cached) = state.translation_cache.languages() {
        return Ok(cached);
    }
    let map = match &state.config.translation {
        None => BTreeMap::new(),
        Some(TranslationConfig::LibreTranslate { endpoint, api_key }) => {
            libre_languages(state, endpoint, api_key.as_deref()).await?
        }
        Some(TranslationConfig::DeepL { plan, api_key }) => {
            deepl_languages(state, *plan, api_key).await?
        }
        Some(TranslationConfig::OpenAi { languages, .. }) => openai_languages(languages),
    };
    let map = Arc::new(map);
    state.translation_cache.store_languages(Arc::clone(&map));
    Ok(map)
}

/// The language map for web-render eligibility: `None` when no backend is
/// configured or its list can't be fetched — the Translate control then
/// hides rather than offering requests that would fail.
pub async fn web_language_map(state: &AppState) -> Option<Arc<BTreeMap<String, Vec<String>>>> {
    state.config.translation.as_ref()?;
    language_map(state).await.ok()
}

/// The user-facing line for a failed translation — specific where the error
/// is (unsupported language, rate limit, quota), generic otherwise.
#[must_use]
pub fn error_message(error: &ApiError) -> String {
    match error {
        ApiError::Forbidden(message) | ApiError::ServiceUnavailable(message) => message.clone(),
        ApiError::TooManyRequests => {
            "Too many translation requests — please wait a moment.".to_owned()
        }
        _ => "Translation is unavailable right now.".to_owned(),
    }
}

/// The instance document's `translation_languages` map: `{}` when no backend
/// is configured or its language list can't be fetched (Mastodon degrades the
/// same way rather than failing the whole instance response).
pub async fn languages_json(state: &AppState) -> Value {
    match language_map(state).await {
        Ok(map) => {
            let object: serde_json::Map<String, Value> = map
                .iter()
                .map(|(source, targets)| {
                    // Mastodon renames the `nil` source key to `und`.
                    let key = if source == AUTO_KEY { "und" } else { source };
                    (key.to_owned(), json!(targets))
                })
                .collect();
            Value::Object(object)
        }
        Err(_) => json!({}),
    }
}

/// Translates `status` into `target_language` for `viewer_account_id`,
/// returning the Mastodon `Translation` entity. `target_language` is the
/// requester's locale (e.g. `en` or `pt-BR`); it is narrowed to a base subtag
/// when the backend has no exact match, matching Mastodon's narrowing.
///
/// Layered lookup (`TRANSLATION_CACHE_PLAN.md`): the in-memory cache, then the
/// persistent `status_translations` row (hash-validated, so edits invalidate
/// implicitly), and only then the backend — behind a per-(status, target)
/// single-flight lock, the per-account rate limit and the operator's
/// backend-concurrency gate.
pub async fn translate_status(
    state: &AppState,
    status: &Status,
    target_language: &str,
    viewer_account_id: i64,
) -> Result<Value, ApiError> {
    let Some(backend) = &state.config.translation else {
        return Err(TranslationError::NotConfigured.into());
    };

    let languages = language_map(state).await?;
    let source_key = status.language.as_deref().unwrap_or(AUTO_KEY);

    // Permitted only for a distributable post the backend can translate into
    // the requested language (Mastodon's permit rule).
    if !is_distributable(status) {
        return Err(ApiError::Forbidden("This action is not allowed".to_owned()));
    }
    let Some(target) = permitted_target(&languages, source_key, target_language) else {
        // Specific enough to act on — "unavailable right now" hid the fact
        // that e.g. Finnish simply isn't in the backend's inventory.
        let source = status
            .language
            .as_deref()
            .map_or_else(|| "an undetected language".to_owned(), language_name);
        return Err(ApiError::Forbidden(format!(
            "Translating from {source} is not supported by the translation service"
        )));
    };

    // Gather the source fragments in a stable order so the backend's
    // positional response maps back cleanly.
    let sources = collect_sources(state, status).await?;
    if sources.is_empty() {
        // Nothing translatable; return an empty translation rather than call
        // the backend with no text.
        return Ok(parts_to_entity(
            status,
            &target,
            &TranslationParts::default(),
        ));
    }

    let source_hash = fragments_hash(&sources);
    let cache_key = format!("{}/{target}/{:x?}", status.id, source_hash);
    if let Some(cached) = state.translation_cache.translation(&cache_key) {
        return Ok(cached);
    }
    if let Some(cached) = cached_row(state, status, &target, &source_hash, backend).await? {
        let rendered = parts_to_entity(status, &target, &parts_from_row(&cached));
        state
            .translation_cache
            .store_translation(cache_key, rendered.clone());
        return Ok(rendered);
    }

    // Miss: serialize identical requests so only one pays the backend; the
    // waiters re-read the caches the winner filled. `inflight` is a function-
    // scoped RAII guard: its Drop reclaims the map slot on every exit below,
    // including the `?` early returns inside the block, so a failed pair cannot
    // leak an in-flight entry for the rest of the process.
    let inflight = state.translation_cache.inflight_lock(status.id, &target);
    let rendered = {
        let _guard = inflight.lock().lock().await;
        if let Some(cached) = state.translation_cache.translation(&cache_key) {
            Ok(cached)
        } else if let Some(cached) =
            cached_row(state, status, &target, &source_hash, backend).await?
        {
            let rendered = parts_to_entity(status, &target, &parts_from_row(&cached));
            state
                .translation_cache
                .store_translation(cache_key.clone(), rendered.clone());
            Ok(rendered)
        } else {
            crate::rate_limit::check_translation(state, viewer_account_id).await?;
            let gate = state
                .translation_cache
                .backend_gate(state.translation_backend_concurrency().await);
            let permit = gate.acquire().await;
            let result =
                backend_translate(state, backend, status, &sources, source_key, &target).await;
            drop(permit);
            let parts = result?;
            store_row(state, status, &target, &source_hash, &parts).await?;
            let rendered = parts_to_entity(status, &target, &parts);
            state
                .translation_cache
                .store_translation(cache_key, rendered.clone());
            Ok(rendered)
        }
    };
    // `inflight` drops here (or at any `?` above), reclaiming its map slot.
    rendered
}

/// Dispatches the backend call and assembles [`TranslationParts`].
async fn backend_translate(
    state: &AppState,
    backend: &TranslationConfig,
    status: &Status,
    sources: &[Source],
    source_key: &str,
    target: &str,
) -> Result<TranslationParts, ApiError> {
    let texts: Vec<&str> = sources.iter().map(|s| s.sent.as_str()).collect();
    let translations = match backend {
        TranslationConfig::LibreTranslate { endpoint, api_key } => {
            libre_translate(
                state,
                endpoint,
                api_key.as_deref(),
                &texts,
                source_key,
                target,
            )
            .await?
        }
        TranslationConfig::DeepL { plan, api_key } => {
            deepl_translate(state, *plan, api_key, &texts, source_key, target).await?
        }
        TranslationConfig::OpenAi {
            endpoint,
            model,
            api_key,
            ..
        } => openai_translate(state, endpoint, model, api_key.as_deref(), &texts, target).await?,
    };
    if translations.len() != sources.len() {
        return Err(TranslationError::Unexpected.into());
    }
    let detected = translations
        .first()
        .and_then(|t| t.detected_source_language.as_deref())
        .or(status.language.as_deref());
    Ok(assemble_parts(&translations, sources, detected))
}

/// The persistent-cache row when it is still valid: the stored hash must
/// match the current fragments (edits change the hash), and — behind the
/// `translation_refresh_on_provider_change` knob — the stored provider must
/// match the configured backend.
async fn cached_row(
    state: &AppState,
    status: &Status,
    target: &str,
    source_hash: &[u8],
    backend: &TranslationConfig,
) -> Result<Option<StatusTranslation>, ApiError> {
    let Some(row) = status_translation::find(&state.pool, status.id, target).await? else {
        return Ok(None);
    };
    if row.source_hash != source_hash {
        return Ok(None);
    }
    if state.translation_refresh_on_provider_change().await
        && row.provider != provider_label(backend)
    {
        return Ok(None);
    }
    Ok(Some(row))
}

/// The attribution string a backend stamps on its translations.
fn provider_label(backend: &TranslationConfig) -> &str {
    match backend {
        TranslationConfig::DeepL { .. } => "DeepL.com",
        TranslationConfig::LibreTranslate { .. } => "LibreTranslate",
        TranslationConfig::OpenAi { model, .. } => model,
    }
}

async fn store_row(
    state: &AppState,
    status: &Status,
    target: &str,
    source_hash: &[u8],
    parts: &TranslationParts,
) -> Result<(), ApiError> {
    let (media_ids, media_descriptions): (Vec<i64>, Vec<String>) =
        parts.media.iter().cloned().unzip();
    status_translation::upsert(
        &state.pool,
        &NewStatusTranslation {
            status_id: status.id,
            target_language: target,
            source_hash,
            provider: parts.provider.as_deref().unwrap_or(""),
            detected_source_language: parts.detected_source_language.as_deref(),
            title: &parts.title,
            content: &parts.content,
            spoiler_text: &parts.spoiler_text,
            poll_options: &parts.poll_options,
            media_ids: &media_ids,
            media_descriptions: &media_descriptions,
        },
    )
    .await?;
    Ok(())
}

/// A `public`/`unlisted` post — Mastodon's `Status#distributable?`.
fn is_distributable(status: &Status) -> bool {
    matches!(status.visibility.as_str(), "public" | "unlisted")
}

/// Whether the backend can translate `source_key` (a language code or
/// [`AUTO_KEY`]) into `target_language`, returning the (possibly
/// base-subtag-narrowed) target it should ask for. Shared by the translate
/// path and the web UI's button eligibility, so the control is only offered
/// when a request would be permitted.
#[must_use]
pub fn permitted_target(
    languages: &BTreeMap<String, Vec<String>>,
    source_key: &str,
    target_language: &str,
) -> Option<String> {
    let targets = languages.get(source_key)?;
    // Narrow `pt-BR` → `pt` when the backend only lists the base language.
    if targets.iter().any(|t| t == target_language) {
        return Some(target_language.to_owned());
    }
    let base = target_language
        .split(['-', '_'])
        .next()
        .unwrap_or(target_language);
    targets.iter().any(|t| t == base).then(|| base.to_owned())
}

/// A language code's readable English name, falling back to the raw code.
fn language_name(code: &str) -> String {
    crate::languages::find(code).map_or_else(|| code.to_owned(), |l| l.english.to_owned())
}

/// Applies a `Translation` entity onto a rendered `Status` entity in place —
/// the web client's translated thread view: content, content warning,
/// poll option titles and media descriptions are swapped for the translated
/// text, and the attribution (provider, detected source, target) is stashed
/// under `_translation` for the renderer's "Translated from …" line.
pub fn apply_to_entity(entity: &mut Value, translation: &Value) {
    let text = |key: &str| {
        translation
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    // Only overwrite fields the translation actually carries: a fragment the
    // source never had comes back empty and must not blank the original.
    //
    // The rendered entity's `content` is the *folded* form — title heading
    // and external-link pill included (`fold_typed_content`) — so a
    // translated body and/or title must be refolded the same way rather than
    // pasted over it.
    let translated_title = text("title").map(str::to_owned);
    let translated_content = text("content").map(str::to_owned);
    if translated_title.is_some() || translated_content.is_some() {
        let title = translated_title.clone().or_else(|| {
            entity
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let external_url = entity
            .get("external_url")
            .and_then(Value::as_str)
            .map(str::to_owned);
        entity["content"] = json!(crate::entities::fold_typed_content(
            translated_content.as_deref().unwrap_or(""),
            title.as_deref(),
            external_url.as_deref(),
        ));
        if let Some(title) = translated_title {
            entity["title"] = json!(title);
        }
    }
    if let Some(spoiler) = text("spoiler_text") {
        entity["spoiler_text"] = json!(spoiler);
    }
    if let Some(options) = translation
        .get("poll")
        .and_then(|poll| poll.get("options"))
        .and_then(Value::as_array)
        && let Some(targets) = entity
            .get_mut("poll")
            .and_then(|poll| poll.get_mut("options"))
            .and_then(Value::as_array_mut)
    {
        for (target, translated) in targets.iter_mut().zip(options) {
            if let Some(title) = translated.get("title").and_then(Value::as_str) {
                target["title"] = json!(title);
            }
        }
    }
    if let Some(attachments) = translation
        .get("media_attachments")
        .and_then(Value::as_array)
    {
        for translated in attachments {
            let (Some(id), Some(description)) = (
                translated.get("id").and_then(Value::as_str),
                translated.get("description").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(target) = entity
                .get_mut("media_attachments")
                .and_then(Value::as_array_mut)
                .and_then(|list| {
                    list.iter_mut()
                        .find(|m| m.get("id").and_then(Value::as_str) == Some(id))
                })
            {
                target["description"] = json!(description);
            }
        }
    }
    entity["_translation"] = json!({
        "provider": translation.get("provider").cloned().unwrap_or(Value::Null),
        "detected_source_language":
            translation.get("detected_source_language").cloned().unwrap_or(Value::Null),
        "language": translation.get("language").cloned().unwrap_or(Value::Null),
    });
}

/// Which field a source fragment came from, so the response maps back.
enum SourceKind {
    /// The post title (Lemmy-style Pages, group submissions) — often the
    /// entire meaning of a title-only Page.
    Title,
    Content,
    SpoilerText,
    PollOption,
    Media(i64),
}

/// A source fragment: the text sent to the backend (HTML-escaped for the
/// plain-text fields so the HTML-mode backend doesn't mangle them) plus where
/// it belongs in the result.
struct Source {
    kind: SourceKind,
    sent: String,
}

/// Assembles the translatable fragments of a status in Mastodon's order:
/// content, spoiler text, each poll option, each media description.
async fn collect_sources(state: &AppState, status: &Status) -> Result<Vec<Source>, ApiError> {
    let mut sources = Vec::new();
    if let Some(title) = status.title.as_deref().filter(|t| !t.trim().is_empty()) {
        sources.push(Source {
            kind: SourceKind::Title,
            sent: escape_html(title.trim()),
        });
    }
    if !status.content.is_empty() {
        sources.push(Source {
            kind: SourceKind::Content,
            // Content is already sanitized HTML; send it verbatim.
            sent: status.content.clone(),
        });
    }
    if !status.spoiler_text.is_empty() {
        sources.push(Source {
            kind: SourceKind::SpoilerText,
            sent: escape_html(&status.spoiler_text),
        });
    }
    if let Some(poll) = poll::find_by_status(&state.pool, status.id).await? {
        for option in &poll.options {
            sources.push(Source {
                kind: SourceKind::PollOption,
                sent: escape_html(option),
            });
        }
    }
    let media = media::for_statuses(&state.pool, &[status.id]).await?;
    if let Some(attachments) = media.get(&status.id) {
        for attachment in attachments {
            if let Some(description) = attachment.description.as_deref().filter(|d| !d.is_empty()) {
                sources.push(Source {
                    kind: SourceKind::Media(attachment.id),
                    sent: escape_html(description),
                });
            }
        }
    }
    Ok(sources)
}

/// A translation's sanitized payload, independent of its rendering: assembled
/// from a backend response, persisted to `status_translations`, and rebuilt
/// from a cached row — one shape for all three.
#[derive(Default)]
struct TranslationParts {
    detected_source_language: Option<String>,
    provider: Option<String>,
    title: String,
    content: String,
    spoiler_text: String,
    poll_options: Vec<String>,
    /// `(media_id, translated description)` pairs.
    media: Vec<(i64, String)>,
}

/// Sanitizes the backend's positional results into [`TranslationParts`].
fn assemble_parts(
    translations: &[Translated],
    sources: &[Source],
    detected_source_language: Option<&str>,
) -> TranslationParts {
    let mut parts = TranslationParts {
        detected_source_language: detected_source_language.map(str::to_owned),
        provider: translations.first().map(|t| t.provider.clone()),
        ..TranslationParts::default()
    };
    for (source, translated) in sources.iter().zip(translations) {
        match source.kind {
            SourceKind::Title => {
                parts.title = sanitize_remote_text(&translated.text);
            }
            SourceKind::Content => {
                parts.content = sanitize_remote_html(&translated.text);
            }
            SourceKind::SpoilerText => {
                parts.spoiler_text = sanitize_remote_text(&translated.text);
            }
            SourceKind::PollOption => {
                parts
                    .poll_options
                    .push(sanitize_remote_text(&translated.text));
            }
            SourceKind::Media(id) => {
                parts
                    .media
                    .push((id, sanitize_remote_text(&translated.text)));
            }
        }
    }
    parts
}

/// Rehydrates [`TranslationParts`] from a persistent-cache row.
fn parts_from_row(row: &StatusTranslation) -> TranslationParts {
    TranslationParts {
        detected_source_language: row.detected_source_language.clone(),
        provider: (!row.provider.is_empty()).then(|| row.provider.clone()),
        title: row.title.clone(),
        content: row.content.clone(),
        spoiler_text: row.spoiler_text.clone(),
        poll_options: row.poll_options.clone(),
        media: row
            .media_ids
            .iter()
            .copied()
            .zip(row.media_descriptions.iter().cloned())
            .collect(),
    }
}

/// Builds the Mastodon `Translation` entity from [`TranslationParts`].
fn parts_to_entity(status: &Status, target: &str, parts: &TranslationParts) -> Value {
    let media_attachments: Vec<Value> = parts
        .media
        .iter()
        .map(|(id, description)| json!({ "id": id.to_string(), "description": description }))
        .collect();
    let mut entity = json!({
        "detected_source_language": parts.detected_source_language,
        "language": target,
        "provider": parts.provider,
        "spoiler_text": parts.spoiler_text,
        "content": parts.content,
        // Plamenu extension (group posts carry a title the body doesn't).
        "title": parts.title,
        "media_attachments": media_attachments,
    });
    if !parts.poll_options.is_empty() {
        entity["poll"] = json!({
            "id": status.id.to_string(),
            "options": parts.poll_options.iter()
                .map(|title| json!({ "title": title }))
                .collect::<Vec<_>>(),
        });
    }
    entity
}

/// A stable sha256 over the exact texts sent to the backend (length-prefixed
/// so fragment boundaries can't collide), shared by the in-memory key and the
/// persistent row's `source_hash` — an edit changes it, invalidating both.
fn fragments_hash(sources: &[Source]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for source in sources {
        hasher.update(u64::to_be_bytes(source.sent.len() as u64));
        hasher.update(source.sent.as_bytes());
    }
    hasher.finalize().to_vec()
}

// ---------------------------------------------------------------------------
// LibreTranslate backend

async fn libre_translate(
    state: &AppState,
    endpoint: &str,
    api_key: Option<&str>,
    texts: &[&str],
    source_language: &str,
    target_language: &str,
) -> Result<Vec<Translated>, TranslationError> {
    let source = if source_language.is_empty() {
        "auto"
    } else {
        source_language
    };
    let body = json!({
        "q": texts,
        "source": source,
        "target": target_language,
        "format": "html",
        "api_key": api_key.unwrap_or_default(),
    });
    let response = state
        .federation
        .service_request(ServiceRequest {
            method: HttpMethod::Post,
            url: format!("{endpoint}/translate"),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: Some(body.to_string()),
            timeout_secs: Some(TRANSLATE_TIMEOUT_SECS),
        })
        .await?;
    let json = libre_ok(&response)?;
    let data: Value = serde_json::from_str(&json).map_err(|_| TranslationError::Unexpected)?;
    let texts = data
        .get("translatedText")
        .and_then(Value::as_array)
        .ok_or(TranslationError::Unexpected)?;
    let detected = data.get("detectedLanguage").and_then(Value::as_array);
    Ok(texts
        .iter()
        .enumerate()
        .map(|(index, text)| Translated {
            text: text.as_str().unwrap_or_default().to_owned(),
            detected_source_language: detected
                .and_then(|d| d.get(index))
                .and_then(|entry| entry.get("language"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| (!source_language.is_empty()).then(|| source_language.to_owned())),
            provider: "LibreTranslate".to_owned(),
        })
        .collect())
}

async fn libre_languages(
    state: &AppState,
    endpoint: &str,
    _api_key: Option<&str>,
) -> Result<BTreeMap<String, Vec<String>>, TranslationError> {
    let response = state
        .federation
        .service_request(ServiceRequest {
            method: HttpMethod::Get,
            url: format!("{endpoint}/languages"),
            headers: Vec::new(),
            body: None,
            timeout_secs: None,
        })
        .await?;
    let json = libre_ok(&response)?;
    let data: Value = serde_json::from_str(&json).map_err(|_| TranslationError::Unexpected)?;
    let entries = data.as_array().ok_or(TranslationError::Unexpected)?;
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in entries {
        let Some(code) = entry.get("code").and_then(Value::as_str) else {
            continue;
        };
        let targets: Vec<String> = entry
            .get("targets")
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(Value::as_str)
                    .filter(|target| *target != code)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        map.insert(code.to_owned(), targets);
    }
    // The `auto` source can reach any target the backend offers.
    let mut all: Vec<String> = map.values().flatten().cloned().collect();
    all.sort_unstable();
    all.dedup();
    map.insert(AUTO_KEY.to_owned(), all);
    Ok(map)
}

/// Maps a `LibreTranslate` HTTP status onto the error hierarchy, returning the
/// body on success.
fn libre_ok(response: &ServiceResponse) -> Result<String, TranslationError> {
    match response.status {
        429 => Err(TranslationError::TooManyRequests),
        403 => Err(TranslationError::QuotaExceeded),
        200..=299 => Ok(response.body.clone()),
        _ => Err(TranslationError::Unexpected),
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible backend (llama.cpp llama-server, built for Hy-MT2)

/// Translates the fragments with as few chat completions as possible: a
/// multi-fragment post is sent as ONE structured-JSON request (Hy-MT2's
/// documented structured-data prompt), with a per-fragment fallback when the
/// model's JSON doesn't parse back (observed ~1/10, e.g. unescaped quotes).
/// Single fragments use the plain prompt directly.
async fn openai_translate(
    state: &AppState,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
    texts: &[&str],
    target_language: &str,
) -> Result<Vec<Translated>, TranslationError> {
    if texts.len() >= 2
        && let Some(results) =
            openai_translate_structured(state, endpoint, model, api_key, texts, target_language)
                .await?
    {
        return Ok(results);
    }
    let mut results = Vec::with_capacity(texts.len());
    for text in texts {
        let prompt = format!(
            "Translate the following text into {}. Note that you should \
             **only output the translated result without any additional \
             explanation**:\n\n{text}",
            language_english_name(target_language),
        );
        let reply = openai_chat(state, endpoint, model, api_key, &prompt).await?;
        results.push(Translated {
            text: reply.trim().to_owned(),
            // Chat models don't report a detected source; callers fall back
            // to the status' declared language.
            detected_source_language: None,
            provider: model.to_owned(),
        });
    }
    Ok(results)
}

/// One structured-JSON request translating every fragment at once, keyed
/// `f1`..`fN`. `Ok(None)` means the model's reply wasn't usable JSON — the
/// caller retries fragment-by-fragment; transport/HTTP errors propagate
/// (retrying wouldn't help those).
async fn openai_translate_structured(
    state: &AppState,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
    texts: &[&str],
    target_language: &str,
) -> Result<Option<Vec<Translated>>, TranslationError> {
    let mut data = serde_json::Map::new();
    for (index, text) in texts.iter().enumerate() {
        data.insert(format!("f{}", index + 1), json!(text));
    }
    let prompt = format!(
        "### Task\nTranslate the user-facing text within the following JSON \
         data into {target}.\n\n### Strict Rules\n1. **Structure \
         Preservation:** You MUST preserve the original JSON data structure, \
         keys, and formatting exactly as they are.\n2. **Selective \
         Translation:** Translate ONLY the values.\n3. **Strict \
         Non-Translation:** NEVER translate or alter keys, HTML tags, \
         attributes, URLs, @mentions or #hashtags. Output only the JSON, no \
         explanation.\n\n### Source Data\n{data}",
        target = language_english_name(target_language),
        data = Value::Object(data),
    );
    let reply = openai_chat(state, endpoint, model, api_key, &prompt).await?;
    // Tolerate a code fence or stray prose around the object.
    let (Some(start), Some(end)) = (reply.find('{'), reply.rfind('}')) else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&reply[start..=end]) else {
        return Ok(None);
    };
    let mut results = Vec::with_capacity(texts.len());
    for index in 1..=texts.len() {
        let Some(text) = parsed.get(format!("f{index}")).and_then(Value::as_str) else {
            return Ok(None);
        };
        results.push(Translated {
            text: text.trim().to_owned(),
            detected_source_language: None,
            provider: model.to_owned(),
        });
    }
    Ok(Some(results))
}

/// One chat completion against the OpenAI-compatible server, returning the
/// assistant text.
async fn openai_chat(
    state: &AppState,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: &str,
) -> Result<String, TranslationError> {
    let body = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": [{ "role": "user", "content": prompt }],
    });
    let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    if let Some(key) = api_key {
        headers.push(("Authorization".to_owned(), format!("Bearer {key}")));
    }
    let response = state
        .federation
        .service_request(ServiceRequest {
            method: HttpMethod::Post,
            url: format!("{endpoint}/v1/chat/completions"),
            headers,
            body: Some(body.to_string()),
            timeout_secs: Some(TRANSLATE_TIMEOUT_SECS),
        })
        .await?;
    let json = openai_ok(&response)?;
    let data: Value = serde_json::from_str(&json).map_err(|_| TranslationError::Unexpected)?;
    data.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(TranslationError::Unexpected)
}

/// The static any-to-any language map for the configured model.
fn openai_languages(languages: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut map = BTreeMap::new();
    for source in std::iter::once(AUTO_KEY).chain(languages.iter().map(String::as_str)) {
        map.insert(
            source.to_owned(),
            languages
                .iter()
                .filter(|target| *target != source)
                .cloned()
                .collect(),
        );
    }
    map
}

/// The English language name the Hy-MT2 prompt template expects ("Translate
/// the following text into German…"); falls back to the raw code for
/// languages outside the inventory.
fn language_english_name(code: &str) -> &str {
    crate::languages::find(code).map_or(code, |language| language.english)
}

/// Maps an OpenAI-compatible server's HTTP status onto the error hierarchy.
fn openai_ok(response: &ServiceResponse) -> Result<String, TranslationError> {
    match response.status {
        429 => Err(TranslationError::TooManyRequests),
        200..=299 => Ok(response.body.clone()),
        _ => Err(TranslationError::Unexpected),
    }
}

// ---------------------------------------------------------------------------
// DeepL backend

fn deepl_base_url(plan: DeepLPlan) -> &'static str {
    match plan {
        DeepLPlan::Free => "https://api-free.deepl.com",
        DeepLPlan::Pro => "https://api.deepl.com",
    }
}

async fn deepl_translate(
    state: &AppState,
    plan: DeepLPlan,
    api_key: &str,
    texts: &[&str],
    source_language: &str,
    target_language: &str,
) -> Result<Vec<Translated>, TranslationError> {
    let mut form: Vec<(&str, String)> = texts
        .iter()
        .map(|text| ("text", (*text).to_owned()))
        .collect();
    if !source_language.is_empty() {
        form.push(("source_lang", source_language.to_uppercase()));
    }
    form.push(("target_lang", target_language.to_owned()));
    form.push(("tag_handling", "html".to_owned()));
    let body = serde_urlencoded::to_string(&form).map_err(|_| TranslationError::Unexpected)?;

    let response = state
        .federation
        .service_request(ServiceRequest {
            method: HttpMethod::Post,
            url: format!("{}/v2/translate", deepl_base_url(plan)),
            headers: vec![
                (
                    "Authorization".to_owned(),
                    format!("DeepL-Auth-Key {api_key}"),
                ),
                (
                    "Content-Type".to_owned(),
                    "application/x-www-form-urlencoded".to_owned(),
                ),
            ],
            body: Some(body),
            timeout_secs: Some(TRANSLATE_TIMEOUT_SECS),
        })
        .await?;
    let json = deepl_ok(&response)?;
    let data: Value = serde_json::from_str(&json).map_err(|_| TranslationError::Unexpected)?;
    let translations = data
        .get("translations")
        .and_then(Value::as_array)
        .ok_or(TranslationError::Unexpected)?;
    Ok(translations
        .iter()
        .map(|translation| Translated {
            text: translation
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            detected_source_language: translation
                .get("detected_source_language")
                .and_then(Value::as_str)
                .map(str::to_lowercase),
            provider: "DeepL.com".to_owned(),
        })
        .collect())
}

async fn deepl_languages(
    state: &AppState,
    plan: DeepLPlan,
    api_key: &str,
) -> Result<BTreeMap<String, Vec<String>>, TranslationError> {
    let source = deepl_fetch_languages(state, plan, api_key, "source").await?;
    // DeepL supports EN and PT as targets but no longer returns them (they are
    // deprecated in favour of the regional variants), so add them explicitly.
    let mut targets = vec!["en".to_owned(), "pt".to_owned()];
    targets.extend(deepl_fetch_languages(state, plan, api_key, "target").await?);
    targets.sort_unstable();
    targets.dedup();

    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // The auto source plus every listed source language.
    for source_lang in std::iter::once(AUTO_KEY.to_owned()).chain(source) {
        let reachable: Vec<String> = targets
            .iter()
            .filter(|t| **t != source_lang)
            .cloned()
            .collect();
        map.insert(source_lang, reachable);
    }
    Ok(map)
}

async fn deepl_fetch_languages(
    state: &AppState,
    plan: DeepLPlan,
    api_key: &str,
    kind: &str,
) -> Result<Vec<String>, TranslationError> {
    let response = state
        .federation
        .service_request(ServiceRequest {
            method: HttpMethod::Get,
            url: format!("{}/v2/languages?type={kind}", deepl_base_url(plan)),
            headers: vec![(
                "Authorization".to_owned(),
                format!("DeepL-Auth-Key {api_key}"),
            )],
            body: None,
            timeout_secs: None,
        })
        .await?;
    let json = deepl_ok(&response)?;
    let data: Value = serde_json::from_str(&json).map_err(|_| TranslationError::Unexpected)?;
    let entries = data.as_array().ok_or(TranslationError::Unexpected)?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("language").and_then(Value::as_str))
        .map(normalize_deepl_language)
        .collect())
}

/// `DeepL` returns `EN-GB`; normalize to Mastodon's `en-GB` shape (lowercase
/// primary subtag, uppercase region).
fn normalize_deepl_language(language: &str) -> String {
    let mut parts = language.split(['-', '_']);
    let primary = parts.next().unwrap_or_default().to_lowercase();
    match parts.next() {
        Some(region) => format!("{primary}-{}", region.to_uppercase()),
        None => primary,
    }
}

/// Maps a `DeepL` HTTP status onto the error hierarchy, returning the body on
/// success. `DeepL` signals over-quota with `456`.
fn deepl_ok(response: &ServiceResponse) -> Result<String, TranslationError> {
    match response.status {
        429 => Err(TranslationError::TooManyRequests),
        456 => Err(TranslationError::QuotaExceeded),
        200..=299 => Ok(response.body.clone()),
        _ => Err(TranslationError::Unexpected),
    }
}
