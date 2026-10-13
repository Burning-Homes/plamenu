//! Native Audio uses the ordinary status ACL, outbox transaction and media gate.
use crate::error::ApiError;
use plamenu_db::media::Media;

pub(crate) fn validate(media: &[Media], title: Option<&str>) -> Result<(), ApiError> {
    if title.is_none_or(|t| t.trim().is_empty()) {
        return Err(ApiError::Unprocessable("Audio needs a title".into()));
    }
    if media.len() != 1 || !media[0].content_type.starts_with("audio/") {
        return Err(ApiError::Unprocessable(
            "Audio needs exactly one audio attachment".into(),
        ));
    }
    if media[0].processing != "complete" {
        return Err(ApiError::Unprocessable(
            "Wait for audio processing to finish".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_edit(
    status: &plamenu_db::status::Status,
    media: &[Media],
    title: Option<&str>,
) -> Result<(), ApiError> {
    if status.object_type.as_deref() == Some("Audio") {
        validate(media, title)?;
    }
    Ok(())
}

pub(crate) fn validate_edit_metadata(
    status: &plamenu_db::status::Status,
    metadata: Option<plamenu_db::music::Metadata>,
) -> Result<Option<plamenu_db::music::Metadata>, ApiError> {
    if let Some(metadata) = &metadata {
        if status.object_type.as_deref() != Some("Audio") {
            return Err(ApiError::Unprocessable(
                "Music metadata requires an Audio post".into(),
            ));
        }
        validate_metadata(metadata)?;
    }
    Ok(metadata)
}

pub(crate) fn validate_metadata(m: &plamenu_db::music::Metadata) -> Result<(), ApiError> {
    if [&m.artist, &m.album, &m.genre]
        .iter()
        .any(|s| s.chars().count() > 200 || s.contains('\0'))
        || m.release_year.is_some_and(|v| !(1..=9999).contains(&v))
        || [m.track_number, m.disc_number]
            .into_iter()
            .flatten()
            .any(|v| !(1..=9999).contains(&v))
        || m.musicbrainz_id.as_ref().is_some_and(|id| !valid_mbid(id))
    {
        return Err(ApiError::Unprocessable("Invalid music metadata".into()));
    }
    Ok(())
}

/// Funkwhale-compatible metadata, embedded without dereferencing untrusted links.
pub(crate) fn enrich(
    object: &mut serde_json::Value,
    metadata: Option<&plamenu_db::music::Metadata>,
) {
    use serde_json::json;
    let Some(metadata) = metadata else {
        return;
    };
    let mut context = plamenu_ap::activity::quote_context();
    if let Some(entries) = context.as_array_mut() {
        entries.push(json!("https://funkwhale.audio/ns"));
    }
    object["@context"] = context;
    let attachment = object["attachment"][0].clone();
    if let (Some(media_url), Some(mime)) =
        (attachment["url"].as_str(), attachment["mediaType"].as_str())
    {
        object["url"] = json!([
            {"type":"Link", "mediaType":"text/html", "href":object["url"]},
            {"type":"Link", "mediaType":mime, "href":media_url},
        ]);
        object["mediaType"] = json!(mime);
        if let Some(duration) = attachment["duration"]
            .as_str()
            .and_then(|s| s.strip_prefix("PT"))
            .and_then(|s| s.strip_suffix('S'))
            .and_then(|s| s.parse::<f64>().ok())
        {
            object["duration"] = json!(duration);
        }
    }
    let id = object["id"].as_str().unwrap_or_default().to_owned();
    let author = object["attributedTo"].clone();
    let artist = json!({"type":"Artist", "id":format!("{id}/artist"), "name":metadata.artist, "attributedTo":author,"published":object["published"],"musicbrainzId":null,"tag":[],"image":null});
    let album = json!({"type":"Album", "id":format!("{id}/album"), "name":if metadata.album.is_empty(){object["name"].as_str().unwrap_or_default()}else{&metadata.album}, "artists":[artist], "attributedTo":author,"published":object["published"],"musicbrainzId":null,"tag":[],"image":null,"released":metadata.release_year.map(|y|format!("{y:04}-01-01"))});
    if object["to"].as_array().is_some_and(|to| {
        to.iter()
            .any(|v| v.as_str() == Some(plamenu_ap::activity::PUBLIC))
    }) {
        object["library"] = json!(format!("{}/library", author.as_str().unwrap_or_default()));
    }
    object["track"] = json!({"type":"Track","id":format!("{id}/track"),"name":object["name"],"attributedTo":author,"artists":[artist],"album":album,"published":object["published"],"position":metadata.track_number,"disc":metadata.disc_number,"musicbrainzId":metadata.musicbrainz_id,"tag":[],"image":null,"license":null,"copyright":null});
}

pub(crate) async fn ingest(
    state: &crate::AppState,
    status: &plamenu_db::status::Status,
    object: &serde_json::Value,
) -> Result<(), ApiError> {
    if status.object_type.as_deref() != Some("Audio") {
        return Ok(());
    }
    let track = &object["track"];
    let text = |v: &serde_json::Value| {
        v.as_str()
            .unwrap_or_default()
            .chars()
            .filter(|c| *c != '\0')
            .take(200)
            .collect::<String>()
    };
    let positive = |v: &serde_json::Value| {
        v.as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| (1..=9999).contains(n))
    };
    let metadata = plamenu_db::music::Metadata {
        artist: text(&track["artists"][0]["name"]),
        album: text(&track["album"]["name"]),
        genre: String::new(),
        release_year: track["album"]["released"]
            .as_str()
            .and_then(|s| s.get(..4))
            .and_then(|s| s.parse().ok())
            .filter(|n| (1..=9999).contains(n)),
        track_number: positive(&track["position"]),
        disc_number: positive(&track["disc"]),
        musicbrainz_id: track["musicbrainzId"]
            .as_str()
            .filter(|s| valid_mbid(s))
            .map(str::to_owned),
    };
    plamenu_db::music::store(&state.pool, status.id, &metadata).await?;
    Ok(())
}

pub struct Catalogue {
    pub items: Vec<plamenu_db::status::Status>,
    pub before: Option<i64>,
}

/// Scan at most sixty records, then apply the ordinary visibility/moderation
/// gate. The cursor advances over filtered rows so empty pages cannot stall.
pub async fn catalogue(
    state: &crate::AppState,
    viewer: Option<i64>,
    mine: bool,
    query: &str,
    before: Option<i64>,
) -> Result<Catalogue, ApiError> {
    if mine && viewer.is_none() {
        return Err(ApiError::Unauthorized("Sign in to see your audio".into()));
    }
    if query.chars().count() > 200 || query.contains('\0') {
        return Err(ApiError::BadRequest("Invalid music search".into()));
    }
    let ids: Vec<i64> = sqlx::query_scalar("SELECT s.id FROM statuses s JOIN accounts a ON a.id=s.account_id LEFT JOIN status_music m ON m.status_id=s.id WHERE s.object_type='Audio' AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND (($2 AND s.account_id=$1) OR (NOT $2 AND s.visibility='public' AND s.ingest_provenance <> 'history')) AND a.suspended_at IS NULL AND ($2 OR a.silenced_at IS NULL) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$1 AND b.target_account_id=s.account_id) OR (b.account_id=s.account_id AND b.target_account_id=$1)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$1 AND m.target_account_id=s.account_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND NOT EXISTS(SELECT 1 FROM account_domain_blocks d WHERE d.account_id=$1 AND d.domain=a.domain) AND ($4::bigint IS NULL OR s.id<$4) AND ($3='' OR position(lower($3) in lower(concat_ws(' ',s.title,m.artist,m.album,m.genre)))>0) ORDER BY s.id DESC LIMIT 61\n-- STUBFILTER: excludes soft-deleted Audio publications")
        .bind(viewer).bind(mine).bind(query.trim()).bind(before).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let next = (ids.len() == 61).then(|| ids[59]);
    let selected = &ids[..ids.len().min(60)];
    let rows = plamenu_db::status::find_by_ids(&state.pool, selected).await?;
    let mut items = crate::entities::filter_viewable(&state.pool, &rows, viewer).await?;
    items.sort_unstable_by_key(|s| std::cmp::Reverse(s.id));
    Ok(Catalogue {
        items,
        before: next,
    })
}

pub(crate) fn valid_mbid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
