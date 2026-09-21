//! Ingestion of remote status objects - shared by the inbox (`Create`
//! activities) and search-by-URL (freshly fetched objects), so a status is
//! stored identically no matter how it reached us.

use std::collections::{HashMap, HashSet};

use plamenu_ap::activity::{id_of, one_or_many, visibility_from_addressing};
use plamenu_ap::urls;
use plamenu_db::account::{self, Account};
use plamenu_db::status::NewRemoteStatus;
use plamenu_db::{media, mention, notification, status, status_edit, tag};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::error::ApiError;
use crate::remote::{host_of, refresh_remote_actor};

/// The route by which a remote status reached canonical storage. Cold history
/// preserves dependencies needed to render the current object, but performs no
/// optional enrichment or user-visible delivery effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteIngestContext {
    Delivery,
    ExplicitResolution,
    History,
}

/// Canonical row plus ownership of one-shot live-delivery effects. Inbox
/// callers must fan out/stream only when `delivery_effects` is true.
pub struct RemoteIngestResult {
    pub status: status::Status,
    pub delivery_effects: bool,
    /// Fresh local recipients gathered while storing this delivery. The inbox
    /// combines these with quote and per-follow reasons before one notification
    /// write, so the built-in client never observes intermediate duplicates.
    pub mention_notify: Vec<i64>,
    pub quote_notify: Option<i64>,
}

impl RemoteIngestContext {
    fn provenance(self) -> status::IngestProvenance {
        match self {
            Self::Delivery => status::IngestProvenance::Delivery,
            Self::ExplicitResolution => status::IngestProvenance::ExplicitResolution,
            Self::History => status::IngestProvenance::History,
        }
    }

    fn is_history(self) -> bool {
        self == Self::History
    }
}

/// How many missing ancestors one inbound reply may pull in. Deeper threads
/// heal progressively as further replies arrive.
const MAX_THREAD_ANCESTORS: usize = 5;
/// Match Mastodon's bounded synchronous quote verification: a quote may pull in
/// a missing quoted post, but recursive quote chains stop quickly.
const MAX_QUOTE_TARGET_FETCH_DEPTH: usize = 2;

/// Sanity cap on how many attachments an inbound federated Note may carry.
/// Unlike the local `max_media_attachments` setting (which governs what our
/// own users may post), remote servers legitimately allow far more — Pleroma
/// permits 1000, `GoToSocial` doesn't cap inbound at all — so we preserve remote
/// media rather than truncating to the local limit the way Mastodon does. This
/// mirrors `polls::MAX_REMOTE_OPTIONS`: generous enough that no honest post is
/// affected, bounded only to blunt a hostile server. Both the persistence path
/// (`store_remote_attachments`) and the edit change-detection path
/// (`attachment_pairs`) must use this same value or edit-diffing breaks.
const MAX_REMOTE_ATTACHMENTS: usize = 100;

/// Sanity cap on how many distinct local recipients one inbound Note's
/// `to`/`cc`/`audience` addressing may resolve to. A signed remote actor can
/// pack thousands of short local user URLs into the ordinary ~2 MiB inbox body;
/// without a cap each distinct name forces a serial account lookup and — for
/// `to` recipients — a persisted mention row and notification.
/// A cap of 50 is far above any honest audience (Mention tags themselves are
/// capped at 20 in `store_remote_tags`) yet blunts the amplification.
const MAX_AUDIENCE_RECIPIENTS: usize = 50;

const SUPPORTED_STATUS_TYPES: &[&str] = &["Note", "Question"];
/// Non-Note object types stored as statuses: the types Mastodon
/// converts, plus `Document`, which Sharkey and `GoToSocial`
/// both accept as a top-level post; Mastodon
/// drops it. Unlike Mastodon — which compacts all of these to a
/// title-plus-link stub and discards the body — each type is ingested
/// natively (full body, real title, per-type extras). Hubzilla publishes files
/// as top-level `Document` objects, so that type follows the same native path.
const CONVERTED_STATUS_TYPES: &[&str] = &[
    "Image", "Audio", "Video", "Article", "Page", "Event", "Document",
];

/// Resolves a status reference: a stored remote URI, or one of our own
/// status URLs.
pub async fn resolve_status_ref(
    state: &AppState,
    uri: &str,
) -> Result<Option<status::Status>, ApiError> {
    if let Some(found) = status::find_by_uri(&state.pool, uri).await? {
        return Ok(Some(found));
    }
    let local_ref = if let Some((account_id, status_id)) =
        urls::parse_local_numeric_status_url(&state.config.domain, uri)
    {
        Some((
            format!("https://{}/ap/accounts/{account_id}", state.config.domain),
            status_id,
        ))
    } else {
        urls::parse_local_status_url(&state.config.domain, uri).map(|(username, status_id)| {
            (
                format!("https://{}/users/{username}", state.config.domain),
                status_id,
            )
        })
    };
    if let Some((actor_uri, status_id)) = local_ref
        && let Some(owner) =
            crate::local_identity::find_actor(&state.pool, &state.config.domain, &actor_uri).await?
    {
        return status::find_local(&state.pool, owner.id, status_id)
            .await
            .map_err(ApiError::from);
    }
    Ok(None)
}

/// Resolves a status by URI, fetching and ingesting a not-yet-stored remote
/// post if needed — Mastodon's bookmark-import path (stored/local first, then
/// `FetchRemoteStatusService`). A local URI we do not have cannot be fetched
/// into existence, so it resolves to `None`.
pub async fn resolve_or_fetch_status(
    state: &AppState,
    uri: &str,
) -> Result<Option<status::Status>, ApiError> {
    if let Some(found) = resolve_status_ref(state, uri).await? {
        return Ok(Some(found));
    }
    if uri.starts_with(&format!("https://{}/", state.config.domain)) {
        return Ok(None);
    }
    let Some((object, author)) = fetch_remote_status_object(state, uri).await? else {
        return Ok(None);
    };
    Ok(Some(
        ingest_remote_note_in_context(
            state,
            &author,
            &object,
            RemoteIngestContext::ExplicitResolution,
        )
        .await?,
    ))
}

/// Re-fetches an already-stored remote status from its origin and applies the
/// authoritative copy as an edit — how a forwarded `Create`/`Update` copy is
/// verified instead of being trusted (there are no Linked Data Signatures).
/// `None` when the origin is unreachable or no longer serves a matching note.
pub async fn refresh_remote_status(
    state: &AppState,
    existing: &status::Status,
    uri: &str,
) -> Result<Option<status::Status>, ApiError> {
    let Some((object, author)) = fetch_remote_status_object(state, uri).await? else {
        return Ok(None);
    };
    if author.id != existing.account_id {
        return Ok(None);
    }
    crate::polls::refresh_remote_poll(state, existing, &object).await?;
    let updated = update_remote_note(state, &author, existing, &object).await?;
    Ok(Some(updated))
}

/// Resolves the parent of an inbound reply, backfilling missing remote
/// ancestors so threads arrive connected instead of as orphaned replies.
///
/// Unknown ancestors are fetched up the `inReplyTo` chain (capped at
/// [`MAX_THREAD_ANCESTORS`], cycle-safe) and ingested root-first, so every
/// stored note finds its parent already present. Backfill is best-effort:
/// an unreachable or invalid ancestor truncates the thread there and the
/// reply is still stored.
async fn resolve_thread_parent(
    state: &AppState,
    parent_uri: &str,
    quote_depth: usize,
    context: RemoteIngestContext,
) -> Result<Option<status::Status>, ApiError> {
    let local_prefix = format!("https://{}/", state.config.domain);
    // Leaf-first: chain[i] is a reply to chain[i + 1].
    let mut chain: Vec<(Value, Account)> = Vec::new();
    // The first already-known status above the missing stretch, if any.
    let mut anchor: Option<status::Status> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = Some(parent_uri.to_owned());

    while let Some(uri) = cursor.take() {
        if let Some(known) = resolve_status_ref(state, &uri).await? {
            anchor = Some(known);
            break;
        }
        // An unknown local URL cannot be fetched into existence, and a
        // forged chain must not make us walk in circles or without end.
        if uri.starts_with(&local_prefix)
            || seen.contains(&uri)
            || chain.len() == MAX_THREAD_ANCESTORS
        {
            break;
        }
        let Some((object, author)) = fetch_remote_status_object(state, &uri).await? else {
            break;
        };
        cursor = object.get("inReplyTo").and_then(id_of).map(str::to_owned);
        seen.push(uri);
        chain.push((object, author));
    }

    if !chain.is_empty() {
        tracing::info!(
            parent = parent_uri,
            ancestors = chain.len(),
            "backfilling thread ancestors"
        );
    }
    // Ingest root-first so each child's parent is already stored.
    let mut parent = anchor;
    for (object, author) in chain.into_iter().rev() {
        let parent_id = parent.as_ref().map(|p| p.id);
        parent = Some(
            Box::pin(store_remote_note(
                state,
                &author,
                &object,
                parent_id,
                quote_depth,
                context,
            ))
            .await?
            .status,
        );
    }
    Ok(parent)
}

/// Fetches one remote status object and resolves its author. Like search-by-URL,
/// the object must be a `Note` attributed to an actor on the note's own
/// host — a server may not speak for notes hosted elsewhere. Network
/// failures are `None` (best effort); database errors propagate.
async fn fetch_remote_status_object(
    state: &AppState,
    uri: &str,
) -> Result<Option<(Value, Account)>, ApiError> {
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        // A best-effort fetch that fails is silently dropped, but the reason
        // matters when a thread arrives orphaned: without this an unreachable
        // parent, a policy block, a 403, a non-AP content type and a malformed
        // object all look identical. `category` lets an operator tell them
        // apart (this is the whole diagnostic trail an orphaned reply leaves).
        tracing::debug!(uri, category = "policy", "remote status fetch skipped");
        return Ok(None);
    }
    // `fetch_object` verifies the returned id matches `uri`. Its error carries
    // the concrete reason — an HTTP 403, a `text/html` content type (Lemmy and
    // others serve their web page to AP requests), a signature rejection, a
    // timeout, an SSRF-policy denial — so surface it rather than discard it.
    let object = match state.federation.fetch_object(uri).await {
        Ok(object) => object,
        Err(err) => {
            tracing::info!(uri, category = "fetch", error = %err, "remote status fetch failed");
            return Ok(None);
        }
    };
    if !is_ingestible_note(&object) {
        let object_type = object.get("type").and_then(Value::as_str).unwrap_or("?");
        tracing::info!(
            uri,
            category = "not-a-note",
            object_type,
            "remote status fetch returned a non-ingestible object"
        );
        return Ok(None);
    }
    let Some(attributed_to) = plamenu_ap::activity::attributed_to_id(&object) else {
        tracing::info!(
            uri,
            category = "no-attribution",
            "remote status has no attributedTo"
        );
        return Ok(None);
    };
    if host_of(attributed_to) != host_of(uri) {
        tracing::info!(
            uri,
            category = "attribution-host-mismatch",
            attributed_to,
            "remote status attributed to an actor on another host"
        );
        return Ok(None);
    }
    let author = if let Some(known) = account::find_by_uri(&state.pool, attributed_to).await? {
        if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &known)
            .await?
        {
            return Ok(None);
        }
        known
    } else {
        if !crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            attributed_to,
        )
        .await?
        {
            return Ok(None);
        }
        let author = match state.federation.fetch_actor(attributed_to).await {
            Ok(actor) => actor,
            Err(err) => {
                tracing::info!(
                    uri,
                    category = "author-fetch",
                    attributed_to,
                    error = %err,
                    "remote status author fetch failed"
                );
                return Ok(None);
            }
        };
        refresh_remote_actor(state, &author).await?
    };
    Ok(Some((object, author)))
}

pub(crate) fn object_type_in(object: &Value, allowed: &[&str]) -> bool {
    match object.get("type") {
        Some(Value::String(kind)) => allowed.contains(&kind.as_str()),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| allowed.contains(&kind)),
        _ => false,
    }
}

fn is_converted_status_object(object: &Value) -> bool {
    object_type_in(object, CONVERTED_STATUS_TYPES)
}

/// The canonical stored `object_type` of a non-Note object, `None` for
/// `Note`/`Question` — what lands in `statuses.object_type` and drives the
/// per-type ingestion strategy.
fn stored_object_type(object: &Value) -> Option<&'static str> {
    CONVERTED_STATUS_TYPES
        .iter()
        .copied()
        .find(|kind| object_type_in(object, std::slice::from_ref(kind)))
}

/// Whether an object is a status we can store: `Note`/`Question` are stored
/// as-is, the non-Note types per [`CONVERTED_STATUS_TYPES`].
///
/// An unpublished `Event` draft is refused here rather than at each call site,
/// so every route into ingestion — inbox `Create`, relay forward, search
/// resolve, thread backfill — drops it on the same rule. See
/// [`is_draft_event`].
#[must_use]
pub fn is_ingestible_note(object: &Value) -> bool {
    if is_draft_event(object) {
        return false;
    }
    object_type_in(object, SUPPORTED_STATUS_TYPES) || is_converted_status_object(object)
}

fn mapped_text<'a>(object: &'a Value, key: &str, map_key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            object
                .get(map_key)
                .and_then(Value::as_object)
                .and_then(|map| map.values().next())
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
}

fn object_content(object: &Value) -> Option<&str> {
    mapped_text(object, "content", "contentMap")
}

fn object_summary(object: &Value) -> Option<&str> {
    mapped_text(object, "summary", "summaryMap")
}

fn object_title(object: &Value) -> Option<&str> {
    mapped_text(object, "name", "nameMap")
}

fn raw_language_code(object: &Value) -> Option<&str> {
    object
        .get("contentMap")
        .and_then(Value::as_object)
        .and_then(|map| map.keys().next())
        .or_else(|| {
            object
                .get("nameMap")
                .and_then(Value::as_object)
                .and_then(|map| map.keys().next())
        })
        .or_else(|| {
            object
                .get("summaryMap")
                .and_then(Value::as_object)
                .and_then(|map| map.keys().next())
        })
        .map(String::as_str)
        .or_else(|| {
            // Lemmy and PeerTube declare language as `{identifier, name}`
            // (schema.org `inLanguage`), never via `*Map` keys.
            object
                .get("language")
                .and_then(|language| language.get("identifier"))
                .and_then(Value::as_str)
        })
}

fn link_href(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Object(map) => map
            .get("href")
            .or_else(|| map.get("url"))
            .or_else(|| map.get("id"))
            .and_then(link_href),
        _ => None,
    }
}

fn url_to_href<'a>(value: &'a Value, preferred_type: Option<&str>) -> Option<&'a str> {
    match value {
        Value::Array(items) if items.first().is_some_and(Value::is_string) => {
            items.first().and_then(Value::as_str)
        }
        Value::Array(items) => items
            .iter()
            .find(|item| {
                let Some(preferred) = preferred_type else {
                    return true;
                };
                item.as_object().is_some_and(|map| {
                    map.get("mimeType")
                        .or_else(|| map.get("mediaType"))
                        .and_then(Value::as_str)
                        .unwrap_or("text/html")
                        == preferred
                })
            })
            .and_then(link_href),
        Value::Object(map) => {
            if let Some(preferred) = preferred_type {
                let media_type = map
                    .get("mimeType")
                    .or_else(|| map.get("mediaType"))
                    .and_then(Value::as_str)
                    .unwrap_or("text/html");
                if media_type != preferred {
                    return None;
                }
            }
            link_href(value)
        }
        Value::String(s) => Some(s),
        _ => None,
    }
}

fn url_to_media_type(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::Object(map) => map
            .get("mediaType")
            .or_else(|| map.get("mimeType"))
            .and_then(Value::as_str),
        Value::Array(items) if !items.first().is_some_and(Value::is_string) => {
            items.iter().find_map(|item| {
                item.as_object().and_then(|map| {
                    map.get("mediaType")
                        .or_else(|| map.get("mimeType"))
                        .and_then(Value::as_str)
                })
            })
        }
        _ => None,
    }
}

/// `https://` everywhere, `http://` only on hidden-service hosts — the
/// remote-reference rule shared with the outbound guard.
fn federation_url(url: &str) -> bool {
    plamenu_federation::is_federation_url(url)
}

fn object_web_url(object: &Value) -> Option<&str> {
    object
        .get("url")
        .and_then(|url| url_to_href(url, Some("text/html")))
        .filter(|url| federation_url(url))
}

fn linkable_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// The title of a remote object, hoisted from `name` on any *top-level*
/// object — `Discourse` and `WriteFreely` title plain `Note`s, so the rule is
/// type-independent; reply `name`s are never hoisted (Mitra's rule; Lemmy
/// comments and `PeerTube` chapters carry incidental names). Plain text only.
fn hoisted_title(object: &Value) -> Option<String> {
    if object.get("inReplyTo").and_then(id_of).is_some() {
        return None;
    }
    let title = plamenu_ap::text::sanitize_remote_plain(object_title(object)?);
    (!title.is_empty()).then_some(title)
}

/// Drops a leading `<h1>`/`<h2>` heading that merely repeats the hoisted
/// title: `WriteFreely` prepends `<h1>{title}</h1>` into a titled Note's
/// `content` as a Mastodon workaround, which would render the title twice.
fn strip_duplicate_title_heading<'a>(content: &'a str, title: Option<&str>) -> &'a str {
    let Some(title) = title else {
        return content;
    };
    let trimmed = content.trim_start();
    for tag in ["h1", "h2"] {
        let Some(rest) = trimmed
            .strip_prefix(&format!("<{tag}>"))
            .and_then(|rest| rest.split_once(&format!("</{tag}>")))
        else {
            continue;
        };
        let (heading, body) = rest;
        if plamenu_ap::text::sanitize_remote_plain(heading) == title {
            return body.trim_start();
        }
    }
    content
}

fn remote_status_content(object: &Value, local_domain: &str) -> String {
    // Every supported status type carries its full native body. WordPress
    // Articles are never truncated on the wire, lotide title-only Pages
    // legitimately have no content, and Hubzilla Documents carry a useful
    // file-share sentence which must not be replaced by a synthetic stub.
    let body = strip_duplicate_title_heading(
        object_content(object).unwrap_or(""),
        hoisted_title(object).as_deref(),
    );
    // PeerTube declares markdown `content` (posts and comments,
    // `mediaType: text/markdown`). Sanitizing it as HTML mangles it — every
    // `\r\n` collapses into one line — so render it properly, then linkify
    // bare URLs and #hashtags.
    if object.get("mediaType").and_then(Value::as_str) == Some("text/markdown") {
        let rendered = crate::compose::markdown_to_sanitized_html(body, true);
        return crate::compose::linkify_remote_html(&rendered, local_domain);
    }
    let raw = body.to_owned();
    // We deliberately keep Mastodon's `RE: <url>` quote fallback in the content
    // rather than stripping it at ingest. Native quote rendering is still
    // unstable across the fediverse, and a quote whose state is pending or denied
    // would otherwise lose all context. Keeping the fallback preserves the link
    // to the quoted post in those cases. Rendering removes it only when an
    // accepted quote card is actually embedded in the response.
    plamenu_ap::text::sanitize_remote_html(&raw)
}

/// The content warning, sensitive flag and language of a remote note. On
/// `Note`/`Question`, `summary` is unconditionally the content warning
/// (Mastodon semantics). On every non-Note type it is an excerpt/teaser —
/// `WordPress`/`WriteFreely`/`NodeBB` send excerpts, `Mobilizon` a generated
/// date-and-place line — **unless** the sender flagged the post
/// `sensitive`, in which case the summary is the warning text (`WordPress`
/// replaces the excerpt with the CW exactly this way). Non-CW excerpts are
/// dropped: the full body is stored, so a teaser would only duplicate it.
fn note_metadata(object: &Value) -> (String, bool, Option<String>) {
    let sensitive = object
        .get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw_summary = if stored_object_type(object).is_none() || sensitive {
        object_summary(object).unwrap_or("")
    } else {
        ""
    };
    // Clients render spoilers as plain text: strip remote markup entirely.
    let spoiler_text = plamenu_ap::text::sanitize_remote_text(raw_summary);
    let language = raw_language_code(object)
        .filter(|code| crate::actions::is_language_code(code))
        .map(str::to_owned);
    (spoiler_text, sensitive, language)
}

/// Longest accepted `category` / `join_mode`-adjacent free-text label. These
/// are display hints from an open vocabulary, so they are kept as sent — but a
/// hostile origin must not be able to park a megabyte in a sidecar column.
const MAX_EVENT_LABEL: usize = 64;
/// Longest accepted structured-address component. Generous next to any real
/// street or locality; bounded for the same reason as [`MAX_EVENT_LABEL`].
const MAX_EVENT_ADDRESS_PART: usize = 200;

/// A trimmed, sanitized, length-capped free-text field of an `Event`.
fn event_text(object: &Value, key: &str, max: usize) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(plamenu_ap::text::sanitize_remote_plain)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty() && text.len() <= max)
}

/// A non-negative `i32` count from an `Event`. A negative or absurd value is
/// dropped rather than clamped: "the origin said something impossible" is not
/// the same fact as "the origin said zero", and a bogus zero capacity would
/// render a joinable event as full.
fn event_count(object: &Value, key: &str) -> Option<i32> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .filter(|n| (0..=i64::from(i32::MAX)).contains(n))
        .and_then(|n| i32::try_from(n).ok())
}

/// The structured `Place` of an `Event`: its own id plus the nested
/// `PostalAddress`. `Mobilizon` sends
/// `location: {id, name, address: {streetAddress, addressLocality, …}}`;
/// leaner dialects send a bare `{name}`, or a plain string, or nothing.
fn event_place(object: &Value) -> (Option<String>, Option<String>, [Option<String>; 5]) {
    let Some(location) = object.get("location") else {
        return (None, None, Default::default());
    };
    // A bare string location is a name, not an address.
    if let Some(name) = location.as_str() {
        let name = plamenu_ap::text::sanitize_remote_plain(name);
        let name = (!name.is_empty()).then_some(name);
        return (name, None, Default::default());
    }
    let name = event_text(location, "name", MAX_EVENT_ADDRESS_PART);
    // The Place's own id — the only stable handle on a venue shared by several
    // events. Only an absolute federation URL is worth keeping.
    let url = location
        .get("id")
        .or_else(|| location.get("url"))
        .and_then(Value::as_str)
        .filter(|url| federation_url(url) && url.len() <= 2048)
        .map(str::to_owned);
    // The address may be nested under `address` (Mobilizon/schema.org) or
    // flattened onto the Place itself, so both are tried per component.
    let part = |key: &str| {
        location
            .get("address")
            .and_then(|address| event_text(address, key, MAX_EVENT_ADDRESS_PART))
            .or_else(|| event_text(location, key, MAX_EVENT_ADDRESS_PART))
    };
    (
        name,
        url,
        [
            part("streetAddress"),
            part("addressLocality"),
            part("addressRegion"),
            part("addressCountry"),
            part("postalCode"),
        ],
    )
}

/// Whether an inbound `Event` is an unpublished draft, which must never be
/// stored. `Mobilizon` marks one `draft: true` and does not federate it; a
/// forwarded, relayed or laxer-dialect copy could still reach us, and
/// publishing an organizer's unfinished draft to our own followers would be a
/// disclosure we cannot take back.
#[must_use]
pub fn is_draft_event(object: &Value) -> bool {
    stored_object_type(object) == Some("Event")
        && object.get("draft").and_then(Value::as_bool) == Some(true)
}

/// Persists the typed sidecar of an inbound `Event` object (`Mobilizon`,
/// Gancio, …): when, where, how to attend, and whether it still happens.
/// Every field is optional — foreign dialects send as little as a bare
/// `{name}` location — and values are kept as sent (`Mobilizon`'s auto-filled
/// `23:59:59` end sentinel included; rendering decides what to show).
///
/// A missing field means "the origin didn't say", never a default: that is why
/// the booleans stay `Option` all the way to the client. `join_mode` in
/// particular gates the RSVP affordance, and guessing `free` for a dialect
/// that never mentions join modes would offer a button that generates a `Join`
/// nobody will answer.
async fn store_event_sidecar(
    state: &AppState,
    stored: &status::Status,
    object: &Value,
) -> Result<bool, ApiError> {
    if stored_object_type(object) != Some("Event") {
        return Ok(false);
    }
    // Read the previous row before overwriting it: a moved start time and a flip
    // to CANCELLED are the two changes an attendee has to re-plan around, and
    // this is the only place either is visible — an `Update(Event)` may touch
    // neither the content nor any other compared field, so the edit diff further
    // down will not see it at all.
    let previous = plamenu_db::status_event::find(&state.pool, stored.id).await?;
    let time_field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    };
    let bool_field = |key: &str| object.get(key).and_then(Value::as_bool);
    let timezone = object
        .get("timezone")
        .and_then(Value::as_str)
        .filter(|tz| !tz.is_empty() && tz.len() <= 64)
        .map(str::to_owned);
    // Mobilizon duplicates the ical status under `status` and `ical:status`;
    // only the three ical values are meaningful.
    let event_status = object
        .get("status")
        .or_else(|| object.get("ical:status"))
        .and_then(Value::as_str)
        .map(str::to_uppercase)
        .filter(|s| matches!(s.as_str(), "CONFIRMED" | "TENTATIVE" | "CANCELLED"));
    // The four values the CHECK constraint accepts; anything else is a dialect
    // we don't model, and is dropped rather than guessed at.
    let join_mode = object
        .get("joinMode")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .filter(|mode| matches!(mode.as_str(), "free" | "restricted" | "invite" | "external"));
    let external_participation_url = object
        .get("externalParticipationUrl")
        .and_then(Value::as_str)
        .filter(|url| federation_url(url) && url.len() <= 2048)
        .map(str::to_owned);
    let (location_name, location_url, [street, locality, region, country, postal_code]) =
        event_place(object);
    let next = plamenu_db::status_event::StatusEvent {
        status_id: stored.id,
        start_time: time_field("startTime"),
        end_time: time_field("endTime"),
        location_name,
        timezone,
        event_status,
        join_mode,
        participant_count: event_count(object, "participantCount"),
        max_attendees: event_count(object, "maximumAttendeeCapacity"),
        remaining_attendees: event_count(object, "remainingAttendeeCapacity"),
        external_participation_url,
        anonymous_participation: bool_field("anonymousParticipationEnabled"),
        is_online: bool_field("isOnline"),
        comments_enabled: bool_field("commentsEnabled"),
        category: event_text(object, "category", MAX_EVENT_LABEL),
        location_url,
        location_street: street,
        location_locality: locality,
        location_region: region,
        location_country: country,
        location_postal_code: postal_code,
    };
    plamenu_db::status_event::upsert(&state.pool, &next).await?;
    // `StatusEvent::disrupts` owns this rule; the local edit path asks it too.
    Ok(previous.is_some_and(|before| before.disrupts(&next)))
}

/// A native-media object's primary file, mined from the `url` Link tree:
/// `PeerTube` ships a `Video`'s files as `url` Links (directly, or nested in
/// an HLS playlist Link's `tag`) and its poster in `icon` — there is no
/// `attachment` at all. Applies equally to `Audio`/`Image` objects
/// (Funkwhale-style `url` Links) and Hubzilla `Document` file Links.
struct ObjectMedia<'a> {
    url: &'a str,
    media_type: &'a str,
    width: Option<i32>,
    height: Option<i32>,
    /// The largest `icon` image — the poster/thumbnail.
    poster: Option<&'a str>,
    /// `PeerTube` separated-audio HLS: the audio-only companion stream (the
    /// `height: 0` file Link) that must be muxed in at cache time — the
    /// video renditions alone are silent.
    audio_url: Option<&'a str>,
    /// Large A/V downloads on first play instead of eagerly at ingest, so a
    /// federated-but-never-watched video costs the origin nothing.
    defer_download: bool,
    /// HLS-native video (`PeerTube`): the origin master playlist URL. `None` for
    /// plain-mp4 media. Its presence is what routes the video down the caching
    /// HLS proxy instead of the whole-file progressive lane.
    hls_master_url: Option<&'a str>,
    /// The full HLS rendition ladder (incl. the audio-only track); empty for
    /// non-HLS media.
    renditions: Vec<media::NewRendition<'a>>,
    /// Set when the object is a `PeerTube` live broadcast rather than a file.
    live: Option<LiveInfo>,
}

/// Where a `PeerTube` live broadcast is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveState {
    /// Announced, not started (or, for a permanent live, between broadcasts).
    Waiting,
    /// On air right now.
    Live,
    /// Finished, with no replay.
    Ended,
}

impl LiveState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Live => "live",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveInfo {
    state: LiveState,
    /// A permanent live re-arms for its next broadcast instead of ending.
    permanent: bool,
}

/// `PeerTube`'s `VideoState` enum (`packages/models/src/videos/`).
const PT_STATE_PUBLISHED: i64 = 1;
const PT_STATE_WAITING_FOR_LIVE: i64 = 4;
const PT_STATE_LIVE_ENDED: i64 = 5;

/// Reads a `Video` object's live-broadcast facts, or `None` when it is not a
/// live at all.
///
/// Note this says nothing about whether the object currently *has* playable
/// media: a live is announced with no `url` ladder and no master playlist, and
/// a finished one keeps advertising the master it is about to delete. The
/// caller decides what to do with each state; here we only read them.
fn live_info(object: &Value) -> Option<LiveInfo> {
    if object.get("isLiveBroadcast").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let state = match object.get("state").and_then(Value::as_i64) {
        Some(PT_STATE_PUBLISHED) => LiveState::Live,
        Some(PT_STATE_LIVE_ENDED) => LiveState::Ended,
        Some(PT_STATE_WAITING_FOR_LIVE) => LiveState::Waiting,
        // An unknown or absent state: treat a published master as "on air"
        // and anything else as not started, so a peer that omits `state`
        // still plays rather than rendering a dead placeholder.
        _ => {
            if hls_master_url(object).is_some() {
                LiveState::Live
            } else {
                LiveState::Waiting
            }
        }
    };
    Some(LiveInfo {
        state,
        permanent: object.get("permanentLive").and_then(Value::as_bool) == Some(true),
    })
}

/// The origin HLS master playlist URL of a `Video` object: the top-level `url`
/// Link whose `mediaType` is `application/x-mpegURL` (`PeerTube` nests every
/// rendition + the audio track inside this Link's `tag`). `None` when the
/// object ships only plain progressive files.
fn hls_master_url(object: &Value) -> Option<&str> {
    one_or_many(object.get("url")).iter().find_map(|entry| {
        let map = entry.as_object()?;
        let media_type = map
            .get("mediaType")
            .or_else(|| map.get("mimeType"))
            .and_then(Value::as_str)?;
        if !matches!(
            media_type,
            "application/x-mpegURL" | "application/vnd.apple.mpegurl"
        ) {
            return None;
        }
        let url = map
            .get("href")
            .or_else(|| map.get("url"))
            .and_then(Value::as_str)?;
        federation_url(url).then_some(url)
    })
}

/// The eager in-memory download cap ([`plamenu_federation`]'s
/// `MEDIA_BODY_LIMIT`, Mastodon's `VIDEO_LIMIT`): A/V whose declared size
/// exceeds it can only be cached by the streaming on-demand lane.
const EAGER_MEDIA_LIMIT: i64 = 99 * 1024 * 1024;

/// The caps steering rendition selection for a native-media object — from
/// the instance settings (`remote_video_max_height` / `remote_video_max_mb`).
/// Edit-diffing ([`attachment_pairs`]) must use the same values as ingest or
/// every Update looks like an attachment change.
#[derive(Debug, Clone, Copy)]
struct MediaCaps {
    max_height: i64,
    budget_bytes: i64,
}

impl MediaCaps {
    async fn load(state: &AppState) -> Result<Self, ApiError> {
        let settings = state.settings_cache.get(&state.pool).await?;
        Ok(Self {
            max_height: i64::from(settings.remote_video_max_height),
            budget_bytes: i64::from(settings.remote_video_max_mb).saturating_mul(1024 * 1024),
        })
    }
}

/// One playable-file candidate from the `url` tree.
struct MediaLink<'a> {
    url: &'a str,
    media_type: &'a str,
    width: Option<i64>,
    height: Option<i64>,
    size: i64,
    /// Frames per second, when the origin declares it (`PeerTube` rendition
    /// Links carry `fps`); kept for the HLS rendition ladder.
    fps: Option<i64>,
    /// `PeerTube` 7.3+ publishes explicit ffprobe stream-type hints. `None`
    /// means an older peer omitted the hints; it does not mean false.
    has_audio: Option<bool>,
    has_video: Option<bool>,
    /// Nested below the HLS master rather than a top-level `WebVideo` link.
    in_hls: bool,
    /// Explicit `peertube_format_flag=web-video`, used to prefer a directly
    /// playable muxed source over a same-height split HLS rendition.
    web_video: bool,
}

fn peertube_link_hints(entry: &Value) -> (Option<bool>, Option<bool>, bool) {
    let values: Vec<(&str, &str)> = one_or_many(entry.get("attachment"))
        .iter()
        .filter_map(|value| {
            let name = value.get("name").and_then(Value::as_str)?;
            let value = value.get("value").and_then(Value::as_str)?;
            Some((name, value))
        })
        .collect();
    let stream_hints: Vec<&str> = values
        .iter()
        .filter_map(|(name, value)| (*name == "ffprobe_codec_type").then_some(*value))
        .collect();
    let has_audio = (!stream_hints.is_empty()).then(|| stream_hints.contains(&"audio"));
    let has_video = (!stream_hints.is_empty()).then(|| stream_hints.contains(&"video"));
    let web_video = values
        .iter()
        .any(|(name, value)| *name == "peertube_format_flag" && *value == "web-video");
    (has_audio, has_video, web_video)
}

/// Reads one `url` entry (bare string, or a `Link`/`Image` object) as a
/// media-file candidate of the wanted kind. `None` accepts any non-HTML file
/// representation, for a top-level `Document`.
fn media_link<'a>(entry: &'a Value, wanted: Option<&str>, in_hls: bool) -> Option<MediaLink<'a>> {
    let wanted_type = |media_type: &str| {
        wanted.map_or_else(
            || {
                !matches!(
                    media_type.split(';').next().unwrap_or("").trim(),
                    "text/html" | "application/activity+json" | "application/ld+json"
                )
            },
            |prefix| media_type.starts_with(prefix),
        )
    };
    match entry {
        Value::String(url) => {
            let media_type = content_type_from_url(url)?;
            wanted_type(media_type).then_some(MediaLink {
                url,
                media_type,
                width: None,
                height: None,
                size: 0,
                fps: None,
                has_audio: None,
                has_video: None,
                in_hls,
                web_video: false,
            })
        }
        Value::Object(map) => {
            let media_type = map
                .get("mediaType")
                .or_else(|| map.get("mimeType"))
                .and_then(Value::as_str)?;
            if !wanted_type(media_type) {
                return None;
            }
            let url = map
                .get("href")
                .or_else(|| map.get("url"))
                .and_then(Value::as_str)?;
            let (has_audio, has_video, web_video) = peertube_link_hints(entry);
            Some(MediaLink {
                url,
                media_type,
                width: map.get("width").and_then(Value::as_i64),
                height: map.get("height").and_then(Value::as_i64),
                size: map
                    .get("size")
                    .or_else(|| map.get("contentSize"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                fps: map.get("fps").and_then(Value::as_i64),
                has_audio,
                has_video,
                in_hls,
                web_video,
            })
        }
        _ => None,
    }
}

fn object_primary_media(object: &Value, caps: MediaCaps) -> Option<ObjectMedia<'_>> {
    let object_type = stored_object_type(object)?;
    let wanted = match object_type {
        "Video" => Some("video/"),
        "Audio" => Some("audio/"),
        "Image" => Some("image/"),
        "Document" => None,
        _ => return None,
    };
    let mut candidates: Vec<MediaLink<'_>> = Vec::new();
    for entry in one_or_many(object.get("url")) {
        candidates.extend(media_link(entry, wanted, false));
        // PeerTube nests the actual mp4 file Links inside the HLS playlist
        // Link's own `tag` array.
        for nested in one_or_many(entry.get("tag")) {
            candidates.extend(media_link(nested, wanted, true));
        }
    }
    candidates.retain(|link| federation_url(link.url));

    // The HLS master playlist (PeerTube): its presence routes the video down
    // the caching HLS proxy + quality selector instead of the whole-file lane.
    let hls_master = (wanted == Some("video/"))
        .then(|| hls_master_url(object))
        .flatten();

    // A live broadcast declares no file Links at all — its rendition ladder
    // exists only inside the playlist, which does not exist yet when the live
    // is merely announced. Mining `url` for a file therefore finds nothing, so
    // the attachment is synthesized from the broadcast itself. A live that has
    // been *replayed* does publish a ladder, and falls through to the ordinary
    // VOD path below: at that point it is simply a video.
    if candidates.is_empty()
        && wanted == Some("video/")
        && let Some(live) = live_info(object)
    {
        return live_media(object, live, hls_master);
    }

    // PeerTube separated-audio HLS: `height: 0` file Links are the
    // audio-only stream, not renditions — split them out and remember the
    // biggest as the companion to mux in at cache time.
    let (audio_streams, mut renditions): (Vec<MediaLink<'_>>, Vec<MediaLink<'_>>) = if wanted
        == Some("video/")
    {
        candidates.into_iter().partition(|link| {
            link.has_audio == Some(true) && link.has_video == Some(false) || link.height == Some(0)
        })
    } else {
        (Vec::new(), candidates)
    };
    let audio = audio_streams.into_iter().max_by_key(|link| link.size);
    let audio = if renditions.is_empty() {
        // An audio-only publication (PeerTube "audio" videos): the sole
        // stream is the media itself, not a companion.
        renditions = audio.into_iter().collect();
        None
    } else {
        audio
    };
    pick_primary(
        object,
        wanted.unwrap_or("document/"),
        &renditions,
        audio,
        caps,
        hls_master,
    )
}

/// Ranks a video rendition for the single progressive `url` handed to
/// quality-blind Mastodon-API clients (lower = preferred). The ladder the user
/// asked for so every dumb client gets the SAME universally-light file: exactly
/// 480p first, then the tallest rung *below* 480p, then the shortest rung
/// *above* it. An undeclared height (a plain progressive `url` with no `height`)
/// sorts as an unknown-but-assumed-light rung at the bottom of the ≤480 tier;
/// same-height/same-size ties fall to the origin's url-tree order. The full
/// ladder is still persisted for the HLS quality selector — this only picks the
/// one fallback rendition.
fn progressive_rank(link: &MediaLink<'_>) -> (u8, i64, u8, u8, i64) {
    let (tier, distance) = match link.height.unwrap_or(0) {
        h @ 1..=480 => (0, 480 - h),
        h if h > 480 => (1, h),
        _ => (0, 480),
    };
    // At the same useful height prefer a source that is explicitly muxed,
    // then a WebVideo over a fragmented file. Unknown old-peer hints remain
    // behind known-muxed but ahead of known-silent.
    let audio_rank = match link.has_audio {
        Some(true) => 0,
        None => 1,
        Some(false) => 2,
    };
    let format_rank = u8::from(!link.web_video);
    (tier, distance, audio_rank, format_rank, link.size)
}

/// Chooses the rendition to store: for video, the compatibility-ladder pick
/// ([`progressive_rank`]); for audio/image, the best within the operator's
/// height/size caps. When nothing fits, the lightest offered, so the post still
/// renders and the origin link still works.
fn pick_primary<'a>(
    object: &'a Value,
    wanted: &str,
    renditions: &[MediaLink<'a>],
    audio: Option<MediaLink<'a>>,
    caps: MediaCaps,
    hls_master: Option<&'a str>,
) -> Option<ObjectMedia<'a>> {
    let fits = |link: &MediaLink<'_>| {
        let needs_companion = link.in_hls && link.has_audio != Some(true);
        let audio_size = if needs_companion {
            audio.as_ref().map_or(0, |audio| audio.size)
        } else {
            0
        };
        (caps.max_height <= 0 || link.height.unwrap_or(0) <= caps.max_height)
            && (caps.budget_bytes <= 0
                || link.size == 0
                || link.size.saturating_add(audio_size) <= caps.budget_bytes)
    };
    // Video renditions get the compatibility ladder ([`progressive_rank`]):
    // every quality-blind Mastodon-API client is handed the SAME universally
    // light single rendition (prefer 480p). Audio/image have no such ladder —
    // keep the largest that fits. When nothing fits the caps, fall back to the
    // lightest offered so the post still renders and the origin link works.
    let best = if wanted == "video/" {
        renditions
            .iter()
            .filter(|link| fits(link))
            .min_by_key(|link| progressive_rank(link))
            .or_else(|| renditions.iter().min_by_key(|link| progressive_rank(link)))
    } else {
        renditions
            .iter()
            .filter(|link| fits(link))
            .max_by_key(|link| (link.height.unwrap_or(0), link.size))
            .or_else(|| {
                renditions
                    .iter()
                    .min_by_key(|link| (link.height.unwrap_or(0), link.size))
            })
    }?;
    // The separated audio stream belongs to the nested HLS ladder. A
    // top-level WebVideo is independently playable even when an older peer
    // omitted the ffprobe codec hints.
    let needs_companion = audio.is_some() && best.in_hls && best.has_audio != Some(true);
    let is_av = matches!(wanted, "audio/" | "video/");
    // HLS video, large A/V, or separated audio never downloads eagerly: HLS
    // plays through the caching proxy, the rest is fetched on first play by
    // the streaming lane. A deliberately-downscaled multi-rendition video
    // (`renditions.len() > 1` — we picked the light rung, not the tallest) must
    // defer too: the chosen rung can fall under `EAGER_MEDIA_LIMIT` and would
    // otherwise eager-fetch a never-watched video at ingest, breaking the
    // "a federated-but-never-watched video costs the origin nothing" rule.
    // Everything else keeps the eager in-memory pipeline.
    let defer_download = is_av
        && (hls_master.is_some()
            || audio.is_some()
            || best.size > EAGER_MEDIA_LIMIT
            || renditions.len() > 1);
    // The full rendition ladder, retained only for HLS videos (the quality
    // selector + per-rendition segment cache). `remote_url` = `best` remains
    // the single-rendition progressive-mp4 fallback for dumb clients.
    let ladder = if hls_master.is_some() {
        let mut ladder: Vec<media::NewRendition<'a>> = renditions
            .iter()
            .filter(|link| link.in_hls)
            .map(|link| media::NewRendition {
                height: link.height.and_then(|h| i32::try_from(h).ok()).unwrap_or(0),
                width: link.width.and_then(|w| i32::try_from(w).ok()),
                frame_rate: link.fps.and_then(|f| i32::try_from(f).ok()),
                size_bytes: (link.size > 0).then_some(link.size),
                origin_url: link.url,
                is_audio: false,
            })
            .collect();
        if let Some(audio_link) = audio.as_ref() {
            ladder.push(media::NewRendition {
                height: 0,
                width: None,
                frame_rate: None,
                size_bytes: (audio_link.size > 0).then_some(audio_link.size),
                origin_url: audio_link.url,
                is_audio: true,
            });
        }
        ladder
    } else {
        Vec::new()
    };
    let poster = object_poster(object);
    Some(ObjectMedia {
        url: best.url,
        media_type: best.media_type,
        width: best.width.and_then(|w| i32::try_from(w).ok()),
        height: best.height.and_then(|h| i32::try_from(h).ok()),
        poster,
        // `audio` was only borrowed to build the ladder above; move it now.
        audio_url: needs_companion
            .then(|| audio.map(|link| link.url))
            .flatten(),
        defer_download,
        hls_master_url: hls_master,
        renditions: ladder,
        live: None,
    })
}

/// The largest `icon` image — a native-media object's poster/thumbnail.
///
/// Exposed within the server so the on-demand poster lane can recover when a
/// remote producer rotates the icon URL after the object was first ingested.
pub(crate) fn object_poster(object: &Value) -> Option<&str> {
    object_posters(object).into_iter().next()
}

/// Every valid native-media poster, largest first. The proxy uses the rest of
/// this list when a producer keeps advertising a broken primary preview beside
/// a working thumbnail (a shape emitted by `PeerTube` during some transcodes).
pub(crate) fn object_posters(object: &Value) -> Vec<&str> {
    let mut posters: Vec<_> = one_or_many(object.get("icon"))
        .iter()
        .filter_map(|icon| {
            let url = icon
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| icon.as_str())?;
            federation_url(url)
                .then(|| (url, icon.get("width").and_then(Value::as_i64).unwrap_or(0)))
        })
        .collect();
    posters.sort_by_key(|(_, width)| std::cmp::Reverse(*width));
    posters.dedup_by_key(|(url, _)| *url);
    posters.into_iter().map(|(url, _)| url).collect()
}

/// The attachment for a `PeerTube` live broadcast, which publishes no file to
/// point at.
///
/// `remote_url` is the identity a re-ingest deduplicates on and an edit diffs
/// against, so it must be present and stable across the whole lifecycle —
/// including before the first broadcast, when there is no playlist at all.
/// The master playlist URL is neither (it is absent while waiting), so the
/// video's own AP id is used: stable from announcement to replay, and already
/// the canonical address of the thing being watched. The playable addresses
/// live elsewhere — `hls_master_url` for the proxy, and the live gateway URL
/// the serializer derives from the attachment id.
fn live_media<'a>(
    object: &'a Value,
    live: LiveInfo,
    hls_master: Option<&'a str>,
) -> Option<ObjectMedia<'a>> {
    let id = object.get("id").and_then(Value::as_str)?;
    if !federation_url(id) {
        return None;
    }
    Some(ObjectMedia {
        url: id,
        // The only media type a live ever declares. `kind_or_derived` maps it
        // to `video`, which is what every client needs to see.
        media_type: "application/x-mpegURL",
        width: None,
        height: None,
        poster: object_poster(object),
        audio_url: None,
        // A live is never downloaded: it has no file, and its segments are
        // deleted from the origin as the window slides.
        defer_download: true,
        hls_master_url: hls_master,
        renditions: Vec::new(),
        live: Some(live),
    })
}

/// An `xsd:duration` (`PT2H49M45S`, `PT10185S`, …) as seconds — `PeerTube`
/// declares the video length on the object; storing it means clients show
/// the real duration before the file is ever downloaded and probed.
fn parse_iso8601_seconds(raw: &str) -> Option<f64> {
    let rest = raw.strip_prefix('P')?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((date, time)) => (date, time),
        None => (rest, ""),
    };
    let mut total = 0.0_f64;
    let mut scan = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
            } else {
                let unit = units.iter().find(|(u, _)| *u == ch)?;
                total += number.parse::<f64>().ok()? * unit.1;
                number.clear();
            }
        }
        number.is_empty().then_some(())
    };
    scan(
        date_part,
        &[('Y', 31_536_000.0), ('W', 604_800.0), ('D', 86_400.0)],
    )?;
    scan(time_part, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)])?;
    (total > 0.0).then_some(total)
}
fn parse_quote_policy(
    object: &Value,
    author: &Account,
    followers_url: &str,
    following_url: &str,
) -> i32 {
    let Some(interaction_policy) = object.get("interactionPolicy") else {
        return 0;
    };
    plamenu_ap::quote_policy::parse_interaction_policy(
        interaction_policy,
        author.uri.as_deref().unwrap_or(""),
        followers_url,
        following_url,
    )
}

/// Stores a remote `Note` authored by `author` (the caller has verified the
/// attribution): sanitized content, addressing-derived visibility, reply
/// threading (with ancestor backfill), attachments, tags/mentions and quote
/// linkage.
pub async fn ingest_remote_note(
    state: &AppState,
    author: &Account,
    object: &Value,
) -> Result<status::Status, ApiError> {
    Ok(ingest_remote_note_delivery(state, author, object)
        .await?
        .status)
}

pub async fn ingest_remote_note_delivery(
    state: &AppState,
    author: &Account,
    object: &Value,
) -> Result<RemoteIngestResult, ApiError> {
    let result = ingest_remote_note_delivery_deferred(state, author, object).await?;
    if result.delivery_effects {
        create_deferred_post_notifications(state, &result).await?;
    }
    Ok(result)
}

/// Delivery ingest with mention/quote notifications returned to the caller.
/// The inbox uses this form so it can add notify-on-post recipients before one
/// canonical notification write; other callers use [`ingest_remote_note_delivery`]
/// and retain its established notification side effects.
pub(crate) async fn ingest_remote_note_delivery_deferred(
    state: &AppState,
    author: &Account,
    object: &Value,
) -> Result<RemoteIngestResult, ApiError> {
    ingest_remote_note_with_quote_depth(state, author, object, 0, RemoteIngestContext::Delivery)
        .await
}

async fn create_deferred_post_notifications(
    state: &AppState,
    result: &RemoteIngestResult,
) -> Result<(), ApiError> {
    let mut candidates: Vec<notification::PostNotification> = result
        .mention_notify
        .iter()
        .copied()
        .map(notification::PostNotification::mention)
        .collect();
    if let Some(account_id) = result.quote_notify {
        candidates.push(notification::PostNotification {
            account_id,
            quote: true,
            ..notification::PostNotification::default()
        });
    }
    notification::create_post_notifications_many(
        &state.pool,
        &candidates,
        result.status.account_id,
        result.status.id,
    )
    .await?;
    Ok(())
}

pub async fn ingest_remote_note_in_context(
    state: &AppState,
    author: &Account,
    object: &Value,
    context: RemoteIngestContext,
) -> Result<status::Status, ApiError> {
    Ok(
        ingest_remote_note_with_quote_depth(state, author, object, 0, context)
            .await?
            .status,
    )
}

async fn ingest_remote_note_with_quote_depth(
    state: &AppState,
    author: &Account,
    object: &Value,
    quote_depth: usize,
    context: RemoteIngestContext,
) -> Result<RemoteIngestResult, ApiError> {
    let in_reply_to_id = match object.get("inReplyTo").and_then(id_of) {
        Some(parent_uri) if context.is_history() => resolve_status_ref(state, parent_uri)
            .await?
            .map(|parent| parent.id),
        Some(parent_uri) => resolve_thread_parent(state, parent_uri, quote_depth, context)
            .await?
            .map(|parent| parent.id),
        None => None,
    };
    Box::pin(store_remote_note(
        state,
        author,
        object,
        in_reply_to_id,
        quote_depth,
        context,
    ))
    .await
}

/// The storage half of [`ingest_remote_note`], with the reply parent already
/// resolved — backfilled ancestors come through here directly so they cannot
/// recurse into another backfill.
/// Applies the scope-widening clamp to replies that have just been joined to
/// `parent`, and to everything below them.
///
/// The clamp normally runs at ingest: a reply may not be stored as more public
/// than the conversation it threads into, so a forged wider `to`/`cc` cannot leak
/// a closed thread. But it can only run when the parent resolved — an orphaned
/// reply had no conversation to be measured against and kept whatever audience it
/// claimed. Delivery order is the attacker's to choose (a reply to a post we are
/// not allowed to fetch is *always* orphaned), so the check has to be re-run at
/// the one moment the conversation becomes known.
///
/// Narrowing only, and the ceiling comes from the same place as the ingest-time
/// clamp: the conversation root. A parent that is itself an unresolved reply has
/// no recorded root, and then the post being answered is the ceiling — which is
/// what the ingest-time clamp would have used had this parent been the root.
/// `clamp_visibility` returns either the row's own value or the ceiling, so every
/// row that changes changes to the same value and one UPDATE covers them all.
async fn clamp_adopted_audience(
    conn: &mut plamenu_db::PgConnection,
    parent: &status::Status,
    adopted: &[i64],
) -> Result<(), ApiError> {
    let ceiling = plamenu_db::conversation::context_of_status(&mut *conn, parent.id)
        .await?
        .and_then(|ctx| ctx.root_visibility)
        .unwrap_or_else(|| parent.visibility.clone());
    let mut narrowed = Vec::new();
    for (id, visibility) in status::subtree_visibilities_many(&mut *conn, adopted).await? {
        if plamenu_ap::activity::clamp_visibility(&visibility, &ceiling) != visibility.as_str() {
            narrowed.push(id);
        }
    }
    if !narrowed.is_empty() {
        tracing::info!(
            parent = %parent.id,
            narrowed = narrowed.len(),
            visibility = %ceiling,
            "clamped adopted replies to the conversation's audience"
        );
        status::narrow_visibility(&mut *conn, &narrowed, &ceiling).await?;
    }
    Ok(())
}

/// The FEP conversation IRIs an inbound object declares: `context` (FEP-7888,
/// or the legacy Mastodon `conversation`) → the collection of posts;
/// `contextHistory` (FEP-171b) → the collection of activities. IRIs on our own
/// domain are dropped — they name a conversation we own, into which the reply
/// threads through its parent, not by minting a phantom remote conversation.
fn context_refs_of(object: &Value, domain: &str) -> (Option<String>, Option<String>) {
    let local_prefix = format!("https://{domain}/");
    let foreign = |value: Option<&Value>| -> Option<String> {
        let uri = value.and_then(id_of)?;
        (federation_url(uri) && !uri.starts_with(&local_prefix)).then(|| uri.to_owned())
    };
    let context = foreign(object.get("context")).or_else(|| foreign(object.get("conversation")));
    let history = foreign(object.get("contextHistory"));
    (context, history)
}

#[allow(
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "linear ingest pass: the note plus its sidecar rows"
)]
async fn store_remote_note(
    state: &AppState,
    author: &Account,
    object: &Value,
    in_reply_to_id: Option<i64>,
    quote_depth: usize,
    context: RemoteIngestContext,
) -> Result<RemoteIngestResult, ApiError> {
    let object_uri = id_of(object).ok_or_else(|| ApiError::BadRequest("note has no id".into()))?;
    // Remote HTML is attacker-controlled: sanitize before it is stored. The
    // quote `RE:` fallback is dropped first — Plamenu renders quotes natively,
    // and sanitizing would strip its marker `class` and leave a stray `RE:`
    // paragraph alongside the quote card.
    let content = remote_status_content(object, &state.config.domain);
    let published = object
        .get("published")
        .and_then(Value::as_str)
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let collection_urls = account::collection_urls_of(&state.pool, author.id)
        .await?
        .unwrap_or(account::CollectionUrls {
            followers_url: String::new(),
            following_url: String::new(),
            outbox_url: String::new(),
        });
    let mut visibility = visibility_from_addressing(
        object.get("to").unwrap_or(&Value::Null),
        object.get("cc").unwrap_or(&Value::Null),
        &collection_urls.followers_url,
    );
    // Scope-widening guard: a reply into a conversation whose root is
    // private/direct must not be ingested as more public than the conversation.
    // A forged wider `to`/`cc` would otherwise leak the closed thread into
    // shared feeds — audience inheritance is copied down, never up (FEP-171b).
    if let Some(parent_id) = in_reply_to_id
        && let Some(ctx) =
            plamenu_db::conversation::context_of_status(&state.pool, parent_id).await?
        && let Some(root_visibility) = ctx.root_visibility
    {
        visibility = plamenu_ap::activity::clamp_visibility(visibility, &root_visibility);
    }
    if context.is_history() && !matches!(visibility, "public" | "unlisted") {
        return Err(ApiError::BadRequest(
            "cold history accepts only public or unlisted objects".into(),
        ));
    }
    let (spoiler_text, declared_sensitive, language) = note_metadata(object);
    let sensitive = author.sensitized() || declared_sensitive;
    // The Note's human web URL (`url`), distinct from its AP id. Kept only when
    // it is a real https link; the serializers fall back to the AP id otherwise.
    let web_url = object_web_url(object);
    let title = hoisted_title(object);
    let new_status = NewRemoteStatus {
        title: title.as_deref(),
        object_type: stored_object_type(object),
        external_url: attachment_link_target(object),
        uri: object_uri,
        account_id: author.id,
        content: &content,
        created_at: published,
        visibility,
        in_reply_to_id,
        // Kept for every reply, resolved parent or not: a reply whose
        // parent could not be fetched (a group announces its comments; the
        // origin may be a 403 or serve HTML to an AP request) must not be
        // mistaken for a root post and boosted into timelines.
        in_reply_to_uri: object.get("inReplyTo").and_then(id_of),
        spoiler_text: &spoiler_text,
        sensitive,
        language: language.as_deref(),
        url: web_url,
        quote_approval_policy: parse_quote_policy(
            object,
            author,
            &collection_urls.followers_url,
            &collection_urls.following_url,
        ),
    };
    let (stored, delivery_effects, promote_media) = if context == RemoteIngestContext::Delivery {
        if let Some(stored) =
            status::insert_remote_delivery_claimed(&state.pool, new_status).await?
        {
            (stored, true, false)
        } else {
            let stored = status::upsert_remote_with_provenance(
                &state.pool,
                new_status,
                context.provenance(),
            )
            .await?;
            let previous =
                status::claim_delivery_side_effects_with_previous(&state.pool, stored.id).await?;
            let promote_media = previous.as_deref() == Some("history");
            if promote_media {
                crate::remote_history::record_promotion();
            }
            (stored, previous.is_some(), promote_media)
        }
    } else {
        (
            status::upsert_remote_with_provenance(&state.pool, new_status, context.provenance())
                .await?,
            false,
            false,
        )
    };
    store_remote_attachments(state, &stored, object, context.is_history()).await?;
    if let Some(invitation) = crate::webxdc::remote_invitation(object) {
        plamenu_db::webxdc::set_remote_invitation(&state.pool, stored.id, Some(invitation)).await?;
    }
    if promote_media {
        media::promote_for_status(&state.pool, stored.id).await?;
    }
    store_event_sidecar(state, &stored, object).await?;
    // Assign the conversation before mentions notify, so a reply into a muted
    // thread is suppressed. The object's FEP context IRIs converge the thread
    // and stamp a remote root's owner/identity.
    let (context_uri, history_uri) = context_refs_of(object, &state.config.domain);
    // A reply whose parent we could not resolve still has an `inReplyTo`, so it
    // must not be mistaken for a conversation root.
    let is_reply = object.get("inReplyTo").and_then(id_of).is_some();
    {
        // Inbound ingest is not (yet) one transaction; the connection-scoped
        // conversation helpers run over a briefly-held pooled
        // connection, each statement autocommitting as before.
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(plamenu_db::DbError::from)?;
        crate::conversations::ensure_conversation(
            &mut conn,
            &stored,
            is_reply,
            plamenu_db::conversation::ContextRefs {
                context_uri: context_uri.as_deref(),
                history_uri: history_uri.as_deref(),
            },
        )
        .await?;
        // Replies that arrived before this post — its parent never resolved for
        // them — can finally be joined to it. Runs *after* the conversation
        // exists, because absorbing the placeholder conversation those replies
        // minted needs this post's own. A redelivery finds nothing left to adopt.
        if let Some(uri) = stored.uri.as_deref() {
            let adopted = status::adopt_orphan_replies(&mut conn, stored.id, uri).await?;
            if !adopted.is_empty() {
                tracing::info!(
                    parent = %stored.id,
                    adopted = adopted.len(),
                    "joined orphaned replies to their parent"
                );
                clamp_adopted_audience(&mut conn, &stored, &adopted).await?;
            }
        }
    }
    Box::pin(store_remote_emojis(
        state,
        author,
        object,
        delivery_effects && matches!(stored.visibility.as_str(), "public" | "unlisted" | "local"),
    ))
    .await?;
    store_remote_tags(state, author, &stored, object, !context.is_history()).await?;
    let mention_notify = if carries_mentions(object, &state.config.domain) {
        Box::pin(reconcile_note_mentions(
            state, &stored, object, false, false,
        ))
        .await?
    } else {
        Vec::new()
    };
    // Register tag usage for the ranking engine, at ingest time like Mastodon's
    // inbound-Create `Trends.tags.register`. Edits (above) don't re-register.
    if delivery_effects {
        tag::record_uses(&state.pool, stored.id, OffsetDateTime::now_utc().date()).await?;
    }
    crate::polls::store_remote_poll(state, author, &stored, object).await?;
    if delivery_effects {
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(plamenu_db::DbError::from)?;
        crate::conversations::record_direct_status(state, &mut conn, author, &stored).await?;
    }
    // Quote linkage is canonical object structure, like replies, attachments
    // and polls: explicit resolution and cold history must preserve it even
    // though merely fetching a post must not trigger live-delivery effects.
    let fields = quote_fields(object);
    let quote_notify = if let Some(quoted_uri) = fields.quoted_uri {
        link_inbound_quote(
            state,
            author,
            &stored,
            quoted_uri,
            fields.authorization,
            fields.legacy,
            quote_depth,
            delivery_effects,
        )
        .await?
    } else {
        None
    };
    // Link preview crawl (Mastodon crawls remote statuses too); the worker
    // scans the stored HTML for an eligible anchor. Skipped outright when the
    // body has no `https://` in it and there is no main link, which is the
    // large majority of inbound notes — the worker was otherwise spending four
    // or five round trips each to find nothing.
    if delivery_effects && crate::link_preview::may_carry_link(&stored) {
        plamenu_db::preview_card::enqueue_crawl(&state.pool, stored.id).await?;
    }
    Ok(RemoteIngestResult {
        status: stored,
        delivery_effects,
        mention_notify: if delivery_effects {
            mention_notify
        } else {
            Vec::new()
        },
        quote_notify: delivery_effects.then_some(quote_notify).flatten(),
    })
}

/// The `(url, description)` pairs of a Note's attachments, in the shape
/// [`store_remote_attachments`] would persist — for change detection.
fn attachment_pairs(object: &Value, caps: MediaCaps) -> Vec<(String, Option<String>)> {
    let mut pairs: Vec<(String, Option<String>)> = one_or_many(object.get("attachment"))
        .iter()
        .filter(|entry| !is_link_attachment(entry))
        .take(MAX_REMOTE_ATTACHMENTS)
        .filter_map(|entry| {
            let url = attachment_url(entry)?;
            federation_url(url).then(|| (url.to_owned(), attachment_description(entry)))
        })
        .collect();
    if let Some(primary) = object_primary_media(object, caps) {
        pairs.push((primary.url.to_owned(), None));
    }
    // No structured attachment and no native media, but images inline in the
    // body (Lemmy/PieFed markdown posts): recover them so they aren't lost to
    // the sanitizer. Gated on emptiness so a normal post with both attached and
    // text-inline images is untouched. Must mirror [`store_remote_attachments`].
    if pairs.is_empty() {
        pairs.extend(inline_image_urls(object).into_iter().map(|url| (url, None)));
    }
    pairs.sort();
    pairs.dedup();
    pairs
}

/// Safe (federation-URL) image sources carried only inline in the body's
/// `<img>` tags, deduped and capped. Used to recover images from posts that
/// declare no `attachment` and no native media object — Lemmy/PieFed render
/// their images inline, and the HTML sanitizer strips `<img>`, so without
/// this they vanish.
fn inline_image_urls(object: &Value) -> Vec<String> {
    let Some(content) = object_content(object) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    crate::link_preview::inline_image_srcs(content)
        .into_iter()
        .filter(|src| federation_url(src))
        .filter(|src| seen.insert(src.clone()))
        .take(MAX_REMOTE_ATTACHMENTS)
        .collect()
}

/// Applies a live broadcast's current state to the status' live attachment,
/// returning whether it actually moved.
///
/// This is deliberately *not* part of edit diffing. A live goes on air and off
/// again by way of plain `Update` activities that change no authored field, and
/// routing them through the edit path would stamp `edited_at`, snapshot a
/// revision and rebuild the attachment row (losing its identity) every time a
/// stream started or stopped.
///
/// Returns `Ok(false)` for anything that is not a live, or whose stored
/// attachment is not one — including a live that has since been replayed, which
/// is an ordinary video and belongs in the edit path.
async fn refresh_live_state(
    state: &AppState,
    existing: &status::Status,
    object: &Value,
) -> Result<bool, ApiError> {
    let Some(live) = live_info(object) else {
        return Ok(false);
    };
    let stored = media::for_statuses(&state.pool, &[existing.id])
        .await?
        .remove(&existing.id)
        .unwrap_or_default();
    let Some(item) = stored.into_iter().find(|item| item.live_state.is_some()) else {
        return Ok(false);
    };
    let was_live = item.live_state.as_deref() == Some("live");
    let changed = media::update_live(
        &state.pool,
        item.id,
        live.state.as_str(),
        hls_master_url(object),
    )
    .await?;
    // Going on air is the one transition worth telling anyone about, and only
    // the transition: `update_live` moves the row exactly once per go-live
    // (its UPDATE is conditional on the state actually changing), so a second
    // Update reporting the same thing — or two refreshes racing — cannot
    // notify twice. A permanent live that starts a new session does move
    // again, and does notify again, which is what its followers want.
    if changed && !was_live && live.state == LiveState::Live {
        notify_live_started(state, existing).await?;
    }
    Ok(changed)
}

/// Tells the people who asked to hear about this account's posts that its
/// broadcast has started.
///
/// Deliberately the same audience as a new post (`notify_follower_ids` — the
/// per-account "notify me" opt-in), not every follower: a live is worth an
/// interruption only to someone who said they wanted them from this account.
async fn notify_live_started(state: &AppState, existing: &status::Status) -> Result<(), ApiError> {
    let subscribers: Vec<(i64, i64)> = plamenu_db::follow::notify_follower_ids(
        &state.pool,
        existing.account_id,
        existing.language.as_deref(),
    )
    .await?
    .into_iter()
    .map(|recipient| (recipient, existing.id))
    .collect();
    notification::create_ungroupable_many(&state.pool, &subscribers, existing.account_id, "live")
        .await?;
    Ok(())
}

/// Ids and `(url, description)` pairs of a status' stored remote media.
async fn stored_attachment_state(
    state: &AppState,
    status_id: i64,
) -> Result<(Vec<i64>, Vec<(String, Option<String>)>), ApiError> {
    let stored = sqlx::query!(
        "SELECT id, remote_url, description, preview_card_id
         FROM media_attachments WHERE status_id = $1",
        status_id,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let ids = stored.iter().map(|item| item.id).collect();
    // Preview-derived audio does not exist in the signed ActivityPub object.
    // Exclude it from wire attachment diffing so an unrelated Update does not
    // repeatedly rebuild—or accidentally discard—the derived player.
    let mut pairs: Vec<(String, Option<String>)> = stored
        .into_iter()
        .filter(|item| item.preview_card_id.is_none())
        .filter_map(|item| item.remote_url.map(|url| (url, item.description)))
        .collect();
    pairs.sort();
    Ok((ids, pairs))
}

/// Applies an edit (`Update(Note)`) to an already-stored remote status (the
/// caller has verified that `existing` belongs to the activity's verified
/// sender): content re-sanitized, attachments, tags and mentions rebuilt,
/// and the version recorded for `/history`. Accounts that were already
/// mentioned are not notified again. An `Update` that changes nothing (e.g.
/// Mastodon redistributing a post for its quote stamp) is not an edit and
/// leaves the status untouched.
pub async fn update_remote_note(
    state: &AppState,
    author: &Account,
    existing: &status::Status,
    object: &Value,
) -> Result<status::Status, ApiError> {
    update_remote_note_in_context(
        state,
        author,
        existing,
        object,
        RemoteIngestContext::Delivery,
    )
    .await
}

#[allow(
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "content/context are distinct terms in a linear edit reconciliation pass"
)]
pub async fn update_remote_note_in_context(
    state: &AppState,
    author: &Account,
    existing: &status::Status,
    object: &Value,
    context: RemoteIngestContext,
) -> Result<status::Status, ApiError> {
    let content = remote_status_content(object, &state.config.domain);
    let external_url = attachment_link_target(object);
    let title = hoisted_title(object);
    let edited_at = object
        .get("updated")
        .and_then(Value::as_str)
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    if context.is_history()
        && existing
            .edited_at
            .is_some_and(|stored_at| edited_at <= stored_at)
    {
        return Ok(existing.clone());
    }
    let delivery_effects = context == RemoteIngestContext::Delivery;
    let mut promote_media = false;
    if delivery_effects {
        // This may be the first live event for a row discovered through
        // history. Claiming promotes it; an already-delivered row simply
        // returns false and continues through normal edit idempotency below.
        let previous =
            status::claim_delivery_side_effects_with_previous(&state.pool, existing.id).await?;
        promote_media = previous.as_deref() == Some("history");
        if promote_media {
            crate::remote_history::record_promotion();
        }
    }
    let (spoiler_text, declared_sensitive, language) = note_metadata(object);
    let sensitive = author.sensitized() || declared_sensitive;
    let collection_urls = account::collection_urls_of(&state.pool, author.id)
        .await?
        .unwrap_or(account::CollectionUrls {
            followers_url: String::new(),
            following_url: String::new(),
            outbox_url: String::new(),
        });

    // Quote state refreshes on every Update, including otherwise-identical
    // ones — Mastodon delivers the `quoteAuthorization` stamp of a freshly
    // accepted quote exactly as such an implicit Update (its
    // `update_quote_approval!`), so this must run before the no-change
    // early return below.
    refresh_inbound_quote(state, author, existing, object, delivery_effects).await?;
    // Mention rows and emoji definitions are canonical render data too. They
    // can change without any field in the status edit diff changing, so refresh
    // them before its no-change return.
    Box::pin(reconcile_note_mentions(
        state,
        existing,
        object,
        true,
        delivery_effects,
    ))
    .await?;
    Box::pin(store_remote_emojis(state, author, object, false)).await?;
    // The event sidecar likewise refreshes on every Update: a rescheduled
    // event may move `startTime` without touching any compared field. When it
    // moved — or the event was cancelled — everyone with a live RSVP is told,
    // since that is the whole point of having RSVP'd.
    if store_event_sidecar(state, existing, object).await? && delivery_effects {
        crate::events::notify_event_changed(state, existing).await?;
    }

    // A live broadcast's lifecycle — announced, on air, over — federates as an
    // ordinary Update, but it is not an edit: nothing the author wrote changed,
    // only whether the stream is running. Apply it straight to the attachment
    // *before* the diff below, so going live never marks the post edited or
    // snapshots a revision, and so the master URL the stream just published is
    // already stored when the diff reads it back.
    if !context.is_history() {
        refresh_live_state(state, existing, object).await?;
    }

    // Link-only invitation updates must also refresh when the post text is unchanged.
    plamenu_db::webxdc::set_remote_invitation(
        &state.pool,
        existing.id,
        crate::webxdc::remote_invitation(object),
    )
    .await?;

    let (current_media_ids, current_attachments) =
        stored_attachment_state(state, existing.id).await?;
    // A re-transcode can regenerate the HLS master (new UUID) + ladder while the
    // chosen rendition file the attachment compares on is unchanged; the master
    // is the reliable change signal, so a differing one forces a re-ingest (else
    // the proxy keeps serving a stale, now-404 master URL).
    let stored_hls_master = media::hls_master_for_status(&state.pool, existing.id).await?;
    if content == existing.content
        && spoiler_text == existing.spoiler_text
        && sensitive == existing.sensitive
        && language.as_deref() == existing.language.as_deref()
        && title.as_deref() == existing.title.as_deref()
        && external_url == existing.external_url.as_deref()
        && attachment_pairs(object, MediaCaps::load(state).await?) == current_attachments
        && hls_master_url(object).map(str::to_owned) == stored_hls_master
    {
        return Ok(existing.clone());
    }

    // The original becomes the first history entry on first edit.
    if !context.is_history() && !status_edit::any_for_status(&state.pool, existing.id).await? {
        status_edit::snapshot(
            &state.pool,
            status_edit::NewStatusEdit {
                status_id: existing.id,
                account_id: author.id,
                content: &existing.content,
                text: "",
                spoiler_text: &existing.spoiler_text,
                sensitive: existing.sensitive,
                media_ids: &current_media_ids,
                created_at: existing.created_at,
            },
        )
        .await?;
    }

    let updated = status::apply_edit(
        &state.pool,
        existing.id,
        status::StatusEdit {
            content: &content,
            spoiler_text: &spoiler_text,
            sensitive,
            language: language.as_deref(),
            edited_at,
            title: title.as_deref(),
            external_url,
            quote_approval_policy: parse_quote_policy(
                object,
                author,
                &collection_urls.followers_url,
                &collection_urls.following_url,
            ),
        },
    )
    .await?;

    plamenu_db::media::delete_remote_for_status(&state.pool, existing.id).await?;
    store_remote_attachments(state, &updated, object, context.is_history()).await?;
    if promote_media {
        media::promote_for_status(&state.pool, updated.id).await?;
    }
    tag::detach_all(&state.pool, existing.id).await?;
    store_remote_tags(state, author, &updated, object, !context.is_history()).await?;

    if !context.is_history() {
        let (new_media_ids, _) = stored_attachment_state(state, existing.id).await?;
        status_edit::snapshot(
            &state.pool,
            status_edit::NewStatusEdit {
                status_id: updated.id,
                account_id: author.id,
                content: &content,
                text: "",
                spoiler_text: &spoiler_text,
                sensitive,
                media_ids: &new_media_ids,
                created_at: edited_at,
            },
        )
        .await?;
    }
    // A changed text gets a fresh link preview, like Mastodon's
    // `ProcessStatusUpdateService#reset_preview_card!`.
    if delivery_effects && content != existing.content {
        let main_link = updated.external_url.as_deref();
        crate::link_preview::reset_for_edit(state, existing.id, &content, main_link).await?;
    }
    if delivery_effects {
        crate::actions::notify_status_edited(state, &updated).await?;
    }
    tracing::info!(author = %author.username, status = updated.id, "remote status edited");
    Ok(updated)
}

/// Synchronizes an edited remote post's quote relationship (Mastodon's
/// `update_quote!` / `update_quote_approval!`): a quote added by the edit is
/// linked, a dropped one removed, and a still-pending one re-verified — the
/// authorization stamp of a remote-to-remote quote only ever arrives on an
/// Update, often one with no other visible change.
async fn refresh_inbound_quote(
    state: &AppState,
    author: &Account,
    existing: &status::Status,
    object: &Value,
    delivery_effects: bool,
) -> Result<(), ApiError> {
    let fields = quote_fields(object);
    let stored_uri = existing.uri.as_deref().unwrap_or_default();
    let row = plamenu_db::quote::find_by_status_uri(&state.pool, stored_uri).await?;

    match (row, fields.quoted_uri) {
        (None, Some(quoted_uri)) => {
            // The edit introduced a quote.
            link_inbound_quote(
                state,
                author,
                existing,
                quoted_uri,
                fields.authorization,
                fields.legacy,
                0,
                delivery_effects,
            )
            .await
            .map(|_| ())
        }
        (Some(row), None) => {
            // The edit dropped the quote.
            plamenu_db::quote::delete_by_id(&state.pool, row.id).await?;
            Ok(())
        }
        (Some(row), Some(quoted_uri)) if row.state == "pending" => {
            plamenu_db::quote::set_quoted_uri(&state.pool, row.id, quoted_uri).await?;
            plamenu_db::quote::set_legacy(&state.pool, row.id, fields.legacy).await?;
            if let Some(stamp) = fields.authorization {
                plamenu_db::quote::set_pending_stamp(&state.pool, row.id, stamp).await?;
            }
            let effective_stamp = fields
                .authorization
                .map(str::to_owned)
                .or(row.approval_uri.clone());
            let outcome = evaluate_inbound_quote(
                state,
                author,
                stored_uri,
                quoted_uri,
                effective_stamp.as_deref(),
                0,
            )
            .await?;
            if apply_quote_outcome(state, &row, &outcome, effective_stamp.as_deref()).await? {
                schedule_quote_verification(
                    state,
                    row.id,
                    &outcome,
                    fields.legacy,
                    effective_stamp.is_some(),
                )
                .await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Writes an evaluation's result onto an existing pending quote row. Returns
/// whether the quote is still `pending` (the caller decides how to retry —
/// the ingest paths queue a fresh verification job, the verify worker
/// reschedules with its attempt budget).
pub(crate) async fn apply_quote_outcome(
    state: &AppState,
    row: &plamenu_db::quote::Quote,
    outcome: &QuoteOutcome,
    stamp: Option<&str>,
) -> Result<bool, ApiError> {
    if let (Some(quoted_status_id), Some(quoted_account_id)) =
        (outcome.quoted_status_id, outcome.quoted_account_id)
        && row.quoted_status_id.is_none()
    {
        plamenu_db::quote::link_quoted_target(
            &state.pool,
            row.id,
            quoted_status_id,
            quoted_account_id,
        )
        .await?;
    }
    match outcome.state {
        "accepted" => match stamp {
            Some(stamp) => {
                plamenu_db::quote::accept_with_stamp(&state.pool, row.id, stamp).await?;
            }
            None => plamenu_db::quote::set_state(&state.pool, row.id, "accepted").await?,
        },
        "rejected" => plamenu_db::quote::set_state(&state.pool, row.id, "rejected").await?,
        _ => return Ok(true),
    }
    Ok(false)
}

/// The quote-related fields of an inbound Note.
pub(crate) struct QuoteFields<'a> {
    pub quoted_uri: Option<&'a str>,
    pub authorization: Option<&'a str>,
    /// No FEP-044f `quote` property (Misskey-style aliases only): there is no
    /// consent handshake behind it — Mastodon's `legacy_quote?`.
    pub legacy: bool,
}

/// Extracts the quoted-post URI and `quoteAuthorization` stamp of an inbound
/// Note: Mastodon's flat-property alias order (`quote`, `_misskey_quote`,
/// `quoteUrl`, `quoteUri`), then a FEP-e232 quote Link tag. A flat property
/// and a Link tag naming the same URI are inherently one quote (a single
/// URI is extracted); an e232-only quote has no `quote` property, so it
/// lands on the legacy (no-consent-handshake) path — the correct semantics.
pub(crate) fn quote_fields(object: &Value) -> QuoteFields<'_> {
    let quoted_uri = ["quote", "_misskey_quote", "quoteUrl", "quoteUri"]
        .iter()
        .find_map(|key| object.get(*key).and_then(id_of))
        .or_else(|| e232_quote_link(object));
    QuoteFields {
        quoted_uri,
        authorization: object.get("quoteAuthorization").and_then(id_of),
        legacy: object.get("quote").is_none(),
    }
}

/// FEP-e232 object links: `tag` entries of type `Link` whose `mediaType` is
/// an `ActivityPub` type — references to other AP *objects*, as opposed to
/// ordinary hyperlink tags. Yields `(entry, rels)`; entries with rels we
/// don't understand are simply not acted on (their anchors remain in the
/// content), never rejected.
fn object_link_entries(object: &Value) -> impl Iterator<Item = &Value> {
    one_or_many(object.get("tag")).iter().filter(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("Link")
            && entry
                .get("mediaType")
                .and_then(Value::as_str)
                .is_some_and(|media_type| {
                    media_type == "application/activity+json"
                        || media_type.starts_with("application/ld+json")
                })
    })
}

/// The quoted-post URI carried only as a FEP-e232 Link tag — Misskey's rel
/// on the wire from Mitra, Sharkey and the streams/Hubzilla family (Sharkey
/// dual-emits flat aliases; streams does not).
fn e232_quote_link(object: &Value) -> Option<&str> {
    object_link_entries(object).find_map(|entry| {
        let quote_rel = one_or_many(entry.get("rel"))
            .iter()
            .filter_map(Value::as_str)
            .any(|rel| rel == "https://misskey-hub.net/ns#_misskey_quote" || rel == "quote");
        if !quote_rel {
            return None;
        }
        entry.get("href").and_then(Value::as_str)
    })
}

/// Records the quote relationship of an inbound quote post and decides its
/// state: self-quotes pass, quotes of local posts must match an
/// authorization we issued, quotes of remote posts are verified by fetching
/// the FEP-044f `QuoteAuthorization` stamp.
async fn resolve_or_fetch_quote_target(
    state: &AppState,
    sender: &Account,
    quoted_uri: &str,
    authorization: Option<&str>,
    quote_depth: usize,
) -> Result<Option<status::Status>, ApiError> {
    if let Some(known) = resolve_status_ref(state, quoted_uri).await? {
        return Ok(Some(known));
    }
    let local_prefix = format!("https://{}/", state.config.domain);
    if quoted_uri.starts_with(&local_prefix) || quote_depth > MAX_QUOTE_TARGET_FETCH_DEPTH {
        return Ok(None);
    }
    let has_remote_authorization = authorization
        .and_then(host_of)
        .is_some_and(|host| host != state.config.domain.as_str());
    let same_origin_as_sender = sender.uri.as_deref().and_then(host_of) == host_of(quoted_uri);
    if !has_remote_authorization && !same_origin_as_sender {
        return Ok(None);
    }

    let Some((object, author)) = fetch_remote_status_object(state, quoted_uri).await? else {
        return Ok(None);
    };
    if id_of(&object) != Some(quoted_uri) {
        return Ok(None);
    }
    tracing::info!(
        quoted_uri,
        quote_depth,
        "fetched missing quoted status for remote quote verification"
    );
    Box::pin(ingest_remote_note_with_quote_depth(
        state,
        &author,
        &object,
        quote_depth + 1,
        RemoteIngestContext::ExplicitResolution,
    ))
    .await
    .map(|result| Some(result.status))
}

/// The resolved verdict of one quote-verification pass.
pub(crate) struct QuoteOutcome {
    /// `accepted` | `pending` | `rejected`.
    pub state: &'static str,
    pub quoted_status_id: Option<i64>,
    pub quoted_account_id: Option<i64>,
    /// Whether the quoted author is one of ours (their consent flows through
    /// `QuoteRequest`, never through stamp verification).
    pub quoted_author_is_local: bool,
}

/// Resolves the quoted post (fetching it when eligible) and decides the quote
/// state — the shared core of ingest-time linking, Update refreshes and the
/// background verify worker.
pub(crate) async fn evaluate_inbound_quote(
    state: &AppState,
    sender: &Account,
    stored_uri: &str,
    quoted_uri: &str,
    authorization: Option<&str>,
    quote_depth: usize,
) -> Result<QuoteOutcome, ApiError> {
    let quoted =
        resolve_or_fetch_quote_target(state, sender, quoted_uri, authorization, quote_depth)
            .await?;
    let quoted_author = match &quoted {
        Some(target) => account::find_by_id(&state.pool, target.account_id).await?,
        None => None,
    };
    let quoted_author_is_local = quoted_author.as_ref().is_some_and(Account::is_local);

    let verdict = if quoted.as_ref().is_some_and(|t| t.account_id == sender.id) {
        // FEP-044f fast track: anyone may quote themselves.
        "accepted"
    } else if quoted_author_is_local {
        // Quotes of local posts are only accepted through our QuoteRequest
        // flow; anything else is unauthorized.
        "pending"
    } else if let (Some(authorization_uri), Some(quoted_author)) =
        (authorization, quoted_author.as_ref())
    {
        match verify_remote_quote_authorization(
            state,
            authorization_uri,
            stored_uri,
            quoted_uri,
            quoted_author,
        )
        .await
        {
            StampCheck::Verified => "accepted",
            // The issuer withdrew the stamp (or it never existed): Mastodon's
            // `quote.reject!` on a permanently-gone approval object.
            StampCheck::Gone => "rejected",
            StampCheck::Mismatch | StampCheck::Transient => "pending",
        }
    } else {
        "pending"
    };

    Ok(QuoteOutcome {
        state: verdict,
        quoted_status_id: quoted.as_ref().map(|t| t.id),
        quoted_account_id: quoted_author.as_ref().map(|a| a.id),
        quoted_author_is_local,
    })
}

/// Queues a background re-verification for a quote that stayed `pending`
/// (Mastodon's `RefetchAndVerifyQuoteWorker`): the stamp usually arrives only
/// via a later Update — which a relay may never forward — and the quoted post
/// may not have been fetchable yet. Excluded: quotes of local posts (those
/// wait for the `QuoteRequest` flow) and stamp-less legacy quotes whose
/// target is already linked — no handshake exists that could ever settle
/// them, they stay `pending` like on Mastodon.
async fn schedule_quote_verification(
    state: &AppState,
    quote_id: i64,
    outcome: &QuoteOutcome,
    legacy: bool,
    has_stamp: bool,
) -> Result<(), ApiError> {
    let settled_legacy = legacy && !has_stamp && outcome.quoted_status_id.is_some();
    if outcome.state == "pending" && !outcome.quoted_author_is_local && !settled_legacy {
        plamenu_db::quote_verify_job::enqueue(
            &state.pool,
            quote_id,
            OffsetDateTime::now_utc() + time::Duration::seconds(60),
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn link_inbound_quote(
    state: &AppState,
    sender: &Account,
    stored: &status::Status,
    quoted_uri: &str,
    authorization: Option<&str>,
    legacy: bool,
    quote_depth: usize,
    delivery_effects: bool,
) -> Result<Option<i64>, ApiError> {
    let stored_uri = stored.uri.as_deref().unwrap_or_default();

    // The QuoteRequest flow may have created the row before the post arrived.
    if let Some(existing) = plamenu_db::quote::find_by_status_uri(&state.pool, stored_uri).await? {
        plamenu_db::quote::link_status(&state.pool, existing.id, stored.id).await?;
        // Now that the quoting post exists locally, notify the quoted (local)
        // author — pointing at the quoting status, not their own post. This is
        // a live-delivery effect: an explicit fetch can link the row, while the
        // later Create owns the one notification through its delivery claim.
        let notify = if delivery_effects
            && existing.state == "accepted"
            && let Some(quoted_account_id) = existing.quoted_account_id
        {
            Some(quoted_account_id)
        } else {
            None
        };
        return Ok(notify);
    }

    let outcome = evaluate_inbound_quote(
        state,
        sender,
        stored_uri,
        quoted_uri,
        authorization,
        quote_depth,
    )
    .await?;
    let row = plamenu_db::quote::create(
        &state.pool,
        plamenu_db::quote::NewQuote {
            quote_id: plamenu_db::id::next(),
            status_id: Some(stored.id),
            status_uri: stored_uri,
            account_id: sender.id,
            quoted_status_id: outcome.quoted_status_id,
            quoted_account_id: outcome.quoted_account_id,
            state: outcome.state,
            activity_uri: None,
            approval_uri: authorization,
            quoted_uri: Some(quoted_uri),
            legacy,
        },
    )
    .await?;
    schedule_quote_verification(state, row.id, &outcome, legacy, authorization.is_some()).await?;
    Ok(None)
}

/// The outcome of fetching + checking a `QuoteAuthorization` stamp.
pub(crate) enum StampCheck {
    /// The stamp binds exactly this quoting/quoted pair and issuer.
    Verified,
    /// The stamp exists but does not authorize this quote — no retry helps.
    Mismatch,
    /// The origin says the stamp is permanently gone (404/410).
    Gone,
    /// A transient fetch failure; verification may succeed on retry.
    Transient,
}

/// Fetches and checks a `QuoteAuthorization` stamp: right type, hosted by
/// its own author, and binding exactly this quoting/quoted pair.
async fn verify_remote_quote_authorization(
    state: &AppState,
    authorization_uri: &str,
    quoting_uri: &str,
    quoted_uri: &str,
    quoted_author: &Account,
) -> StampCheck {
    match crate::instance_policy::can_federate_url(
        &state.pool,
        &state.config.domain,
        authorization_uri,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) | Err(_) => return StampCheck::Mismatch,
    }
    let stamp = match state.federation.fetch_object(authorization_uri).await {
        Ok(stamp) => stamp,
        Err(plamenu_federation::FederationError::Status(404 | 410)) => return StampCheck::Gone,
        Err(_) => return StampCheck::Transient,
    };
    let kind = stamp.get("type").and_then(Value::as_str);
    let attributed_to = stamp.get("attributedTo").and_then(id_of);
    let interacting = stamp.get("interactingObject").and_then(id_of);
    let target = stamp.get("interactionTarget").and_then(id_of);

    let verified = kind == Some("QuoteAuthorization")
        && interacting == Some(quoting_uri)
        && target == Some(quoted_uri)
        // The stamp must be hosted by whoever it claims issued it…
        && attributed_to.and_then(host_of) == host_of(authorization_uri)
        // …and that issuer must be the quoted author.
        && attributed_to == quoted_author.uri.as_deref();
    if verified {
        StampCheck::Verified
    } else {
        StampCheck::Mismatch
    }
}

/// Registers the custom emoji definitions carried by an inbound Note. These
/// definitions are canonical render data; registration stores remote metadata
/// but does not download media bytes.
async fn store_remote_emojis(
    state: &AppState,
    author: &Account,
    object: &Value,
    record_usage: bool,
) -> Result<(), ApiError> {
    let entries = one_or_many(object.get("tag"));
    if let Some(domain) = author.domain.as_deref()
        && !entries.is_empty()
    {
        crate::emoji::ingest_remote_emoji_tags(
            &state.pool,
            Some(&state.config.domain),
            domain,
            entries,
        )
        .await?;
        if record_usage {
            record_remote_emoji_usage(state, author, object).await?;
        }
    }
    Ok(())
}

async fn record_remote_emoji_usage(
    state: &AppState,
    author: &Account,
    object: &Value,
) -> Result<(), ApiError> {
    let Some(domain) = author.domain.as_deref() else {
        return Ok(());
    };
    let codes: Vec<String> = one_or_many(object.get("tag"))
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("Emoji"))
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(|name| name.trim_matches(':').to_owned())
        .collect();
    let emoji = plamenu_db::custom_emoji::lookup(&state.pool, &codes, Some(domain)).await?;
    let ids: Vec<i64> = emoji.iter().map(|emoji| emoji.id).collect();
    let origins: Vec<i64> = plamenu_db::custom_emoji::find_managed_by_ids(&state.pool, &ids)
        .await?
        .into_iter()
        .map(|emoji| emoji.origin_id)
        .collect();
    plamenu_db::custom_emoji::record_post_usage(&state.pool, author.id, &origins).await?;
    Ok(())
}

/// Persists the non-mention, non-emoji `tag` entries of an inbound Note:
/// hashtags and, outside cold history, referenced featured collections. Every
/// mention — both `tag` Mention entries and `to`/`cc`/`audience` addressing —
/// is handled together by [`reconcile_note_mentions`], which resolves the
/// targets in batches and rewrites the mention rows atomically.
async fn store_remote_tags(
    state: &AppState,
    author: &Account,
    stored: &status::Status,
    object: &Value,
    resolve_collections: bool,
) -> Result<(), ApiError> {
    let entries = one_or_many(object.get("tag"));
    if entries.is_empty() {
        return Ok(());
    }
    let hashtag_names: Vec<&str> = entries
        .iter()
        .take(20)
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("Hashtag"))
        .filter_map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .map(|n| n.trim_start_matches('#'))
                .filter(|n| !n.is_empty() && n.len() <= 100)
        })
        .collect();
    if !hashtag_names.is_empty() {
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(plamenu_db::DbError::from)?;
        tag::ensure_and_attach_many(&mut conn, stored.id, &hashtag_names).await?;
    }
    for entry in entries.iter().take(20) {
        if resolve_collections
            && entry.get("type").and_then(Value::as_str) == Some("FeaturedCollection")
        {
            // A referenced collection (Mastodon's `process_tagged_collection`):
            // resolve it by `id` — the author's own collection, known or
            // fetched — and record the reference.
            if let Some(id) = entry.get("id").and_then(Value::as_str)
                && let Some(collection) =
                    crate::collections::find_or_fetch_tagged_collection(state, author, id).await?
            {
                plamenu_db::tagged_object::add(&state.pool, stored.id, collection.id, id).await?;
            }
        }
    }
    Ok(())
}

/// Rebuilds a status's mention rows from an inbound Note's `tag` Mention entries
/// and its `to`/`cc`/`audience` addressing, optionally notifying newly mentioned
/// local accounts when the object arrived as a live delivery.
///
/// Every target is resolved up front and in bulk: local usernames from the tag
/// mentions and from the addressing are each resolved with one
/// [`account::find_local_by_usernames`] query instead of one lookup per
/// recipient, and remote mentioned actors are fetched only as needed and bounded
/// by the tag cap. This closes the amplification where one hostile signed
/// delivery packing many local URLs drove an unbounded serial query / mention /
/// notification fan-out.
///
/// The whole mention set is then written in ONE transaction. On an edit
/// (`rebuild`) the previous rows are detached in that same transaction, so a
/// concurrent reader never observes a half-rebuilt set and a mid-rebuild failure
/// can never strand a private post with its recipients dropped. (Committing the
/// status row, its mentions, and the outbox together is the broader finding
/// #18.) Notifications fire only after the commit; they are additive and, on an
/// edit, are suppressed for any recipient the status already had.
///
/// Addressing semantics match Mastodon's `process_audience`: `to`, `cc`, and
/// `audience` grant silent access — delivery and private-post visibility —
/// without a notification. A real (`tag`) mention always outranks a silent
/// audience one for the same account and is the only audience-related source
/// of a mention notification.
fn carries_mentions(object: &Value, domain: &str) -> bool {
    one_or_many(object.get("tag"))
        .iter()
        .take(20)
        .any(|entry| entry.get("type").and_then(Value::as_str) == Some("Mention"))
        || !audience_recipients(object, domain).is_empty()
}

/// Records a mention id into the ordered, deduplicated set, keeping the
/// strongest silent flag (a non-silent tag mention wins over a silent audience
/// one for the same account).
fn record_mention(
    order: &mut Vec<i64>,
    silent_by_id: &mut HashMap<i64, bool>,
    id: i64,
    silent: bool,
) {
    use std::collections::hash_map::Entry;
    match silent_by_id.entry(id) {
        Entry::Occupied(mut entry) => *entry.get_mut() = *entry.get() && silent,
        Entry::Vacant(entry) => {
            entry.insert(silent);
            order.push(id);
        }
    }
}

async fn reconcile_note_mentions(
    state: &AppState,
    stored: &status::Status,
    object: &Value,
    rebuild: bool,
    notification_effects: bool,
) -> Result<Vec<i64>, ApiError> {
    // Classify the tag Mention hrefs into local usernames and remote hrefs.
    let mut local_tag_uris: Vec<String> = Vec::new();
    let mut remote_mention_hrefs: Vec<&str> = Vec::new();
    for entry in one_or_many(object.get("tag")).iter().take(20) {
        if entry.get("type").and_then(Value::as_str) != Some("Mention") {
            continue;
        }
        let Some(href) = entry.get("href").and_then(Value::as_str) else {
            continue;
        };
        if crate::local_identity::has_local_actor_shape(&state.config.domain, href) {
            local_tag_uris.push(href.to_owned());
        } else {
            remote_mention_hrefs.push(href);
        }
    }

    // Resolve local tag mentions in one query; remote mentioned actors as needed.
    let tag_uri_refs: Vec<&str> = local_tag_uris.iter().map(String::as_str).collect();
    let local_tag_by_uri: HashMap<String, i64> =
        crate::local_identity::find_actors(&state.pool, &state.config.domain, &tag_uri_refs)
            .await?
            .into_iter()
            .map(|(uri, account)| (uri, account.id))
            .collect();
    let remote_mention_ids = resolve_remote_mentions(state, remote_mention_hrefs).await?;

    // Resolve the `to`/`cc`/`audience` recipients (already capped/deduped) in
    // one query.
    let audience = audience_recipients(object, &state.config.domain);
    let audience_refs: Vec<&str> = audience.iter().map(|(uri, _)| uri.as_str()).collect();
    let audience_by_uri: HashMap<String, i64> =
        crate::local_identity::find_actors(&state.pool, &state.config.domain, &audience_refs)
            .await?
            .into_iter()
            .map(|(uri, account)| (uri, account.id))
            .collect();

    // Build the mention set — first-seen order, deduplicated, a non-silent
    // mention winning over a silent one — and the notify list.
    let mut order: Vec<i64> = Vec::new();
    let mut silent_by_id: HashMap<i64, bool> = HashMap::new();
    let mut notify: Vec<i64> = Vec::new();
    let mut notify_seen: HashSet<i64> = HashSet::new();

    // Tag mentions: real (non-silent); local ones notify.
    for uri in &local_tag_uris {
        if let Some(&id) = local_tag_by_uri.get(uri) {
            record_mention(&mut order, &mut silent_by_id, id, false);
            if notify_seen.insert(id) {
                notify.push(id);
            }
        }
    }
    // Remote mentions: real (non-silent); never a local notification.
    for &id in &remote_mention_ids {
        record_mention(&mut order, &mut silent_by_id, id, false);
    }
    // Audience addressing grants silent access but never manufactures a
    // notification. Current Pleroma and Mitra serializers emit a real Mention
    // tag for ordinary replies; arbitrary `to` has broader delivery meanings.
    for (uri, _) in &audience {
        if let Some(&id) = audience_by_uri.get(uri) {
            record_mention(&mut order, &mut silent_by_id, id, true);
        }
    }
    let mention_rows: Vec<(i64, bool)> = order.iter().map(|id| (*id, silent_by_id[id])).collect();

    // Persist the whole set atomically; an edit clears the old rows in the same
    // transaction so a reader never sees a partially rebuilt mention set.
    let already_mentioned = if rebuild || !mention_rows.is_empty() {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        let already = if rebuild {
            mention::detach_all(&mut *tx, stored.id).await?
        } else {
            Vec::new()
        };
        mention::attach_many(&mut *tx, stored.id, &mention_rows).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        already
    } else {
        Vec::new()
    };

    // Notify after the commit; an edit does not re-notify a pre-edit recipient.
    let already: HashSet<i64> = already_mentioned.into_iter().collect();
    let fresh: Vec<i64> = notify
        .into_iter()
        .filter(|id| !already.contains(id))
        .collect();
    if notification_effects {
        notification::create_mentions_many(&state.pool, &fresh, stored.account_id, stored.id)
            .await?;
    }
    Ok(fresh)
}

/// The distinct local-looking usernames addressed by a Note's `to`/`cc`/
/// `audience`, each paired with whether it first appeared in `to`. Public
/// sentinels are dropped; duplicates collapse to their first occurrence, so
/// `to` wins over a later `cc`. Deduplicated and capped at
/// [`MAX_AUDIENCE_RECIPIENTS`] so a hostile actor can't drive an unbounded
/// serial lookup / mention / notification fan-out from one delivery. `audience`
/// is FEP-1b12's community claim: Lemmy stamps the group there
/// (sometimes without repeating it in `to`/`cc`), and a local group's
/// submission attribution rides these mention rows.
fn audience_recipients(object: &Value, domain: &str) -> Vec<(String, bool)> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, bool)> = Vec::new();
    for field in ["to", "cc", "audience"] {
        let uris = match object.get(field) {
            Some(Value::String(one)) => vec![one.as_str()],
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        for uri in uris {
            if matches!(uri, plamenu_ap::activity::PUBLIC | "as:Public" | "Public") {
                continue;
            }
            if !crate::local_identity::has_local_actor_shape(domain, uri) {
                continue;
            }
            if !seen.insert(uri.to_owned()) {
                continue;
            }
            out.push((uri.to_owned(), field == "to"));
            if out.len() >= MAX_AUDIENCE_RECIPIENTS {
                return out;
            }
        }
    }
    out
}

/// Resolves inbound remote Mention hrefs to account ids, first-seen order:
/// known actors resolve in one batched lookup, and only genuine misses pay the
/// per-actor path (policy check + network fetch). Deduplicated first, so an
/// actor named twice in `tag` is neither looked up nor fetched twice.
async fn resolve_remote_mentions(state: &AppState, hrefs: Vec<&str>) -> Result<Vec<i64>, ApiError> {
    let mut deduped_hrefs: Vec<&str> = Vec::new();
    for href in hrefs {
        if !deduped_hrefs.contains(&href) {
            deduped_hrefs.push(href);
        }
    }
    let known_by_uri: HashMap<String, i64> = if deduped_hrefs.is_empty() {
        HashMap::new()
    } else {
        account::find_by_uris(&state.pool, &deduped_hrefs)
            .await?
            .into_iter()
            .filter_map(|account| account.uri.as_ref().map(|uri| (uri.clone(), account.id)))
            .collect()
    };
    let mut resolved: Vec<i64> = Vec::new();
    for href in deduped_hrefs {
        if let Some(&id) = known_by_uri.get(href) {
            resolved.push(id);
        } else if let Some(remote) = resolve_mentioned_actor(state, href).await? {
            resolved.push(remote.id);
        }
    }
    Ok(resolved)
}

/// Resolves a non-local Mention `href` (the mentioned actor's AP id) to its
/// account: known accounts by uri, unknown ones by a best-effort policy-
/// checked actor fetch — Mastodon's `process_mention`. `None` leaves the
/// content's mention anchor an external link.
async fn resolve_mentioned_actor(
    state: &AppState,
    href: &str,
) -> Result<Option<Account>, ApiError> {
    // Only the `/users/{name}` shape (handled by the caller) names a local
    // user; any other local-domain href must not be re-fetched as if we
    // were a remote server — that would mint a duplicate account row.
    if host_of(href) == Some(state.config.domain.as_str()) {
        return Ok(None);
    }
    if let Some(known) = account::find_by_uri(&state.pool, href).await? {
        return Ok(Some(known));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, href).await? {
        return Ok(None);
    }
    match state.federation.fetch_actor(href).await {
        Ok(actor) => Ok(Some(refresh_remote_actor(state, &actor).await?)),
        Err(error) => {
            tracing::debug!(%error, href, "mentioned actor fetch failed");
            Ok(None)
        }
    }
}

/// An attachment's alt text — Mastodon's parser prefers `summary` over
/// `name`.
fn attachment_description(entry: &Value) -> Option<String> {
    ["summary", "name"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn attachment_url(entry: &Value) -> Option<&str> {
    entry.get("url").and_then(|url| url_to_href(url, None))
}

/// Whether an `attachment` entry is a hyperlink rather than media: a `Link`
/// (Lemmy puts a link post's target here, as `href`) or an attached HTML page
/// (GNU Social attaches those as `Document`s; Mitra skips them too). Neither
/// must be stored — let alone fetched — as media.
fn is_link_attachment(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("Link") {
        return true;
    }
    entry
        .get("mediaType")
        .and_then(Value::as_str)
        .is_some_and(|media_type| media_type == "text/html")
}

/// A link post's target URL: the first `Link` attachment's `href` (Lemmy only
/// ever reads the first attachment either).
fn attachment_link_target(object: &Value) -> Option<&str> {
    one_or_many(object.get("attachment"))
        .iter()
        .find(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("Link")
                && crate::webxdc::remote_invitation(&serde_json::json!({"attachment": entry}))
                    .is_none()
        })
        .and_then(link_href)
        .filter(|url| linkable_url(url))
}

fn content_type_from_url(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "apng" => Some("image/apng"),
        "avif" => Some("image/avif"),
        "gif" => Some("image/gif"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        "m4a" => Some("audio/mp4"),
        "mp3" => Some("audio/mpeg"),
        "oga" | "ogg" | "opus" => Some("audio/ogg"),
        "wav" => Some("audio/wav"),
        "m4v" | "mp4" => Some("video/mp4"),
        "ogv" => Some("video/ogg"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "md" | "markdown" => Some("text/markdown"),
        "pdf" => Some("application/pdf"),
        "txt" => Some("text/plain"),
        "zip" => Some("application/zip"),
        _ => None,
    }
}

fn attachment_content_type(entry: &Value, url: &str) -> String {
    entry
        .get("mediaType")
        .and_then(Value::as_str)
        .or_else(|| url_to_media_type(entry.get("url")))
        .or_else(|| content_type_from_url(url))
        .unwrap_or("application/octet-stream")
        .to_owned()
}

/// The base83 alphabet blurhashes are encoded in.
const BLURHASH_ALPHABET: &str =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

/// Mastodon's `supported_blurhash?`: well-formed and at most 5x5
/// components (we also require the length the components imply — a
/// truncated hash would fail in every client).
fn valid_blurhash(hash: &str) -> bool {
    let mut chars = hash.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let Some(size_flag) = BLURHASH_ALPHABET.find(first) else {
        return false;
    };
    if !chars.all(|c| BLURHASH_ALPHABET.contains(c)) {
        return false;
    }
    let components_x = size_flag % 9 + 1;
    let components_y = size_flag / 9 + 1;
    components_x <= 5 && components_y <= 5 && hash.len() == 4 + 2 * components_x * components_y
}

/// A `focalPoint` array as the focus pair, if well-formed.
fn parse_focal_point(entry: &Value) -> Option<(f64, f64)> {
    let point = entry.get("focalPoint")?.as_array()?;
    Some((point.first()?.as_f64()?, point.get(1)?.as_f64()?))
}

/// Persists `attachment` entries of an inbound Note as remote media.
async fn store_remote_attachments(
    state: &AppState,
    stored: &status::Status,
    object: &Value,
    force_on_demand: bool,
) -> Result<(), ApiError> {
    // Owncast's go-live Note carries a cosmetic preview Image while its
    // playable HLS endpoint lives in WebFinger. Once the author has been
    // positively identified as Owncast, store the live video the Note
    // represents instead of the broken-looking image tile. Every other Note
    // continues through the generic attachment path unchanged.
    if crate::owncast::maybe_store_live_note(state, stored, object).await? {
        return Ok(());
    }
    // Counts every media row created so the inline-image fallback below runs
    // only when the object declared no structured or native media.
    let mut created = 0usize;
    for entry in one_or_many(object.get("attachment"))
        .iter()
        .filter(|entry| !is_link_attachment(entry))
        .take(MAX_REMOTE_ATTACHMENTS)
    {
        let Some(url) = attachment_url(entry) else {
            continue;
        };
        if !federation_url(url) {
            continue;
        }
        let content_type = attachment_content_type(entry, url);
        let description = attachment_description(entry);
        let blurhash = entry
            .get("blurhash")
            .and_then(Value::as_str)
            .filter(|hash| valid_blurhash(hash));
        // `icon` is the federated thumbnail: an Image object or a bare URL.
        let thumbnail_remote_url = entry
            .get("icon")
            .and_then(|icon| {
                if icon.is_object() {
                    icon.get("url").and_then(Value::as_str)
                } else {
                    icon.as_str()
                }
            })
            .filter(|icon_url| federation_url(icon_url));
        media::create_remote(
            &state.pool,
            media::NewRemoteMedia {
                account_id: stored.account_id,
                status_id: stored.id,
                remote_url: url,
                content_type: &content_type,
                description: description.as_deref(),
                blurhash,
                focus: parse_focal_point(entry),
                thumbnail_remote_url,
                width: entry
                    .get("width")
                    .and_then(Value::as_i64)
                    .and_then(|w| i32::try_from(w).ok()),
                height: entry
                    .get("height")
                    .and_then(Value::as_i64)
                    .and_then(|h| i32::try_from(h).ok()),
                remote_audio_url: None,
                duration: None,
                // History images use the ordinary proxy cache lane on first
                // view. History A/V remains in the play-triggered lane; the
                // independent `history_deferred` flag suppresses ingest work
                // for both and is cleared by a later live delivery.
                download_on_demand: force_on_demand
                    && (content_type.starts_with("audio/") || content_type.starts_with("video/")),
                history_deferred: force_on_demand,
                ..Default::default()
            },
        )
        .await?;
        created += 1;
    }
    // A native-media object (`Video`/`Audio`/`Image`) carries its file in the
    // `url` Link tree, not in `attachment` — store the best candidate as this
    // status' media. `create_remote` is idempotent per (status, url), so an
    // object that (unusually) duplicates the file as an attachment stores it
    // once. Must stay in step with [`attachment_pairs`] or edit-diffing breaks.
    if let Some(primary) = object_primary_media(object, MediaCaps::load(state).await?) {
        media::create_remote(
            &state.pool,
            media::NewRemoteMedia {
                account_id: stored.account_id,
                status_id: stored.id,
                remote_url: primary.url,
                content_type: primary.media_type,
                thumbnail_remote_url: primary.poster,
                width: primary.width,
                height: primary.height,
                remote_audio_url: primary.audio_url,
                // The AP object declares the length — clients show the real
                // duration before the file is downloaded and probed.
                duration: object
                    .get("duration")
                    .and_then(Value::as_str)
                    .and_then(parse_iso8601_seconds),
                download_on_demand: primary.defer_download
                    || (force_on_demand
                        && (primary.media_type.starts_with("audio/")
                            || primary.media_type.starts_with("video/"))),
                history_deferred: force_on_demand && !primary.defer_download,
                hls_master_url: primary.hls_master_url,
                renditions: primary.renditions,
                live_state: primary.live.map(|live| live.state.as_str()),
                live_permanent: primary.live.is_some_and(|live| live.permanent),
                ..Default::default()
            },
        )
        .await?;
        created += 1;
    }
    // Fallback: the object declared no attachment and no native media, but its
    // body carries images inline (Lemmy/PieFed markdown posts). Recover them —
    // the sanitizer will otherwise drop the `<img>` and the post arrives with
    // no image at all.
    if created == 0 {
        store_inline_image_attachments(state, stored, object, force_on_demand).await?;
    }
    Ok(())
}

/// Recovers images carried only inline in the body's `<img>` tags as remote
/// media (Lemmy/PieFed markdown posts declare no `attachment` and no native
/// media object; the sanitizer strips the `<img>`). Called only when
/// [`store_remote_attachments`] created nothing else. `create_remote` is
/// idempotent per (status, url); the content type is a hint the media job
/// re-probes on download. Mirrors the fallback in [`attachment_pairs`].
async fn store_inline_image_attachments(
    state: &AppState,
    stored: &status::Status,
    object: &Value,
    force_on_demand: bool,
) -> Result<(), ApiError> {
    for url in inline_image_urls(object) {
        let content_type = content_type_from_url(&url).unwrap_or("image/jpeg");
        media::create_remote(
            &state.pool,
            media::NewRemoteMedia {
                account_id: stored.account_id,
                status_id: stored.id,
                remote_url: &url,
                content_type,
                description: None,
                blurhash: None,
                focus: None,
                thumbnail_remote_url: None,
                width: None,
                height: None,
                remote_audio_url: None,
                duration: None,
                download_on_demand: false,
                history_deferred: force_on_demand,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_durations_parse() {
        assert_eq!(parse_iso8601_seconds("PT10185S"), Some(10185.0));
        assert_eq!(parse_iso8601_seconds("PT2H49M45S"), Some(10185.0));
        assert_eq!(parse_iso8601_seconds("PT1145S"), Some(1145.0));
        assert_eq!(parse_iso8601_seconds("PT1M30.5S"), Some(90.5));
        assert_eq!(parse_iso8601_seconds("P1DT1S"), Some(86401.0));
        assert_eq!(parse_iso8601_seconds("PT0S"), None, "zero is not a length");
        assert_eq!(parse_iso8601_seconds("garbage"), None);
        assert_eq!(parse_iso8601_seconds("PT5"), None, "dangling number");
    }

    #[test]
    fn inline_image_urls_extracts_safe_images_deduped() {
        // A Lemmy/PieFed post: the image lives only in an inline `<img>`
        // (empty `attachment`), which the sanitizer would drop.
        let object = serde_json::json!({
            "content": "<p>look</p><p>\
                <img src=\"https://h.example/pictrs/a.avif\" alt=\"x\">\
                <img src=\"http://h.example/insecure.png\">\
                <img src=\"https://h.example/pictrs/a.avif\"></p>",
        });
        assert_eq!(
            inline_image_urls(&object),
            ["https://h.example/pictrs/a.avif"],
            "https only, first-seen order, deduped"
        );
    }

    #[test]
    fn inline_image_urls_empty_without_images() {
        let object = serde_json::json!({ "content": "<p>just text, no pictures</p>" });
        assert!(inline_image_urls(&object).is_empty());
        let no_content = serde_json::json!({ "type": "Note" });
        assert!(inline_image_urls(&no_content).is_empty());
    }

    #[test]
    fn top_level_document_selects_non_html_file_link() {
        let object = serde_json::json!({
            "type": "Document",
            "url": [
                {"type": "Link", "mediaType": "text/html",
                 "href": "https://hub.example/cloud/alice/report"},
                {"type": "Link", "mediaType": "text/markdown",
                 "href": "https://hub.example/attach/report.md", "contentSize": 2701},
            ],
        });
        let primary = object_primary_media(
            &object,
            MediaCaps {
                max_height: 1080,
                budget_bytes: 2048 * 1024 * 1024,
            },
        )
        .unwrap();
        assert_eq!(primary.url, "https://hub.example/attach/report.md");
        assert_eq!(primary.media_type, "text/markdown");
        assert!(!primary.defer_download);
    }

    #[test]
    fn audience_recipients_reads_to_cc_audience_and_flags_to() {
        let object = serde_json::json!({
            "to": ["https://plamenu.test/users/alice",
                   "https://www.w3.org/ns/activitystreams#Public"],
            "cc": "https://plamenu.test/users/bob",
            "audience": ["https://plamenu.test/users/team", "https://other.example/users/x"],
        });
        // Public sentinels and non-local URLs are dropped; `to` recipients are
        // marked for notification, `cc`/`audience` are silent.
        assert_eq!(
            audience_recipients(&object, "plamenu.test"),
            vec![
                ("https://plamenu.test/users/alice".to_owned(), true),
                ("https://plamenu.test/users/bob".to_owned(), false),
                ("https://plamenu.test/users/team".to_owned(), false),
            ],
        );
    }

    #[test]
    fn audience_recipients_dedups_first_occurrence_wins() {
        // A name in both `to` and `cc` collapses to its `to` occurrence, so it
        // is notified exactly once (the notify-once contract).
        let object = serde_json::json!({
            "to": ["https://plamenu.test/users/alice"],
            "cc": ["https://plamenu.test/users/alice", "https://plamenu.test/users/alice"],
        });
        assert_eq!(
            audience_recipients(&object, "plamenu.test"),
            vec![("https://plamenu.test/users/alice".to_owned(), true)],
        );
    }

    #[test]
    fn audience_recipients_caps_the_fan_out() {
        // A hostile Note packs thousands of distinct local recipients into `to`.
        let many: Vec<String> = (0..5_000)
            .map(|n| format!("https://plamenu.test/users/u{n}"))
            .collect();
        let object = serde_json::json!({ "to": many });
        let recipients = audience_recipients(&object, "plamenu.test");
        assert_eq!(recipients.len(), MAX_AUDIENCE_RECIPIENTS);
        assert_eq!(
            recipients[0],
            ("https://plamenu.test/users/u0".to_owned(), true)
        );
    }

    #[test]
    fn audience_recipients_accepts_immutable_actor_ids() {
        let object = serde_json::json!({
            "to": "https://plamenu.test/ap/accounts/42",
            "cc": "https://plamenu.test/ap/accounts/43",
        });
        assert_eq!(
            audience_recipients(&object, "plamenu.test"),
            vec![
                ("https://plamenu.test/ap/accounts/42".to_owned(), true),
                ("https://plamenu.test/ap/accounts/43".to_owned(), false),
            ],
        );
    }

    /// The rendition caps steer selection; nothing fitting falls back to the
    /// smallest so the post still renders.
    #[test]
    fn rendition_selection_honours_caps() {
        let object = serde_json::json!({
            "type": "Video",
            "url": [{
                "type": "Link",
                "mediaType": "application/x-mpegURL",
                "href": "https://origin.example/master.m3u8",
                "tag": [
                    {"type": "Link", "mediaType": "video/mp4", "height": 1080,
                     "href": "https://origin.example/v-1080.mp4", "size": 900_000_000},
                    {"type": "Link", "mediaType": "video/mp4", "height": 480,
                     "href": "https://origin.example/v-480.mp4", "size": 300_000_000},
                    {"type": "Link", "mediaType": "video/mp4", "height": 0,
                     "href": "https://origin.example/v-audio.mp4", "size": 100_000_000},
                ],
            }],
        });
        let caps = |max_height, budget_mb: i64| MediaCaps {
            max_height,
            budget_bytes: budget_mb * 1024 * 1024,
        };

        // Roomy caps: the 480p sweet-spot rendition (NOT the tallest) is the
        // single progressive `url` dumb clients get, still paired with the
        // separated-audio stream (which the HLS ladder keeps for the player).
        let primary = object_primary_media(&object, caps(1080, 2048)).unwrap();
        assert_eq!(primary.url, "https://origin.example/v-480.mp4");
        assert_eq!(
            primary.audio_url,
            Some("https://origin.example/v-audio.mp4")
        );
        assert!(primary.defer_download, "separated audio is always deferred");
        // The whole ladder is still persisted for the quality selector.
        assert_eq!(primary.renditions.len(), 3, "video ladder + audio rung");

        // A height cap that excludes 480p is moot here (480 already preferred);
        // a lower cap still lands on 480.
        let primary = object_primary_media(&object, caps(720, 2048)).unwrap();
        assert_eq!(primary.url, "https://origin.example/v-480.mp4");

        // A byte budget (video + audio together) keeps 480p.
        let primary = object_primary_media(&object, caps(1080, 500)).unwrap();
        assert_eq!(primary.url, "https://origin.example/v-480.mp4");

        // Nothing fits: the lightest rendition still gets stored so the post
        // renders and the origin link works.
        let primary = object_primary_media(&object, caps(1080, 1)).unwrap();
        assert_eq!(primary.url, "https://origin.example/v-480.mp4");
    }

    /// A `PeerTube` live, in each of the three states it federates in. The
    /// shapes here are copied from a real 8.2.3 peer: note that the master
    /// `Link`'s `tag` array carries NO file Links at all, which is what makes
    /// a live invisible to the ordinary rendition mining.
    fn live_object(state: i64, master: bool, permanent: bool) -> Value {
        let mut url = vec![serde_json::json!({
            "type": "Link", "mediaType": "text/html",
            "href": "https://peer.example/w/abc",
        })];
        if master {
            url.push(serde_json::json!({
                "type": "Link",
                "mediaType": "application/x-mpegURL",
                "href": "https://peer.example/static/streaming-playlists/hls/vid/master.m3u8",
                "tag": [
                    {"type": "Infohash", "name": "deadbeef"},
                    {"type": "Link", "name": "sha256", "mediaType": "application/json",
                     "href": "https://peer.example/static/streaming-playlists/hls/vid/segments-sha256.json"},
                ],
            }));
        }
        serde_json::json!({
            "type": "Video",
            "id": "https://peer.example/videos/watch/vid",
            "duration": "PT0S",
            "isLiveBroadcast": true,
            "permanentLive": permanent,
            "state": state,
            "url": url,
            "icon": [{"type": "Image", "url": "https://peer.example/thumb.jpg", "width": 280}],
        })
    }

    #[test]
    fn a_live_gets_an_attachment_even_before_it_has_a_playlist() {
        let caps = MediaCaps {
            max_height: 1080,
            budget_bytes: 2048 * 1024 * 1024,
        };
        // Announced: no master playlist exists yet anywhere in the object.
        let announced = live_object(4, false, false);
        let waiting = object_primary_media(&announced, caps).unwrap();
        assert_eq!(
            waiting.live.map(|l| l.state),
            Some(LiveState::Waiting),
            "state 4 is an announced broadcast"
        );
        assert_eq!(
            waiting.url, "https://peer.example/videos/watch/vid",
            "identity is the video's own AP id — the one address that exists \
             in every state"
        );
        assert_eq!(waiting.hls_master_url, None);
        assert_eq!(waiting.poster, Some("https://peer.example/thumb.jpg"));
        assert!(waiting.defer_download, "a live is never downloaded");
        assert!(waiting.renditions.is_empty());

        // On air: the master appears, but the identity does not move — so the
        // attachment is updated in place rather than replaced.
        let on_air = live_object(1, true, false);
        let live = object_primary_media(&on_air, caps).unwrap();
        assert_eq!(live.live.map(|l| l.state), Some(LiveState::Live));
        assert_eq!(live.url, "https://peer.example/videos/watch/vid");
        assert_eq!(
            live.hls_master_url,
            Some("https://peer.example/static/streaming-playlists/hls/vid/master.m3u8")
        );

        // Over: PeerTube keeps advertising the master it is about to delete,
        // so the state — not the URL's presence — is what says it ended.
        let finished = live_object(5, true, false);
        let ended = object_primary_media(&finished, caps).unwrap();
        assert_eq!(ended.live.map(|l| l.state), Some(LiveState::Ended));

        // A permanent live is flagged so "off air" can be phrased as between
        // broadcasts rather than finished.
        let recurring = live_object(4, false, true);
        let permanent = object_primary_media(&recurring, caps).unwrap();
        assert!(permanent.live.unwrap().permanent);
    }

    #[test]
    fn a_replayed_live_is_an_ordinary_video_again() {
        // Saving a replay gives the (still `isLiveBroadcast`) object a real
        // rendition ladder. It must take the VOD path — a stored `live` state
        // would leave it permanently unplayable.
        let mut object = live_object(1, true, false);
        object["url"][1]["tag"] = serde_json::json!([
            {"type": "Link", "mediaType": "video/mp4", "height": 480,
             "href": "https://peer.example/replay-480.mp4", "size": 10_000_000},
        ]);
        let primary = object_primary_media(
            &object,
            MediaCaps {
                max_height: 1080,
                budget_bytes: 2048 * 1024 * 1024,
            },
        )
        .unwrap();
        assert!(primary.live.is_none(), "a replay is just a video");
        assert_eq!(primary.url, "https://peer.example/replay-480.mp4");
        assert_eq!(primary.renditions.len(), 1);
    }

    #[test]
    fn a_video_that_is_not_a_broadcast_is_never_treated_as_one() {
        let caps = MediaCaps {
            max_height: 1080,
            budget_bytes: 2048 * 1024 * 1024,
        };
        // PeerTube stamps `isLiveBroadcast: false` on every ordinary video.
        let mut object = live_object(1, true, false);
        object["isLiveBroadcast"] = Value::Bool(false);
        assert!(
            object_primary_media(&object, caps).is_none(),
            "no files and not a live means no attachment, as before"
        );
        // An object that omits the field entirely is likewise not a live.
        let mut bare = live_object(1, true, false);
        bare.as_object_mut().unwrap().remove("isLiveBroadcast");
        assert!(object_primary_media(&bare, caps).is_none());
    }

    #[test]
    fn a_peer_that_omits_the_state_is_read_from_its_playlist() {
        // `state` is PeerTube-specific; a peer that publishes the live flag
        // without it should still play rather than render a dead placeholder.
        let mut object = live_object(1, true, false);
        object.as_object_mut().unwrap().remove("state");
        assert_eq!(live_info(&object).map(|l| l.state), Some(LiveState::Live));
        let mut no_master = live_object(4, false, false);
        no_master.as_object_mut().unwrap().remove("state");
        assert_eq!(
            live_info(&no_master).map(|l| l.state),
            Some(LiveState::Waiting)
        );
    }

    /// The progressive-`url` compatibility ladder (issue-1 convergence): a
    /// quality-blind client always gets the SAME rendition — prefer 480p, else
    /// the tallest rung below it, else the shortest rung above it.
    #[test]
    fn progressive_url_prefers_480p_then_ladders() {
        // height (0 = audio, partitioned out for HLS) -> url. `mk` builds a HLS
        // Video whose master `tag` array is the ladder.
        let mk = |heights: &[i64]| {
            let tag: Vec<Value> = heights
                .iter()
                .map(|h| {
                    serde_json::json!({"type": "Link", "mediaType": "video/mp4", "height": h,
                           "href": format!("https://origin.example/v-{h}.mp4"),
                           "size": 10_000_000})
                })
                .collect();
            serde_json::json!({
                "type": "Video",
                "id": "https://origin.example/videos/watch/x",
                "url": [{
                    "type": "Link", "mediaType": "application/x-mpegURL",
                    "href": "https://origin.example/master.m3u8", "tag": tag,
                }],
            })
        };
        let caps = MediaCaps {
            max_height: 1080,
            budget_bytes: 2048 * 1024 * 1024,
        };
        let pick = |heights: &[i64]| {
            object_primary_media(&mk(heights), caps)
                .unwrap()
                .url
                .to_owned()
        };

        // Exact 480p wins over everything.
        assert_eq!(
            pick(&[1080, 720, 480, 360, 144, 0]),
            "https://origin.example/v-480.mp4"
        );
        // No 480: the tallest rung *below* 480.
        assert_eq!(
            pick(&[1080, 360, 240, 0]),
            "https://origin.example/v-360.mp4"
        );
        // Nothing at/below 480: the shortest rung *above* it (not the tallest).
        assert_eq!(pick(&[1080, 720, 0]), "https://origin.example/v-720.mp4");
        // A single rung is picked as-is.
        assert_eq!(pick(&[720]), "https://origin.example/v-720.mp4");
    }

    #[test]
    fn progressive_url_prefers_explicitly_muxed_web_video() {
        let stream = |value: &str| {
            serde_json::json!({
                "type": "PropertyValue",
                "name": "ffprobe_codec_type",
                "value": value,
            })
        };
        let format = |value: &str| {
            serde_json::json!({
                "type": "PropertyValue",
                "name": "peertube_format_flag",
                "value": value,
            })
        };
        let object = serde_json::json!({
            "type": "Video",
            "id": "https://origin.example/videos/watch/muxed",
            "url": [
                {
                    "type": "Link", "mediaType": "video/mp4", "height": 480,
                    "href": "https://origin.example/web-480.mp4", "size": 30_000_000,
                    "attachment": [stream("audio"), stream("video"), format("web-video")],
                },
                {
                    "type": "Link", "mediaType": "application/x-mpegURL",
                    "href": "https://origin.example/hls/master.m3u8",
                    "tag": [
                        {
                            "type": "Link", "mediaType": "video/mp4", "height": 480,
                            "href": "https://origin.example/hls/video-480.mp4", "size": 20_000_000,
                            "attachment": [stream("video"), format("fragmented")],
                        },
                        {
                            "type": "Link", "mediaType": "video/mp4", "height": 0,
                            "href": "https://origin.example/hls/audio.mp4", "size": 5_000_000,
                            "attachment": [stream("audio"), format("fragmented")],
                        }
                    ]
                }
            ]
        });
        let primary = object_primary_media(
            &object,
            MediaCaps {
                max_height: 1080,
                budget_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
        .unwrap();
        assert_eq!(primary.url, "https://origin.example/web-480.mp4");
        assert_eq!(primary.audio_url, None, "muxed source needs no companion");
        assert_eq!(
            primary.renditions.len(),
            2,
            "the HLS ladder excludes the top-level WebVideo"
        );
    }

    #[test]
    fn top_level_web_video_without_codec_hints_does_not_borrow_hls_audio() {
        let object = serde_json::json!({"type": "Video"});
        let top_level = MediaLink {
            url: "https://origin.example/web-480.mp4",
            media_type: "video/mp4",
            width: Some(854),
            height: Some(480),
            size: 30_000_000,
            fps: Some(30),
            has_audio: None,
            has_video: None,
            in_hls: false,
            web_video: true,
        };
        let split_video = MediaLink {
            url: "https://origin.example/hls/video-480-fragmented.mp4",
            media_type: "video/mp4",
            width: Some(854),
            height: Some(480),
            size: 20_000_000,
            fps: Some(30),
            has_audio: Some(false),
            has_video: Some(true),
            in_hls: true,
            web_video: false,
        };
        let split_audio = MediaLink {
            url: "https://origin.example/hls/audio-fragmented.mp4",
            media_type: "video/mp4",
            width: None,
            height: Some(0),
            size: 5_000_000,
            fps: None,
            has_audio: Some(true),
            has_video: Some(false),
            in_hls: true,
            web_video: false,
        };
        let renditions = [top_level, split_video];
        let primary = pick_primary(
            &object,
            "video/",
            &renditions,
            Some(split_audio),
            MediaCaps {
                max_height: 1080,
                budget_bytes: 2 * 1024 * 1024 * 1024,
            },
            Some("https://origin.example/hls/master.m3u8"),
        )
        .unwrap();
        assert_eq!(primary.url, "https://origin.example/web-480.mp4");
        assert_eq!(primary.audio_url, None);
    }

    /// A non-HLS multi-rendition Video (no master, no audio companion) still
    /// takes the 480p ladder AND still defers its download — picking the light
    /// rung must not flip a never-watched video to an eager ingest fetch.
    #[test]
    fn non_hls_multi_rendition_ladders_and_defers() {
        let object = serde_json::json!({
            "type": "Video",
            "id": "https://origin.example/videos/watch/y",
            "url": [
                {"type": "Link", "mediaType": "video/mp4", "height": 720,
                 "href": "https://origin.example/p-720.mp4", "size": 40_000_000},
                {"type": "Link", "mediaType": "video/mp4", "height": 480,
                 "href": "https://origin.example/p-480.mp4", "size": 20_000_000},
                {"type": "Link", "mediaType": "video/mp4", "height": 360,
                 "href": "https://origin.example/p-360.mp4", "size": 10_000_000},
            ],
        });
        let caps = MediaCaps {
            max_height: 1080,
            budget_bytes: 2048 * 1024 * 1024,
        };
        let primary = object_primary_media(&object, caps).unwrap();
        assert_eq!(primary.url, "https://origin.example/p-480.mp4");
        assert!(
            primary.defer_download,
            "a downscaled multi-rendition video defers even under EAGER_MEDIA_LIMIT"
        );
        assert!(primary.renditions.is_empty(), "non-HLS keeps no ladder");
    }

    /// Undeclared-height progressive files (no `height` field) resolve to the
    /// lightest by byte size rather than silently flipping to the largest.
    #[test]
    fn undeclared_height_picks_lightest() {
        let object = serde_json::json!({
            "type": "Video",
            "id": "https://origin.example/videos/watch/z",
            "url": [
                {"type": "Link", "mediaType": "video/mp4",
                 "href": "https://origin.example/big.mp4", "size": 90_000_000},
                {"type": "Link", "mediaType": "video/mp4",
                 "href": "https://origin.example/small.mp4", "size": 30_000_000},
            ],
        });
        let caps = MediaCaps {
            max_height: 1080,
            budget_bytes: 2048 * 1024 * 1024,
        };
        let primary = object_primary_media(&object, caps).unwrap();
        assert_eq!(primary.url, "https://origin.example/small.mp4");
        assert!(primary.defer_download, "multi-rendition defers");
    }
}
