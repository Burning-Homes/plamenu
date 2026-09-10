//! Builders for the per-page head metadata (`layout::PageMeta`).
//!
//! Public pages — profiles, threads, collections, the instance-level entry
//! points — are shared into other platforms and crawled by preview scrapers,
//! so each builds the Open Graph block Mastodon renders for the same page
//! (`accounts/_og`, `statuses/show`, `shared/_og`). The descriptions mirror
//! Mastodon's `account_description`/`status_description` helpers so previews
//! of a Plamenu link read like previews of a Mastodon link.

use fluent_bundle::FluentArgs;
use plamenu_db::instance_settings;
use serde_json::Value;

use super::i18n::Locale;
use super::layout::{MetaImage, PageMeta};
use super::view;
use crate::AppState;
use crate::error::ApiError;
use crate::filters::plain_text;

/// The operator's site title, for `og:site_name` on subject pages.
pub async fn site_name(state: &AppState) -> Result<String, ApiError> {
    Ok(instance_settings::get(&state.pool).await?.site_title)
}

/// The full `user@domain` handle: entity `acct` values omit the domain for
/// local accounts, but `og:title`/`profile:username` always carry it.
fn full_handle(account: &view::Account, local_domain: &str) -> String {
    let acct = account.acct();
    if acct.contains('@') {
        acct.to_owned()
    } else {
        format!("{acct}@{local_domain}")
    }
}

/// `Display Name (@user@domain)` — Mastodon's `og:title` for profiles and
/// statuses alike. A group carries its `!` community sigil, matching the handle
/// shown on the page it previews.
fn subject_title(account: &view::Account, local_domain: &str) -> String {
    format!(
        "{} ({}{})",
        account.name(),
        account.handle_prefix(),
        full_handle(account, local_domain)
    )
}

/// Collapses the tag-boundary spaces `plain_text` leaves behind — meta
/// descriptions and titles are single-line strings.
fn flatten(html: &str) -> String {
    plain_text(html)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// The page metadata for a profile (`accounts/_og`): a `profile`-typed
/// preview with the avatar, the follower-count summary and the bio.
pub fn account_page(
    site_name: String,
    host_domain: &str,
    account_domain: &str,
    account: &view::Account,
    noindex: bool,
    locale: Locale,
) -> PageMeta {
    // The Atom feed is a local-account affordance only: a remote profile's
    // `acct` carries its `@domain`, and we don't syndicate mirrors.
    let feed_url = (!account.acct().contains('@'))
        .then(|| plamenu_ap::urls::account_atom_url(host_domain, account.acct()));
    PageMeta {
        og_title: Some(subject_title(account, account_domain)),
        og_kind: Some("profile"),
        description: Some(account_description(account, locale)),
        canonical: non_empty(account.url()),
        site_name: Some(site_name),
        image: Some(MetaImage::avatar(account.avatar())),
        profile_username: Some(full_handle(account, account_domain)),
        alternate: non_empty(account.uri()),
        feed_url,
        noindex,
        ..PageMeta::default()
    }
}

/// Mastodon's `account_description`: the post/follow counts, then the bio.
/// Meta descriptions leave the page (scrapers read the attribute verbatim),
/// so they format through [`Locale::plain_with`].
fn account_description(account: &view::Account, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("posts", account.statuses_count());
    args.set("following", account.following_count());
    args.set("followers", account.followers_count());
    let stats = locale.plain_with("meta-account-stats", &args);
    let note = flatten(account.note_html());
    if note.is_empty() {
        stats
    } else {
        format!("{stats} · {note}")
    }
}

/// The page metadata for a thread (`statuses/show`): an `article`-typed
/// preview of the focused status — its text (or content warning), its first
/// piece of media when showable, its language and publication time.
pub fn status_page(
    site_name: String,
    account_domain: &str,
    status: &view::Status,
    noindex: bool,
    locale: Locale,
) -> PageMeta {
    let author = status.account();
    let mut meta = PageMeta {
        og_title: Some(subject_title(&author, account_domain)),
        og_kind: Some("article"),
        description: Some(status_description(status, locale)),
        canonical: non_empty(status.url()),
        site_name: Some(site_name),
        published_time: non_empty(status.created_at()),
        locale: status.language().and_then(non_empty),
        profile_username: Some(full_handle(&author, account_domain)),
        alternate: non_empty(status.uri()),
        noindex,
        ..PageMeta::default()
    };
    // Sensitive media never leaks into previews (Mastodon's
    // `non_sensitive_with_media?` gate); the card falls back to the avatar.
    if !status.sensitive() {
        meta.image = status_image(status);
        meta.large_card = meta.image.is_some();
    }
    if meta.image.is_none() {
        meta.image = Some(MetaImage::avatar(author.avatar()));
    }
    meta
}

/// The thread page's `<title>`, Mastodon's `statuses.title`:
/// `Name: "the first fifty characters…"` — the quote marks belong to the
/// catalog, so a locale with its own quotation style supplies it.
pub fn status_page_title(status: &view::Status, locale: Locale) -> String {
    let spoiler = status.spoiler_text();
    let text = if spoiler.is_empty() {
        flatten(status.content_html())
    } else {
        spoiler.to_owned()
    };
    if text.is_empty() {
        // A media-only post: quoting nothing reads worse than no quote.
        return status.account().name().to_owned();
    }
    let mut quote: String = text.chars().take(50).collect();
    if quote.len() < text.len() {
        quote.push('…');
    }
    let author = status.account();
    let mut args = FluentArgs::new();
    args.set("name", author.name());
    args.set("quote", quote);
    locale.plain_with("meta-status-title", &args)
}

/// Mastodon's `status_description`: the attachment summary and content
/// warning on the first line; the text and poll options below, but only when
/// there is no content warning.
fn status_description(status: &view::Status, locale: Locale) -> String {
    let mut headline: Vec<String> = Vec::new();
    if let Some(summary) = media_summary(status.media(), locale) {
        headline.push(summary);
    }
    let spoiler = status.spoiler_text();
    if !spoiler.is_empty() {
        let mut args = FluentArgs::new();
        args.set("spoiler", spoiler);
        headline.push(locale.plain_with("meta-content-warning", &args));
    }
    let mut parts = vec![headline.join(" · ")];
    if spoiler.is_empty() {
        parts.push(flatten(status.content_html()));
        if let Some(options) = poll_summary(status) {
            parts.push(options);
        }
    }
    parts.retain(|part| !part.is_empty());
    parts.join("\n\n")
}

/// `Attached: 2 images · 1 video` — the attachment counts by kind. The list
/// is variable-length, so each kind is its own pluralized message and the
/// lead-in wraps the joined result.
fn media_summary(media: &[Value], locale: Locale) -> Option<String> {
    let (mut images, mut videos, mut audios) = (0, 0, 0);
    for item in media {
        match item.get("type").and_then(Value::as_str) {
            Some("image") => images += 1,
            Some("video" | "gifv") => videos += 1,
            Some("audio") => audios += 1,
            _ => {}
        }
    }
    let mut kinds = Vec::new();
    for (count, id) in [
        (images, "meta-media-images"),
        (videos, "meta-media-videos"),
        (audios, "meta-media-audios"),
    ] {
        if count > 0 {
            let mut args = FluentArgs::new();
            args.set("count", count);
            kinds.push(locale.plain_with(id, &args));
        }
    }
    (!kinds.is_empty()).then(|| {
        let mut args = FluentArgs::new();
        args.set("kinds", kinds.join(" · "));
        locale.plain_with("meta-attached", &args)
    })
}

/// The poll's options, one `[ ] choice` per line (Mastodon's `poll_summary`).
fn poll_summary(status: &view::Status) -> Option<String> {
    let options = status.poll()?.get("options")?.as_array()?;
    let lines: Vec<String> = options
        .iter()
        .filter_map(|option| option.get("title").and_then(Value::as_str))
        .map(|title| format!("[ ] {title}"))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The preview image for a status: the first image attachment at full size,
/// or the first video/gifv's poster frame (`statuses/_og_image`, minus the
/// player card). Audio-only posts fall back to the avatar like Mastodon.
fn status_image(status: &view::Status) -> Option<MetaImage> {
    let dimensions = |item: &Value, style: &str| {
        let geometry = item.get("meta")?.get(style)?;
        Some((
            geometry.get("width")?.as_i64()?,
            geometry.get("height")?.as_i64()?,
        ))
    };
    for item in status.media() {
        let (url, style) = match item.get("type").and_then(Value::as_str) {
            Some("image") => (item.get("url"), "original"),
            Some("video" | "gifv") => (item.get("preview_url"), "small"),
            _ => continue,
        };
        let Some(url) = url.and_then(Value::as_str).filter(|u| !u.is_empty()) else {
            continue;
        };
        let (width, height) = dimensions(item, style).unzip();
        return Some(MetaImage {
            url: url.to_owned(),
            width,
            height,
            alt: item
                .get("description")
                .and_then(Value::as_str)
                .and_then(non_empty),
        });
    }
    None
}

/// The page metadata for a collection page (`collections/show`): title and
/// description under the owner's site, always `noindex` like Mastodon, with
/// the `ActivityPub` collection as the alternate representation. A remote
/// collection points back at its origin URLs.
pub fn collection_page(
    site_name: String,
    local_domain: &str,
    owner: &plamenu_db::account::Account,
    collection: &plamenu_db::collection::Collection,
) -> PageMeta {
    let canonical = collection.url.clone().or_else(|| collection.uri.clone());
    let alternate = collection.uri.clone();
    PageMeta {
        og_title: Some(collection.name.clone()),
        og_kind: Some("website"),
        // A sensitive collection's description stays out of previews
        // (Mastodon's `unless @collection.sensitive?` guard).
        description: (!collection.sensitive)
            .then(|| flatten(&collection.description))
            .filter(|text| !text.is_empty()),
        canonical: canonical.or_else(|| {
            Some(plamenu_ap::urls::collection_web_url(
                local_domain,
                &owner.username,
                collection.id,
            ))
        }),
        site_name: Some(site_name),
        locale: collection.language.clone(),
        alternate: alternate.or_else(|| {
            Some(plamenu_ap::urls::collection_uri_for_actor(
                &crate::entities::account_uri(local_domain, owner),
                collection.id,
            ))
        }),
        // Always noindex, like Mastodon's collection pages.
        noindex: true,
        ..PageMeta::default()
    }
}

/// The page metadata for the instance-level entry points — the login page a
/// bare-domain share resolves to and the public explore timeline — mirroring
/// Mastodon's `shared/_og`: the site title and short description, with the
/// operator's server thumbnail as the preview image.
pub async fn instance_page(
    state: &AppState,
    path: &str,
    locale: Locale,
) -> Result<PageMeta, ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    let domain = &state.config.domain;
    let thumbnail = crate::routes::api::site_thumbnail(state).await?;
    let mut args = FluentArgs::new();
    args.set("domain", state.config.account_domain.as_str());
    Ok(PageMeta {
        og_title: Some(settings.site_title),
        og_kind: Some("website"),
        description: non_empty(&settings.site_short_description),
        canonical: Some(format!("https://{domain}{path}")),
        site_name: Some(locale.plain_with("meta-hosted-on", &args)),
        large_card: thumbnail.is_some(),
        image: thumbnail.map(|thumb| MetaImage {
            url: thumb.url_1x,
            width: None,
            height: None,
            alt: non_empty(&thumb.description),
        }),
        ..PageMeta::default()
    })
}
