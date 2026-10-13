//! Remote audio relays own their network body and finite admission slot.
use super::Resource;
use crate::{AppState, error::ApiError};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, header},
    response::Response,
};
use futures_util::StreamExt as _;
use plamenu_db::DbError;
use plamenu_federation::{FederationError, FetchedEnclosure};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

static LISTENERS: OnceLock<Mutex<HashMap<i64, u16>>> = OnceLock::new();
struct Listener(i64);
impl Listener {
    fn admit(viewer: i64) -> Result<Self, ApiError> {
        let mut listeners = LISTENERS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| ApiError::ServiceUnavailable("Audio playback is busy".into()))?;
        let total: u32 = listeners.values().map(|count| u32::from(*count)).sum();
        if total >= 256 || listeners.get(&viewer).copied().unwrap_or(0) >= 16 {
            return Err(ApiError::ServiceUnavailable(
                "Audio playback limit reached".into(),
            ));
        }
        *listeners.entry(viewer).or_default() += 1;
        Ok(Self(viewer))
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        let mut listeners = LISTENERS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = listeners.get_mut(&self.0) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                listeners.remove(&self.0);
            }
        }
    }
}
pub struct Source {
    pub resource: Resource,
    pub media: Resource,
    pub url: String,
}
pub async fn source(state: &AppState, id: i64, viewer: i64) -> Result<Source, ApiError> {
    let resource = super::get(state, id, viewer).await?;
    super::catalogue::current(state, &resource).await?;
    if !matches!(resource.kind.as_str(), "audio" | "track") {
        return Err(ApiError::NotFound);
    }
    let media = if resource.audio_url.is_some() {
        resource.clone()
    } else {
        super::media_for_uris_pool(
            &state.pool,
            std::slice::from_ref(&resource.uri),
            Some(viewer),
        )
        .await?
        .remove(&resource.uri)
        .ok_or(ApiError::NotFound)?
    };
    let url = media.audio_url.clone().ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &url).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Source {
        resource,
        media,
        url,
    })
}

#[derive(Clone, Copy)]
pub enum Access {
    Client {
        principal: crate::music_clients::auth::Principal,
    },
    Account {
        viewer: i64,
        token: i64,
    },
    PublicCollection {
        collection: i64,
        entry: i64,
    },
    ShareCollection {
        link: i64,
        collection: i64,
        entry: i64,
    },
}
impl Access {
    pub(super) fn viewer(self) -> i64 {
        match self {
            Self::Account { viewer, .. } => viewer,
            Self::Client { principal } => principal.owner,
            Self::PublicCollection { .. } | Self::ShareCollection { .. } => 0,
        }
    }
    fn listener(self) -> i64 {
        match self {
            Self::Account { viewer, .. } => viewer,
            Self::Client { principal } => principal.owner,
            Self::PublicCollection { collection, .. }
            | Self::ShareCollection { collection, .. } => -collection,
        }
    }
    pub(super) async fn authorize(self, state: &AppState, resource: i64) -> Result<(), ApiError> {
        match self {
            Self::Account { viewer, token } => authorize_account(state, viewer, token).await,
            Self::Client { principal } => principal.current(state).await,
            Self::PublicCollection { collection, entry } => {
                let parent = crate::music_collections::get(state, collection, None).await?;
                if parent.visibility != "public" {
                    return Err(ApiError::NotFound);
                }
                authorize_entry(state, collection, entry, resource).await
            }
            Self::ShareCollection {
                link,
                collection,
                entry,
            } => {
                let scope = crate::music_shares::resolve_job(state, link, None).await?;
                if scope.target_id != collection
                    || !matches!(scope.kind.as_str(), "library" | "playlist")
                {
                    return Err(ApiError::NotFound);
                }
                authorize_entry(state, collection, entry, resource).await
            }
        }
    }
    async fn check(self, state: &AppState, source: &Source) -> Result<(), ApiError> {
        self.authorize(state, source.resource.id).await?;
        let requested = super::get(state, source.resource.id, self.viewer()).await?;
        if !matches!(requested.kind.as_str(), "audio" | "track") {
            return Err(ApiError::NotFound);
        }
        let current = super::get(state, source.media.id, self.viewer()).await?;
        if current.media_of != source.media.media_of
            || !matches!(current.kind.as_str(), "audio" | "track")
            || current.audio_url.as_deref() != Some(source.url.as_str())
            || !crate::instance_policy::can_federate_url(
                &state.pool,
                &state.config.domain,
                &source.url,
            )
            .await?
        {
            return Err(ApiError::NotFound);
        }
        Ok(())
    }
}
async fn authorize_entry(
    state: &AppState,
    collection: i64,
    entry: i64,
    resource: i64,
) -> Result<(), ApiError> {
    let member:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_collection_entries WHERE collection_id=$1 AND id=$2 AND remote_id=$3)")
        .bind(collection).bind(entry).bind(resource).fetch_one(&state.pool).await.map_err(DbError::from)?;
    if member {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

async fn authorize_account(state: &AppState, viewer: i64, token: i64) -> Result<(), ApiError> {
    let scopes:Option<String>=sqlx::query_scalar("SELECT t.scopes FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.revoked_at IS NULL AND u.account_id=$2").bind(token).bind(viewer).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    if !scopes
        .as_deref()
        .is_some_and(|scopes| crate::auth::scopes_satisfy(scopes, "read:statuses"))
    {
        return Err(ApiError::NotFound);
    }
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, viewer).await?;
    drop(conn);
    Ok(())
}

struct Relay {
    state: AppState,
    access: Access,
    source: Source,
    upstream: FetchedEnclosure,
    _listener: Listener,
    checked: Instant,
    started: Instant,
    received: u64,
    catalogue_checked: Instant,
}
fn interrupted(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, message)
}
async fn next(mut relay: Relay) -> Result<Option<(Bytes, Relay)>, std::io::Error> {
    if relay.state.shutdown.is_cancelled() || relay.started.elapsed() > Duration::from_hours(6) {
        return Err(interrupted("Audio playback stopped"));
    }
    if relay.source.resource.catalogue_kind.is_some()
        && relay.catalogue_checked.elapsed() >= Duration::from_secs(10)
    {
        super::catalogue::current(&relay.state, &relay.source.resource)
            .await
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error))?;
        relay.catalogue_checked = Instant::now();
    }
    if relay.checked.elapsed() >= Duration::from_secs(1) {
        relay
            .access
            .check(&relay.state, &relay.source)
            .await
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error))?;
        relay.checked = Instant::now();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let chunk = loop {
        let remaining = Duration::from_secs(1).saturating_sub(relay.checked.elapsed());
        tokio::select! {
            ()=relay.state.shutdown.cancelled()=>return Err(interrupted("Audio playback stopped")),
            ()=tokio::time::sleep(remaining)=>{
                relay.access.check(&relay.state,&relay.source).await.map_err(|error|std::io::Error::new(std::io::ErrorKind::PermissionDenied,error))?;
                relay.checked=Instant::now();
                if relay.source.resource.catalogue_kind.is_some() && relay.catalogue_checked.elapsed()>=Duration::from_secs(10) {
                    super::catalogue::current(&relay.state,&relay.source.resource).await.map_err(|error|std::io::Error::new(std::io::ErrorKind::PermissionDenied,error))?;
                    relay.catalogue_checked=Instant::now();
                }
            },
            result=tokio::time::timeout_at(deadline,relay.upstream.bytes.next())=>break result.map_err(|_|interrupted("Audio origin stalled"))?,
        }
    };
    match chunk {
        Some(Ok(bytes)) => {
            relay.received = relay
                .received
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            if relay.received > 16 * 1024 * 1024 * 1024
                || relay
                    .upstream
                    .content_length
                    .is_some_and(|length| relay.received > length)
            {
                return Err(interrupted("Audio origin exceeded its byte bound"));
            }
            Ok(Some((Bytes::from(bytes), relay)))
        }
        Some(Err(error)) => Err(std::io::Error::other(error)),
        None => {
            if relay
                .upstream
                .content_length
                .is_some_and(|length| relay.received != length)
            {
                return Err(interrupted("Audio origin ended early"));
            }
            Ok(None)
        }
    }
}
fn validate_range(upstream: &FetchedEnclosure, headers: &HeaderMap) -> Result<(), ApiError> {
    if upstream.status != 206 {
        return Ok(());
    }
    let raw = upstream
        .content_range
        .as_deref()
        .ok_or_else(|| ApiError::BadGateway("Missing remote audio range".into()))?;
    let total = raw
        .rsplit('/')
        .next()
        .and_then(|total| total.parse::<u64>().ok())
        .ok_or_else(|| ApiError::BadGateway("Invalid remote audio range".into()))?;
    match crate::routes::media::parse_range(headers.get(header::RANGE), total) {
        crate::routes::media::ByteRange::Partial(start, end)
            if raw == format!("bytes {start}-{end}/{total}") =>
        {
            Ok(())
        }
        _ => Err(ApiError::BadGateway("Incorrect remote audio range".into())),
    }
}
pub async fn serve(
    state: &AppState,
    id: i64,
    access: Access,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let source = source(state, id, access.viewer()).await?;
    access.check(state, &source).await?;
    let range = headers
        .get(header::RANGE)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ApiError::BadRequest("Invalid audio range".into()))
        })
        .transpose()?;
    if range.is_some_and(|range| {
        range.len() >= 100
            || !range.starts_with("bytes=")
            || range.contains(',')
            || !range
                .bytes()
                .all(|byte| byte.is_ascii_digit() || b"bytes=- ".contains(&byte))
    }) {
        return Err(ApiError::BadRequest("Invalid audio range".into()));
    }
    let listener = Listener::admit(access.listener())?;
    let upstream = state
        .federation
        .fetch_enclosure(&source.url, range)
        .await
        .map_err(|error| match error {
            FederationError::Status(401 | 403 | 404 | 410) => ApiError::NotFound,
            _ => ApiError::BadGateway("Could not stream remote audio".into()),
        })?;
    access.check(state, &source).await?;
    validate_range(&upstream, headers)?;
    if upstream
        .content_length
        .is_some_and(|length| length > 16 * 1024 * 1024 * 1024)
    {
        return Err(ApiError::Unprocessable("Remote audio is too large".into()));
    }
    let mut response = axum::http::Response::builder()
        .status(upstream.status)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::VARY, "Authorization, Cookie")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_TYPE, &upstream.content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(range) = &upstream.content_range {
        response = response.header(header::CONTENT_RANGE, range);
    }
    if upstream.status == 416 {
        return response
            .body(Body::empty())
            .map_err(|error| ApiError::Internal(Box::new(error)));
    }
    if let Some(length) = upstream.content_length {
        response = response.header(header::CONTENT_LENGTH, length);
    }
    let relay = Relay {
        state: state.clone(),
        access,
        source,
        upstream,
        _listener: listener,
        checked: Instant::now(),
        started: Instant::now(),
        received: 0,
        catalogue_checked: Instant::now(),
    };
    response
        .body(Body::from_stream(futures_util::stream::try_unfold(
            relay, next,
        )))
        .map_err(|error| ApiError::Internal(Box::new(error)))
}
