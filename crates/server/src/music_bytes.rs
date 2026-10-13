//! Music bodies retain current source and credential authority after slow reads.
use crate::{
    AppState, auth::CurrentUser, error::ApiError, music_assets, music_renditions::cold::Access,
};
use axum::{
    body::{Body, Bytes},
    response::Response,
};
use futures_util::StreamExt as _;
use plamenu_db::DbError;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Credential {
    pub owner: i64,
    pub token: i64,
}
impl Credential {
    pub(crate) fn from_user(user: &CurrentUser) -> Self {
        Self {
            owner: user.account.id,
            token: user.token_id,
        }
    }
    pub(crate) async fn check(self, state: &AppState) -> Result<(), ApiError> {
        let row:Option<(String,time::OffsetDateTime,i64)>=sqlx::query_as("SELECT t.scopes,t.created_at,t.app_id FROM oauth_tokens t JOIN users u ON u.id=t.user_id JOIN accounts a ON a.id=u.account_id WHERE t.id=$1 AND a.id=$2 AND t.revoked_at IS NULL AND u.approved AND u.confirmed_at IS NOT NULL AND NOT u.disabled AND a.deleted_at IS NULL AND a.suspended_at IS NULL")
            .bind(self.token).bind(self.owner).fetch_optional(&state.pool).await.map_err(DbError::from)?;
        let (scopes, created, app) = row.ok_or(ApiError::NotFound)?;
        if !crate::auth::scopes_satisfy(&scopes, "read:statuses")
            || (state.web_app_id().await.is_ok_and(|web| web == app)
                && time::OffsetDateTime::now_utc() - created >= crate::auth::MAX_WEB_SESSION_AGE)
        {
            return Err(ApiError::NotFound);
        }
        Ok(())
    }
}
#[derive(Clone)]
pub(crate) enum Source {
    Asset {
        id: i64,
        viewer: Option<i64>,
        scope: Option<i64>,
        hash: String,
        file: String,
        original: Option<String>,
    },
    Rendition {
        asset: i64,
        job: i64,
        access: Access,
        hash: String,
        file: String,
    },
    Artwork {
        id: i64,
        viewer: Option<i64>,
        scope: Option<i64>,
        file: String,
    },
}
impl Source {
    pub(crate) async fn asset(
        state: &AppState,
        asset: &music_assets::Asset,
        viewer: Option<i64>,
        scope: Option<i64>,
        original: bool,
    ) -> Result<Self, ApiError> {
        Ok(Self::Asset {
            id: asset.id,
            viewer,
            scope,
            hash: asset.audio_hash.clone(),
            file: if original {
                asset.original_file.clone()
            } else {
                asset.stream_file.clone()
            },
            original: if original {
                Some(crate::music_original::proof(state, asset).await?)
            } else {
                None
            },
        })
    }
    pub(crate) fn rendition(
        row: &crate::music_renditions::Rendition,
        access: Access,
    ) -> Result<Self, ApiError> {
        Ok(Self::Rendition {
            asset: row.asset_id,
            job: row.id,
            access,
            hash: row.source_hash.clone(),
            file: row.file_name.clone().ok_or(ApiError::NotFound)?,
        })
    }
    async fn check(&self, state: &AppState) -> Result<(), ApiError> {
        match self {
            Self::Asset {
                id,
                viewer,
                scope,
                hash,
                file,
                original,
            } => {
                let row = if let Some(scope) = scope {
                    let current = crate::music_shares::resolve_job(state, *scope, *viewer).await?;
                    crate::music_shares::asset(state, &current, *id, *viewer).await?
                } else {
                    music_assets::get(state, *id, *viewer).await?
                };
                let current = if original.is_some() {
                    &row.original_file
                } else {
                    &row.stream_file
                };
                if &row.audio_hash != hash || current != file {
                    return Err(ApiError::NotFound);
                }
                if let Some(proof) = original
                    && &crate::music_original::proof(state, &row).await? != proof
                {
                    return Err(ApiError::NotFound);
                }
            }
            Self::Rendition {
                asset,
                job,
                access,
                hash,
                file,
            } => {
                let row = access.get(state, *asset, *job).await?;
                if row.source_hash != *hash
                    || row.file_name.as_ref() != Some(file)
                    || row.state != "ready"
                {
                    return Err(ApiError::NotFound);
                }
            }
            Self::Artwork {
                id,
                viewer,
                scope,
                file,
            } => {
                let row = if let Some(scope) = scope {
                    let current = crate::music_shares::resolve_job(state, *scope, *viewer).await?;
                    crate::music_shares::artwork(state, &current, *id, *viewer).await?
                } else {
                    crate::music_artworks::get(state, *id, *viewer).await?
                };
                if &row.file_name != file {
                    return Err(ApiError::NotFound);
                }
            }
        }
        Ok(())
    }
}
struct Relay {
    state: AppState,
    source: Option<Source>,
    credential: Option<Credential>,
    body: axum::body::BodyDataStream,
    checked: Instant,
}
async fn next(mut relay: Relay) -> Result<Option<(Bytes, Relay)>, std::io::Error> {
    let bytes = tokio::select! {()=relay.state.shutdown.cancelled()=>return Ok(None),bytes=relay.body.next()=>bytes.transpose().map_err(std::io::Error::other)?};
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    if relay.state.shutdown.is_cancelled() {
        return Ok(None);
    }
    if relay.checked.elapsed() >= Duration::from_secs(1) {
        if let Some(credential) = relay.credential {
            credential
                .check(&relay.state)
                .await
                .map_err(std::io::Error::other)?;
        }
        if let Some(source) = &relay.source {
            source
                .check(&relay.state)
                .await
                .map_err(std::io::Error::other)?;
        }
        relay.checked = Instant::now();
    }
    Ok(Some((bytes, relay)))
}
pub(crate) async fn wrap(
    state: &AppState,
    response: Response,
    source: Option<Source>,
    credential: Option<Credential>,
) -> Result<Response, ApiError> {
    if !response.status().is_success() {
        return Ok(response);
    }
    if let Some(credential) = credential {
        credential.check(state).await?;
    }
    if let Some(source) = &source {
        source.check(state).await?;
    }
    let (parts, body) = response.into_parts();
    let relay = Relay {
        state: state.clone(),
        source,
        credential,
        body: body.into_data_stream(),
        checked: Instant::now(),
    };
    Ok(Response::from_parts(
        parts,
        Body::from_stream(futures_util::stream::try_unfold(relay, next)),
    ))
}
