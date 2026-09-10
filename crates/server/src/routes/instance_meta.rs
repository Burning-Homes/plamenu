//! Public discovery endpoints: the instance-meta family
//! (`/api/v1/instance/{peers,activity,domain_blocks,languages,
//! translation_languages}`), `/api/v1/peers/search` and `GET /api/oembed`.
//! Each mirrors its Mastodon controller, including the 404s when the
//! operator setting turns the surface off and when federation is
//! allow-list based (Mastodon's limited federation mode).

use axum::Json;
use axum::extract::{Query, State};
use plamenu_db::instance_settings::DomainBlocksDisclosure;
use plamenu_db::{account, discovery, status};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::auth::MaybeUser;
use crate::entities::can_view;
use crate::error::ApiError;

/// `GET /api/v1/instance/peers` — every known federating domain. 404 when
/// the peers API is disabled or federation is allow-list based.
pub async fn peers(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    require_peers_api(&state).await?;
    let domains = discovery::peer_domains(&state.pool).await?;
    Ok(Json(json!(domains)))
}

#[derive(Debug, Default, Deserialize)]
pub struct PeersSearchQuery {
    q: Option<String>,
}

/// `GET /api/v1/peers/search?q=` — known domains matching a prefix, most
/// populated first. Mastodon renders `null` (not `[]`) for a blank query.
pub async fn peers_search(
    State(state): State<AppState>,
    Query(query): Query<PeersSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    require_peers_api(&state).await?;
    let prefix = query.q.as_deref().unwrap_or_default().trim().to_lowercase();
    if prefix.is_empty() {
        return Ok(Json(Value::Null));
    }
    let domains = discovery::peer_domains_matching(&state.pool, &prefix, 10).await?;
    Ok(Json(json!(domains)))
}

async fn require_peers_api(state: &AppState) -> Result<(), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.peers_api_enabled || discovery::allow_list_active(&state.pool).await? {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

/// `GET /api/v1/instance/activity` — 12 rolling weeks of statuses/logins/
/// registrations, newest first, every value a string (Mastodon quirk).
pub async fn activity(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.activity_api_enabled || discovery::allow_list_active(&state.pool).await? {
        return Err(ApiError::NotFound);
    }
    let weeks = discovery::activity_weeks(&state.pool).await?;
    let entities: Vec<Value> = weeks
        .iter()
        .map(|week| {
            json!({
                "week": week.week_start.unix_timestamp().to_string(),
                "statuses": week.statuses.to_string(),
                "logins": week.logins.to_string(),
                "registrations": week.registrations.to_string(),
            })
        })
        .collect();
    Ok(Json(json!(entities)))
}

/// `GET /api/v1/instance/domain_blocks` — the silenced/suspended domains,
/// disclosed per the `show_domain_blocks` audience setting; each entry's
/// rationale (`comment`) per `show_domain_blocks_rationale`.
pub async fn domain_blocks(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
) -> Result<Json<Value>, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let signed_in = viewer.is_some();
    if !disclosed_to(settings.show_domain_blocks(), signed_in) {
        return Err(ApiError::NotFound);
    }
    let with_comment = disclosed_to(settings.show_domain_blocks_rationale(), signed_in);
    let blocks = discovery::user_facing_domain_blocks(&state.pool).await?;
    let entities: Vec<Value> = blocks
        .iter()
        .map(|block| {
            json!({
                "domain": public_domain(&block.domain, block.obfuscate),
                "digest": crate::followers_sync::hex(&Sha256::digest(&block.domain).into()),
                "severity": block.severity,
                "comment": if with_comment {
                    json!(block.public_comment.as_deref().unwrap_or_default())
                } else {
                    Value::Null
                },
            })
        })
        .collect();
    Ok(Json(json!(entities)))
}

fn disclosed_to(disclosure: DomainBlocksDisclosure, signed_in: bool) -> bool {
    match disclosure {
        DomainBlocksDisclosure::All => true,
        DomainBlocksDisclosure::Users => signed_in,
        DomainBlocksDisclosure::Disabled => false,
    }
}

/// Mastodon's `DomainBlock#public_domain`: with `obfuscate` set, the middle
/// half of the domain is starred out (dots stay visible).
fn public_domain(domain: &str, obfuscate: bool) -> String {
    if !obfuscate {
        return domain.to_owned();
    }
    let length = domain.chars().count();
    let visible = length / 4;
    domain
        .chars()
        .enumerate()
        .map(|(i, chr)| {
            if i > visible && i < length - visible && chr != '.' {
                '*'
            } else {
                chr
            }
        })
        .collect()
}

/// `GET /api/v1/instance/languages` — every selectable posting locale with
/// its English name, matching Mastodon's supported-locale inventory.
pub async fn languages() -> Json<Value> {
    let mut languages: Vec<&'static crate::languages::Language> =
        crate::languages::LANGUAGES.iter().collect();
    languages.sort_unstable_by_key(|language| language.code);
    let entities: Vec<Value> = languages
        .iter()
        .map(|language| json!({ "code": language.code, "name": language.english }))
        .collect();
    Json(json!(entities))
}

/// `GET /api/v1/instance/translation_languages` — the source→target map of
/// the configured translation backend (M25); `{}` when none is configured.
pub async fn translation_languages(State(state): State<AppState>) -> Json<Value> {
    Json(crate::translation::languages_json(&state).await)
}

#[derive(Debug, Default, Deserialize)]
pub struct OembedQuery {
    url: Option<String>,
    maxwidth: Option<String>,
    maxheight: Option<String>,
}

/// `GET /api/oembed?url=` — the oEmbed provider endpoint for local status
/// URLs (both the `/@name/{id}` web form and the `/users/name/statuses/{id}`
/// AP form). Anonymous; anything but a public or unlisted local status is a
/// 404, like Mastodon's `StatusFinder` + `require_public_status!`.
pub async fn oembed(
    State(state): State<AppState>,
    Query(query): Query<OembedQuery>,
) -> Result<Json<Value>, ApiError> {
    let url = query.url.as_deref().unwrap_or_default().trim();
    let status_id = parse_status_url(&state.config.domain, url).ok_or(ApiError::NotFound)?;
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.uri.is_some() || !can_view(&state.pool, &item, None).await? {
        return Err(ApiError::NotFound);
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let width = query
        .maxwidth
        .as_deref()
        .and_then(|value| value.parse().ok())
        .unwrap_or(400);
    let height = query
        .maxheight
        .as_deref()
        .and_then(|value| value.parse().ok());
    Ok(Json(super::web_api::oembed_entity(
        &state.config.domain,
        &item,
        &author,
        width,
        height,
    )))
}

/// The status id from a local status URL, in either the human web form
/// (`https://{domain}/@{name}/{id}`) or the AP id form.
fn parse_status_url(domain: &str, url: &str) -> Option<i64> {
    if let Some((_, id)) = plamenu_ap::urls::parse_local_numeric_status_url(domain, url) {
        return Some(id);
    }
    if let Some((_, id)) = plamenu_ap::urls::parse_local_status_url(domain, url) {
        return Some(id);
    }
    let path = url.strip_prefix("https://")?.strip_prefix(domain)?;
    let rest = path.strip_prefix("/@")?;
    let (username, id_part) = rest.split_once('/')?;
    if username.is_empty() || id_part.contains(['/', '?', '#']) {
        return None;
    }
    id_part.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_status_url, public_domain};

    #[test]
    fn obfuscation_stars_the_middle_and_keeps_dots() {
        assert_eq!(public_domain("example.com", false), "example.com");
        // Mastodon's exact algorithm: length 11, visible ratio 2 — indexes
        // 3..=8 starred unless they are dots.
        assert_eq!(public_domain("example.com", true), "exa****.*om");
    }

    #[test]
    fn parses_both_local_status_url_forms() {
        assert_eq!(
            parse_status_url("p.local", "https://p.local/@alice/42"),
            Some(42)
        );
        assert_eq!(
            parse_status_url("p.local", "https://p.local/users/alice/statuses/42"),
            Some(42)
        );
        assert_eq!(parse_status_url("p.local", "https://p.local/@alice"), None);
        assert_eq!(
            parse_status_url("p.local", "https://other.example/@alice/42"),
            None
        );
        assert_eq!(
            parse_status_url("p.local", "https://p.local/@alice/42/extra"),
            None
        );
    }
}
