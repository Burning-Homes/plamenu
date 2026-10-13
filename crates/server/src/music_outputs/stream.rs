//! Listener bodies own finite leases and recheck current output authority.
use super::Output;
use crate::{AppState, error::ApiError};
use axum::{
    body::{Body, Bytes},
    http::header,
    response::Response,
};
use plamenu_db::{DbError, id};
use time::OffsetDateTime;
struct Listener {
    state: AppState,
    id: i64,
}
impl Drop for Listener {
    fn drop(&mut self) {
        let state = self.state.clone();
        let id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = sqlx::query("DELETE FROM music_output_listeners WHERE id=$1")
                    .bind(id)
                    .execute(&state.pool)
                    .await;
            });
        }
    }
}
async fn admit(state: &AppState, row: &Output) -> Result<Listener, ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410171)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("DELETE FROM music_output_listeners WHERE lease_until<now()")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let (local, total): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER(WHERE output_id=$1),count(*) FROM music_output_listeners",
    )
    .bind(row.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if local >= 64 || total >= 256 {
        return Err(ApiError::ServiceUnavailable(
            "HTTP stream listener limit reached".into(),
        ));
    }
    let listener = id::next();
    sqlx::query("INSERT INTO music_output_listeners(id,output_id,generation,lease_until) VALUES($1,$2,$3,now()+interval '5 seconds')")
        .bind(listener).bind(row.id).bind(row.generation).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Listener {
        state: state.clone(),
        id: listener,
    })
}
#[derive(Clone)]
pub enum Access {
    Listener(Option<String>),
    Owner { viewer: i64, token: i64 },
}
impl Access {
    async fn authorize(&self, state: &AppState, output: i64) -> Result<Output, ApiError> {
        match self {
            Self::Listener(token) => super::authorize(state, output, token.as_deref()).await,
            Self::Owner { viewer, token } => {
                let scopes:Option<String>=sqlx::query_scalar("SELECT t.scopes FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.revoked_at IS NULL AND u.account_id=$2")
                    .bind(token).bind(viewer).fetch_optional(&state.pool).await.map_err(DbError::from)?;
                if !scopes
                    .as_deref()
                    .is_some_and(|scope| crate::auth::scopes_satisfy(scope, "read:statuses"))
                {
                    return Err(ApiError::NotFound);
                }
                let row = super::get(state, output, Some(*viewer)).await?;
                if row.owner_id != *viewer || !row.enabled {
                    return Err(ApiError::NotFound);
                }
                Ok(row)
            }
        }
    }
}

struct Relay {
    state: AppState,
    row: Output,
    access: Access,
    sequence: i64,
    header: Option<Vec<u8>>,
    burst: Option<super::ogg::Burst>,
    icy: Option<super::icy::Interleave>,
    listener: Listener,
}
fn denied(error: ApiError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, error)
}
async fn next(mut relay: Relay) -> Result<Option<(Bytes, Relay)>, std::io::Error> {
    loop {
        let current = relay
            .access
            .authorize(&relay.state, relay.row.id)
            .await
            .map_err(denied)?;
        if current.generation != relay.row.generation
            || current.transport != "playing"
            || current
                .lease_until
                .is_none_or(|until| until <= OffsetDateTime::now_utc())
        {
            return Ok(None);
        }
        let renewed=sqlx::query(
            "UPDATE music_output_listeners SET lease_until=now()+interval '5 seconds' WHERE id=$1 AND lease_until>now()",
        )
        .bind(relay.listener.id)
        .execute(&relay.state.pool)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
        if renewed.rows_affected() != 1 {
            return Ok(None);
        }
        if let Some(header) = relay.header.take() {
            let bytes = relay.icy.as_mut().map_or_else(
                || header.clone(),
                |icy| icy.push(&header, &current.now_playing),
            );
            return Ok(Some((Bytes::from(bytes), relay)));
        }
        let chunk:Option<(i64,Vec<u8>,String)>=sqlx::query_as("SELECT sequence,bytes,title FROM music_output_chunks WHERE output_id=$1 AND generation=$2 AND sequence>$3 AND created_at>now()-interval '5 seconds' ORDER BY sequence LIMIT 1")
            .bind(relay.row.id).bind(relay.row.generation).bind(relay.sequence).fetch_optional(&relay.state.pool).await.map_err(|error|std::io::Error::other(error.to_string()))?;
        if let Some((sequence, bytes, title)) = chunk {
            if sequence != relay.sequence + 1 && relay.burst.is_some() {
                relay.burst = Some(super::ogg::Burst::new(&current.codec_header).map_err(denied)?);
            }
            relay.sequence = sequence;
            let bytes = if let Some(burst) = &mut relay.burst {
                let Some(bytes) = burst.push(&bytes).map_err(denied)? else {
                    continue;
                };
                bytes
            } else {
                bytes
            };
            let bytes = relay
                .icy
                .as_mut()
                .map_or_else(|| bytes.clone(), |icy| icy.push(&bytes, &title));
            return Ok(Some((Bytes::from(bytes), relay)));
        }
        tokio::select! {()=relay.state.shutdown.cancelled()=>return Ok(None),()=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
    }
}
pub async fn serve(
    state: &AppState,
    output: i64,
    token: Option<&str>,
) -> Result<Response, ApiError> {
    serve_as(state, output, Access::Listener(token.map(str::to_owned))).await
}
pub async fn serve_as(state: &AppState, output: i64, access: Access) -> Result<Response, ApiError> {
    serve_with(state, output, access, false, false).await
}
pub async fn serve_with(
    state: &AppState,
    output: i64,
    access: Access,
    icy: bool,
    head: bool,
) -> Result<Response, ApiError> {
    let row = access.authorize(state, output).await?;
    if row.transport != "playing"
        || row
            .lease_until
            .is_none_or(|until| until <= OffsetDateTime::now_utc())
    {
        return Err(ApiError::NotFound);
    }
    let header = if row.format == "mp3" {
        None
    } else {
        if row.codec_header.is_empty() {
            return Err(ApiError::ServiceUnavailable(
                "HTTP audio is initializing".into(),
            ));
        }
        Some(row.codec_header.clone())
    };
    let burst = header.as_deref().map(super::ogg::Burst::new).transpose()?;
    let kind = if row.format == "mp3" {
        "audio/mpeg"
    } else {
        "audio/ogg"
    };
    let mut response = axum::http::Response::builder()
        .header(header::CONTENT_TYPE, kind)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::VARY, "Authorization, Cookie, Icy-MetaData, Origin")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::ACCEPT_RANGES, "none")
        .header("x-accel-buffering", "no")
        .header("icy-br", row.bitrate.to_string());
    for (name, value) in [
        ("icy-name", &row.name),
        ("icy-description", &row.description),
        ("icy-genre", &row.genre),
    ] {
        let safe = value
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        if !safe.is_empty() {
            response = response.header(name, safe);
        }
    }
    if icy {
        response = response.header("icy-metaint", super::icy::INTERVAL.to_string());
    }
    if row.visibility == "public" || matches!(&access, Access::Listener(Some(_))) {
        response = response.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    }
    if head {
        return response
            .body(Body::empty())
            .map_err(|error| ApiError::Internal(Box::new(error)));
    }
    let listener = admit(state, &row).await?;
    let sequence:i64=sqlx::query_scalar("SELECT GREATEST(COALESCE(max(sequence),0)-8,0) FROM music_output_chunks WHERE output_id=$1 AND generation=$2")
        .bind(row.id).bind(row.generation).fetch_one(&state.pool).await.map_err(DbError::from)?;
    let relay = Relay {
        state: state.clone(),
        row,
        access,
        sequence,
        header,
        burst,
        icy: icy.then(super::icy::Interleave::default),
        listener,
    };
    response
        .body(Body::from_stream(futures_util::stream::try_unfold(
            relay, next,
        )))
        .map_err(|error| ApiError::Internal(Box::new(error)))
}
