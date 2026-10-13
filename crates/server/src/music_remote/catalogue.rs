//! Public Songhive catalogue API, separate from `ActivityPub` publication authority.
//! Unsigned anonymous reads never carry a member credential or bypass origin ACLs.
use super::{
    Resource,
    document::{self, Document, Kind},
};
use crate::{AppState, error::ApiError};
use plamenu_db::DbError;
use serde_json::Value;
use url::Url;

pub const PAGE_SIZE: usize = 24;
fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
fn key(value: &str) -> bool {
    crate::audio::valid_mbid(value)
}
fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(500)
        .collect()
}
pub fn origin(input: &str) -> Result<String, ApiError> {
    let url = document::url(input.trim())?;
    Ok(url.origin().ascii_serialization())
}
#[must_use]
pub fn track_key(input: &str) -> Option<String> {
    let url = document::url(input).ok()?;
    let tail = url.path().strip_prefix("/api/v1/tracks/")?;
    key(tail).then(|| tail.to_owned())
}
async fn json(state: &AppState, uri: &str) -> Result<Value, ApiError> {
    document::url(uri)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        return Err(ApiError::NotFound);
    }
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        state.federation.fetch_media_limited(uri, 2 * 1024 * 1024),
    )
    .await
    .map_err(|_| invalid("The music server did not respond in time"))?
    .map_err(|e| match e {
        plamenu_federation::FederationError::Status(401 | 403 | 404 | 410) => ApiError::NotFound,
        _ => invalid("The public music catalogue could not be reached"),
    })?;
    if origin(uri)? != origin(&fetched.final_url)?
        || fetched
            .content_type
            .split(';')
            .next()
            .is_none_or(|t| t.trim() != "application/json")
    {
        return Err(invalid(
            "The music server did not return its public catalogue",
        ));
    }
    serde_json::from_slice(&fetched.bytes)
        .map_err(|_| invalid("The music server returned an invalid catalogue"))
}
pub async fn identify(state: &AppState, viewer: i64, base: &str) -> Result<String, ApiError> {
    let info = json(state, &format!("{base}/api/v1/instance")).await?;
    if info["songhive_version"].as_str().is_none() {
        return Err(invalid(
            "This server does not expose a Songhive public catalogue. Open a published music resource URL instead.",
        ));
    }
    super::verified_controller(state, &format!("{base}/ap/actor"), viewer).await?;
    Ok(text(&info["title"]))
}
fn joined(base: &str, value: &str) -> Result<String, ApiError> {
    let url = Url::parse(base)
        .map_err(|_| ApiError::NotFound)?
        .join(value)
        .map_err(|_| ApiError::NotFound)?;
    document::url(url.as_str())?;
    if origin(url.as_str())? != base {
        return Err(invalid("The catalogue points outside its music server"));
    }
    Ok(url.into())
}
fn track(base: &str, value: &Value) -> Result<Document, ApiError> {
    let id = value["id"]
        .as_str()
        .filter(|id| key(id))
        .ok_or(ApiError::NotFound)?;
    if value["visibility"] != "public"
        || value["paid"].as_bool() == Some(true)
        || value["can_stream"].as_bool() == Some(false)
    {
        return Err(ApiError::NotFound);
    }
    let controller = value["owner"]["actor_url"]
        .as_str()
        .ok_or_else(|| invalid("The origin does not identify this track's owner"))?;
    let controller = joined(base, controller)?;
    let uri = format!("{base}/api/v1/tracks/{id}");
    let audio_url = value["audio_url"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|v| joined(base, v))
        .transpose()?;
    let image_url = value["image_url"]
        .as_str()
        .or_else(|| value["album"]["cover_url"].as_str())
        .or_else(|| value["artist"]["image_url"].as_str())
        .map(|v| joined(base, v))
        .transpose()?;
    let parent = value["album_id"]
        .as_str()
        .filter(|id| key(id))
        .map(|id| format!("{base}/albums/{id}"));
    Ok(Document {
        uri: uri.clone(),
        controller,
        proof_uri: uri,
        proof_kind: Kind::Track,
        kind: Kind::Track,
        name: text(&value["title"]),
        summary: plamenu_ap::text::sanitize_remote_html(&text(&value["description"])),
        media_of: None,
        parent,
        audio_url,
        details: super::details::Details {
            artist: text(&value["artist"]["name"]),
            artist_uri: value["artist_id"]
                .as_str()
                .filter(|id| key(id))
                .map(|id| format!("{base}/artists/{id}")),
            track_number: value["track_number"]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .filter(|n| *n > 0),
            disc_number: value["disc_number"]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .filter(|n| *n > 0),
            album: text(&value["album"]["title"]),
            genre: text(&value["genre"]),
            duration: value["duration"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0),
            image_url,
            release_year: value["release_year"]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .filter(|n| (1..=9999).contains(n)),
            ..Default::default()
        },
        catalogue: Some(("track".into(), id.into())),
    })
}
async fn cache(state: &AppState, viewer: i64, docs: Vec<Document>) -> Result<Resource, ApiError> {
    let uri = docs.first().ok_or(ApiError::NotFound)?.uri.clone();
    let started = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&state.pool)
        .await
        .map_err(DbError::from)?;
    super::cache_import(
        state,
        viewer,
        &uri,
        docs,
        Vec::new(),
        super::members::Pages::default(),
        started,
    )
    .await
}
pub async fn import_track(state: &AppState, viewer: i64, uri: &str) -> Result<Resource, ApiError> {
    let base = origin(uri)?;
    identify(state, viewer, &base).await?;
    let id = track_key(uri).ok_or(ApiError::NotFound)?;
    let value = json(
        state,
        &format!("{base}/api/v1/tracks/{id}?include=artist,album,owner"),
    )
    .await?;
    let doc = track(&base, &value)?;
    if doc.uri != format!("{base}/api/v1/tracks/{id}") {
        return Err(ApiError::NotFound);
    }
    cache(state, viewer, vec![doc]).await
}
/// Enrich only recognized canonical Songhive album/artist UUID pages. No AP
/// publication is invented: the anonymous API track retains its separate URI.
pub async fn enrich(state: &AppState, viewer: i64, root: &Resource) -> Result<(), ApiError> {
    children(state, viewer, root, 0).await?;
    Ok(())
}
pub async fn children(
    state: &AppState,
    viewer: i64,
    root: &Resource,
    offset: usize,
) -> Result<Option<(Vec<Resource>, bool)>, ApiError> {
    if !matches!(root.kind.as_str(), "album" | "artist") {
        return Ok(None);
    }
    if offset > 10000 {
        return Err(invalid("The catalogue page is too large"));
    }
    let url = document::url(&root.uri)?;
    let prefix = if root.kind == "album" {
        "/albums/"
    } else {
        "/artists/"
    };
    let Some(id) = url.path().strip_prefix(prefix).filter(|id| key(id)) else {
        return Ok(None);
    };
    let base = origin(&root.uri)?;
    match identify(state, viewer, &base).await {
        Ok(_) => {}
        Err(ApiError::NotFound | ApiError::Unprocessable(_)) => return Ok(None),
        Err(e) => return Err(e),
    }
    let field = if root.kind == "album" {
        "album_id"
    } else {
        "artist_id"
    };
    let uri = format!(
        "{base}/api/v1/tracks/?{field}={id}&include=artist,album,owner&limit=24&offset={offset}"
    );
    let values = json(state, &uri).await?;
    let rows = values
        .as_array()
        .filter(|v| v.len() <= PAGE_SIZE)
        .ok_or_else(|| invalid("The catalogue page is too large"))?;
    let mut documents = Vec::new();
    for row in rows {
        if row[field].as_str() != Some(id) {
            return Err(invalid(
                "The catalogue returned a track outside this album or artist",
            ));
        }
        let doc = match track(&base, row) {
            Ok(doc) => doc,
            Err(ApiError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        documents.push(doc);
    }
    let uris = documents.iter().map(|d| d.uri.clone()).collect::<Vec<_>>();
    if !documents.is_empty() {
        cache(state, viewer, documents.clone()).await?;
    }
    let started = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&state.pool)
        .await
        .map_err(DbError::from)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    super::members::store(
        &mut tx,
        &root.uri,
        &super::members::Pages {
            next: None,
            exhaustive: offset == 0 && rows.len() < PAGE_SIZE,
            items: vec![(
                uri.clone(),
                uris.iter().map(|id| serde_json::json!({"id":id})).collect(),
            )],
        },
        &documents,
        started,
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    let mut by_uri = super::for_uris_pool(&state.pool, &uris, Some(viewer))
        .await?
        .into_iter()
        .map(|r| (r.uri.clone(), r))
        .collect::<std::collections::HashMap<_, _>>();
    let resources = uris.iter().filter_map(|uri| by_uri.remove(uri)).collect();
    super::get(state, root.id, viewer).await?;
    Ok(Some((resources, rows.len() == PAGE_SIZE)))
}
/// Recheck the anonymous API before relaying bytes; cached metadata does not
/// retain a grant after a track becomes private, paid, deleted or changes owner.
pub async fn current(state: &AppState, resource: &Resource) -> Result<(), ApiError> {
    if resource.catalogue_kind.as_deref() == Some("stream") {
        let base = origin(&resource.uri)?;
        let rows = json(state, &format!("{base}/api/v1/streams/")).await?;
        let value = rows
            .as_array()
            .filter(|v| v.len() <= 100)
            .and_then(|rows| {
                rows.iter()
                    .find(|v| v["id"].as_str() == resource.catalogue_key.as_deref())
            })
            .ok_or(ApiError::NotFound)?;
        let doc = stream(&base, value)?;
        let owner: String = sqlx::query_scalar("SELECT uri FROM accounts WHERE id=$1")
            .bind(resource.controller_id)
            .fetch_one(&state.pool)
            .await
            .map_err(DbError::from)?;
        return if doc.controller == owner
            && doc.uri == resource.uri
            && doc.audio_url == resource.audio_url
        {
            Ok(())
        } else {
            Err(ApiError::NotFound)
        };
    }
    if resource.catalogue_kind.is_none() {
        return Ok(());
    }
    let base = origin(&resource.uri)?;
    let id = resource
        .catalogue_key
        .as_deref()
        .filter(|id| key(id))
        .ok_or(ApiError::NotFound)?;
    let value = json(
        state,
        &format!("{base}/api/v1/tracks/{id}?include=artist,album,owner"),
    )
    .await?;
    let doc = track(&base, &value)?;
    let owner: String = sqlx::query_scalar("SELECT uri FROM accounts WHERE id=$1")
        .bind(resource.controller_id)
        .fetch_one(&state.pool)
        .await
        .map_err(DbError::from)?;
    if doc.uri != resource.uri || doc.controller != owner || doc.audio_url != resource.audio_url {
        return Err(ApiError::NotFound);
    }
    Ok(())
}
#[derive(Clone)]
pub struct Item {
    pub name: String,
    pub subtitle: String,
    pub url: String,
    pub playable: bool,
}
pub struct Page {
    pub title: String,
    pub items: Vec<Item>,
    pub more: bool,
}
pub async fn browse(
    state: &AppState,
    viewer: i64,
    server: &str,
    kind: &str,
    q: &str,
    offset: usize,
) -> Result<Page, ApiError> {
    if q.len() > 200 || offset > 10000 {
        return Err(invalid("The catalogue search is too large"));
    }
    let base = origin(server)?;
    let title = identify(state, viewer, &base).await?;
    let endpoint = match kind {
        "tracks" | "albums" | "artists" | "radios" | "streams" => kind,
        _ => return Err(invalid("Choose a music catalogue section")),
    };
    let mut url =
        Url::parse(&format!("{base}/api/v1/{endpoint}/")).map_err(|_| ApiError::NotFound)?;
    url.query_pairs_mut()
        .append_pair("limit", "24")
        .append_pair("offset", &offset.to_string())
        .append_pair("q", q)
        .append_pair("include", "artist,album,owner");
    let values = json(state, url.as_str()).await?;
    let rows = values
        .as_array()
        .filter(|rows| rows.len() <= 100)
        .ok_or_else(|| invalid("The catalogue page is too large"))?;
    let mut items = Vec::new();
    for row in rows.iter().take(PAGE_SIZE) {
        let Some(id) = row["id"].as_str().filter(|id| key(id)) else {
            continue;
        };
        if row["visibility"].as_str().is_some_and(|v| v != "public")
            || row["paid"].as_bool() == Some(true)
        {
            continue;
        }
        let target = if kind == "streams" {
            row["stream_url"]
                .as_str()
                .map(|v| joined(&base, v))
                .transpose()?
                .ok_or(ApiError::NotFound)?
        } else if kind == "radios" {
            format!("{base}/api/v1/radios/{id}")
        } else if kind == "tracks" {
            format!("{base}/api/v1/tracks/{id}")
        } else {
            format!("{base}/{kind}/{id}")
        };
        items.push(Item {
            name: text(row.get("title").unwrap_or(&row["name"])),
            subtitle: text(&row["artist"]["name"]),
            url: target,
            playable: (kind == "tracks" && row["audio_url"].as_str().is_some())
                || (kind == "streams" && row["online"] == true && row["enabled"] == true),
        });
    }
    Ok(Page {
        title,
        items,
        more: rows.len() >= PAGE_SIZE,
    })
}

fn stream(base: &str, value: &Value) -> Result<Document, ApiError> {
    let id = value["id"]
        .as_str()
        .filter(|id| key(id))
        .ok_or(ApiError::NotFound)?;
    if value["visibility"] != "public" || value["enabled"] != true || value["online"] != true {
        return Err(ApiError::NotFound);
    }
    let uri = joined(
        base,
        value["stream_url"].as_str().ok_or(ApiError::NotFound)?,
    )?;
    let username = value["owner"]["username"]
        .as_str()
        .filter(|n| !n.is_empty() && n.len() <= 64)
        .ok_or(ApiError::NotFound)?;
    let segment = url::form_urlencoded::byte_serialize(username.as_bytes()).collect::<String>();
    Ok(Document {
        uri: uri.clone(),
        controller: format!("{base}/users/{segment}"),
        proof_uri: uri.clone(),
        proof_kind: Kind::Audio,
        kind: Kind::Audio,
        name: text(&value["name"]),
        summary: plamenu_ap::text::sanitize_remote_html(&text(&value["description"])),
        media_of: None,
        parent: None,
        audio_url: Some(uri),
        details: super::details::Details {
            artist: text(&value["owner"]["display_name"]),
            genre: text(&value["genre"]),
            ..Default::default()
        },
        catalogue: Some(("stream".into(), id.into())),
    })
}
pub async fn import_stream(state: &AppState, viewer: i64, uri: &str) -> Result<Resource, ApiError> {
    let base = origin(uri)?;
    identify(state, viewer, &base).await?;
    let rows = json(state, &format!("{base}/api/v1/streams/")).await?;
    let values = rows
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or(ApiError::NotFound)?;
    for row in values {
        if row["stream_url"]
            .as_str()
            .is_some_and(|s| joined(&base, s).is_ok_and(|s| s == uri))
        {
            return cache(state, viewer, vec![stream(&base, row)?]).await;
        }
    }
    Err(ApiError::NotFound)
}
pub async fn import_radio(state: &AppState, viewer: i64, uri: &str) -> Result<Resource, ApiError> {
    let base = origin(uri)?;
    identify(state, viewer, &base).await?;
    let u = document::url(uri)?;
    let id = u
        .path()
        .strip_prefix("/api/v1/radios/")
        .filter(|id| key(id))
        .ok_or(ApiError::NotFound)?;
    let value = json(state, uri).await?;
    if value["id"] != id || value["visibility"] != "public" {
        return Err(ApiError::NotFound);
    }
    cache(
        state,
        viewer,
        vec![Document {
            uri: uri.into(),
            controller: format!("{base}/ap/actor"),
            proof_uri: uri.into(),
            proof_kind: Kind::Library,
            kind: Kind::Library,
            name: text(&value["name"]),
            summary: plamenu_ap::text::sanitize_remote_html(&text(&value["description"])),
            media_of: None,
            parent: None,
            audio_url: None,
            details: super::details::Details::default(),
            catalogue: None,
        }],
    )
    .await
}
