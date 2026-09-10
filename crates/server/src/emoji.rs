//! Custom emoji: ingesting `Emoji` tag entries from remote notes and
//! actors, and rendering emoji back out — as Mastodon `CustomEmoji` REST
//! entities and as `Emoji` tag objects on outbound notes and actor
//! documents.

use plamenu_ap::actor::Image;
use plamenu_ap::emoji::{
    EmojiTag, MAX_FEDERATED_SHORTCODE_LEN, emoji_url, is_valid_shortcode, scan_shortcodes,
};
use plamenu_db::custom_emoji::{self, CustomEmoji, RemoteEmojiData};
use plamenu_db::{DbError, PgPool};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::ApiError;

/// Records the `Emoji` entries of an inbound `tag` array under the sender's
/// domain — the per-tag rules are Mastodon's `process_emoji`: a usable
/// shortcode and an `icon.url` are required, anything malformed is skipped.
pub async fn ingest_remote_emoji_tags(
    pool: &PgPool,
    local_domain: Option<&str>,
    domain: &str,
    entries: &[Value],
) -> Result<(), DbError> {
    // A `reject_media` domain block skips emoji registration entirely, like
    // Mastodon's `process_emoji` — their images would otherwise be hotlinked.
    if !entries.is_empty()
        && plamenu_db::instance_policy::domain_rejects_media(pool, domain).await?
    {
        return Ok(());
    }
    for entry in entries.iter().take(20) {
        if entry.get("type").and_then(Value::as_str) != Some("Emoji") {
            continue;
        }
        let Some(shortcode) = entry
            .get("name")
            .and_then(Value::as_str)
            .map(|name| name.trim_matches(':'))
            .filter(|code| is_valid_shortcode(code, MAX_FEDERATED_SHORTCODE_LEN))
        else {
            continue;
        };
        let Some(image_url) = entry
            .get("icon")
            .and_then(plamenu_ap::actor::image_url)
            .filter(|url| plamenu_federation::is_federation_url(url))
        else {
            continue;
        };
        // Some Pleroma-family peers echo our own Emoji tag back when one of
        // their users joins a reaction. It still denotes our local origin;
        // registering it under the sender's domain would split one reaction
        // into two stable-origin groups.
        if local_domain
            .and_then(|local| local_media_file_name(local, image_url))
            .is_some()
        {
            continue;
        }
        let updated = entry
            .get("updated")
            .and_then(Value::as_str)
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
        custom_emoji::upsert_remote(
            pool,
            RemoteEmojiData {
                shortcode,
                domain,
                uri: entry.get("id").and_then(Value::as_str),
                image_remote_url: image_url,
                updated,
            },
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn local_media_file_name<'a>(domain: &str, url: &'a str) -> Option<&'a str> {
    let prefix = format!("https://{domain}/media/");
    url.strip_prefix(&prefix)
        .filter(|file| !file.is_empty() && !file.contains(['/', '?', '#']))
}

/// The image URL of an emoji for **federation / AP** output: our cached copy
/// (local emoji always have one), else the remote origin. Server-to-server, so
/// a real fetchable URL is correct here — unlike the client-facing serializer.
fn image_url(domain: &str, emoji: &CustomEmoji) -> String {
    match (&emoji.image_remote_url, &emoji.image_file_name) {
        (_, Some(file)) => format!("https://{domain}/media/{file}"),
        (Some(remote), None) => remote.clone(),
        (None, None) => String::new(),
    }
}

/// The image URL of an emoji for a **client**: our cached copy, else the media
/// proxy (never the origin), so a remote emoji's image is fetched through the
/// instance. `allow_direct` lets the proxy fall back to the origin as a last
/// resort for a viewer who opted in. Empty when the emoji has no image at all.
#[must_use]
pub fn client_image_url(domain: &str, emoji: &CustomEmoji, allow_direct: bool) -> String {
    if let Some(file) = &emoji.image_file_name {
        format!("https://{domain}/media/{file}")
    } else if emoji.image_remote_url.is_some() {
        crate::entities::media_proxy_url(domain, "emoji", emoji.id, false, allow_direct)
    } else {
        String::new()
    }
}

/// The Mastodon `CustomEmoji` REST entity. The `category` key only exists
/// on categorized emoji, like Mastodon's serializer (`if: :category_loaded?`).
#[must_use]
pub fn custom_emoji_json(domain: &str, emoji: &CustomEmoji, allow_direct: bool) -> Value {
    let url = client_image_url(domain, emoji, allow_direct);
    let mut entity = json!({
        "shortcode": emoji.shortcode,
        "url": url,
        // No static (de-animated) variants yet; Mastodon clients fall back
        // to `url` semantics either way.
        "static_url": url,
        "visible_in_picker": emoji.visible_in_picker,
    });
    if let Some(category) = &emoji.category {
        entity["category"] = json!(category);
    }
    entity
}

/// `CustomEmoji` entities for every emoji of `author_domain` (`None` =
/// local) referenced as `:shortcode:` in the given texts — Mastodon's
/// `CustomEmoji.from_text` joined over the entity's emojifiable fields.
pub async fn emojis_json(
    pool: &PgPool,
    domain: &str,
    author_domain: Option<&str>,
    texts: &[&str],
    allow_direct: bool,
) -> Result<Vec<Value>, ApiError> {
    let found = referenced_emoji(pool, author_domain, texts).await?;
    Ok(found
        .iter()
        .map(|emoji| custom_emoji_json(domain, emoji, allow_direct))
        .collect())
}

/// [`emojis_json`] for a local author, resolving their personal collection
/// before the instance-wide catalog.
pub async fn emojis_json_for_account(
    pool: &PgPool,
    domain: &str,
    account_id: i64,
    texts: &[&str],
    allow_direct: bool,
) -> Result<Vec<Value>, ApiError> {
    let found =
        custom_emoji::lookup_local_for_account(pool, account_id, &shortcodes_of(texts)).await?;
    Ok(found
        .iter()
        .map(|emoji| custom_emoji_json(domain, emoji, allow_direct))
        .collect())
}

/// `Emoji` tag objects for local emoji referenced in the given texts — what
/// outbound Notes and actor documents carry.
pub async fn emoji_tags_for_text(
    pool: &PgPool,
    domain: &str,
    texts: &[&str],
) -> Result<Vec<Value>, ApiError> {
    let found = referenced_emoji(pool, None, texts).await?;
    emoji_tags(domain, &found)
}

/// Outbound `Emoji` tags for a local author, with personal emoji shadowing
/// the same shortcode in the server-wide catalog.
pub async fn emoji_tags_for_text_for_account<'e, E: plamenu_db::PgExecutor<'e>>(
    pool: E,
    domain: &str,
    account_id: i64,
    texts: &[&str],
) -> Result<Vec<Value>, ApiError> {
    let found =
        custom_emoji::lookup_local_for_account(pool, account_id, &shortcodes_of(texts)).await?;
    emoji_tags(domain, &found)
}

/// [`emoji_tags_for_text`] against a preloaded emoji set — the batched form
/// for page builders that looked up every shortcode on the page in one query
/// ([`shortcodes_of`] over the whole page, one `custom_emoji::lookup`) and now
/// need each row's own subset. Selecting from the loaded set rather than
/// querying again keeps the per-row order the single-status form produces:
/// the order the texts mentioned the shortcodes in.
pub fn emoji_tags_from(
    domain: &str,
    loaded: &[CustomEmoji],
    texts: &[&str],
) -> Result<Vec<Value>, ApiError> {
    let referenced: Vec<CustomEmoji> = shortcodes_of(texts)
        .into_iter()
        .filter_map(|code| loaded.iter().find(|e| e.shortcode == code).cloned())
        .collect();
    emoji_tags(domain, &referenced)
}

/// The `Emoji` tag objects for already-resolved emoji rows.
fn emoji_tags(domain: &str, found: &[CustomEmoji]) -> Result<Vec<Value>, ApiError> {
    found
        .iter()
        .map(|emoji| {
            let updated = emoji
                .updated_at
                .format(&Rfc3339)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            let url = image_url(domain, emoji);
            let media_type = emoji
                .image_content_type
                .clone()
                .unwrap_or_else(|| "image/png".to_owned());
            Ok(EmojiTag::new(
                emoji_url(domain, emoji.id),
                &emoji.shortcode,
                updated,
                Image::new(url, &media_type),
            )
            .into_value())
        })
        .collect()
}

/// The distinct `:shortcode:` references in the given texts, in the order the
/// texts mention them — the lookup key both emoji paths are keyed on.
#[must_use]
pub fn shortcodes_of(texts: &[&str]) -> Vec<String> {
    let mut shortcodes: Vec<String> = Vec::new();
    for text in texts {
        for code in scan_shortcodes(text) {
            if !shortcodes.iter().any(|c| c == code) {
                shortcodes.push(code.to_owned());
            }
        }
    }
    shortcodes
}

/// The enabled emoji rows of one domain referenced in the given texts.
async fn referenced_emoji(
    pool: &PgPool,
    author_domain: Option<&str>,
    texts: &[&str],
) -> Result<Vec<CustomEmoji>, DbError> {
    custom_emoji::lookup(pool, &shortcodes_of(texts), author_domain).await
}

/// The `ActivityPub` `Emoji` object served at `/emojis/{id}`, Mastodon's
/// shape: the tag entry plus an `@context` carrying the `toot:Emoji` term.
pub fn ap_emoji_object(domain: &str, emoji: &CustomEmoji) -> Result<Value, ApiError> {
    let updated = emoji
        .updated_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let url = image_url(domain, emoji);
    let media_type = emoji
        .image_content_type
        .clone()
        .unwrap_or_else(|| "image/png".to_owned());
    let mut object = EmojiTag::new(
        emoji_url(domain, emoji.id),
        &emoji.shortcode,
        updated,
        Image::new(url, &media_type),
    )
    .into_value();
    object["@context"] = json!([
        plamenu_ap::AS_CONTEXT,
        { "toot": "http://joinmastodon.org/ns#", "Emoji": "toot:Emoji" },
    ]);
    Ok(object)
}
