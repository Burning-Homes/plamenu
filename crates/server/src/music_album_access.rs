//! Album metadata access and explicitly shared owned tracks mutate atomically.
use crate::{
    AppState,
    error::ApiError,
    music_assets,
    music_entities::{self, Entity, Kind},
    music_publications,
};
use plamenu_db::DbError;
use serde::{Deserialize, Serialize};
#[derive(Deserialize)]
pub struct Edit {
    pub revision: String,
    pub visibility: String,
    #[serde(default)]
    pub share_tracks: bool,
}
#[derive(Serialize)]
pub struct Member {
    pub id: String,
    pub username: String,
}
#[derive(Serialize)]
pub struct Members {
    pub items: Vec<Member>,
    pub before: Option<String>,
}
pub async fn members(
    state: &AppState,
    owner: i64,
    album: i64,
    before: Option<i64>,
) -> Result<Members, ApiError> {
    if before.is_some_and(|value| value <= 0) {
        return Err(ApiError::Unprocessable("Invalid member cursor".into()));
    }
    let current = music_entities::get(state, Kind::Album, album, Some(owner)).await?;
    if current.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    let mut rows: Vec<(i64, String)> = sqlx::query_as("SELECT g.account_id,a.username FROM music_album_grants g JOIN accounts a ON a.id=g.account_id WHERE g.album_id=$1 AND ($2::bigint IS NULL OR g.account_id<$2) ORDER BY g.account_id DESC LIMIT 51")
        .bind(album).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let before = if rows.len() > 50 {
        rows.truncate(50);
        rows.last().map(|row| row.0.to_string())
    } else {
        None
    };
    Ok(Members {
        items: rows
            .into_iter()
            .map(|(id, username)| Member {
                id: id.to_string(),
                username,
            })
            .collect(),
        before,
    })
}
fn revision(input: &str) -> Result<i64, ApiError> {
    input
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid album revision".into()))
}
pub async fn update(
    state: &AppState,
    owner: i64,
    album: i64,
    input: &Edit,
) -> Result<Entity, ApiError> {
    let revision = revision(&input.revision)?;
    if !["source", "private", "local", "public"].contains(&input.visibility.as_str()) {
        return Err(ApiError::Unprocessable("Invalid album visibility".into()));
    }
    let previous = music_entities::get(state, Kind::Album, album, Some(owner)).await?;
    if previous.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let sources = music_entities::lock_sources(&mut tx, owner, Kind::Album, album).await?;
    music_entities::lock_entity(&mut tx, owner, Kind::Album, album, revision).await?;
    sqlx::query("UPDATE music_albums SET visibility=$3,share_tracks=$4,revision=revision+1,updated_at=now() WHERE id=$1 AND owner_id=$2").bind(album).bind(owner).bind(&input.visibility).bind(input.share_tracks).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("UPDATE music_assets SET revision=revision+1 WHERE id=ANY($1) AND owner_id=$2")
        .bind(&sources)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let mut edited = Vec::new();
    for source in sources {
        if let Some(status) =
            music_publications::synchronize_conn(state, &mut tx, owner, source).await?
        {
            edited.push(status);
        }
    }
    music_entities::reconcile_conn(
        state,
        &mut tx,
        owner,
        &[
            (Kind::Album, album),
            (Kind::Artist, previous.artist_id.ok_or(ApiError::NotFound)?),
        ],
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    for status in &edited {
        music_publications::publish_update(state, Some(status)).await;
    }
    music_entities::get(state, Kind::Album, album, Some(owner)).await
}
pub async fn grant(
    state: &AppState,
    owner: i64,
    album: i64,
    member: i64,
    enabled: bool,
) -> Result<(), ApiError> {
    let previous = music_entities::get(state, Kind::Album, album, Some(owner)).await?;
    if previous.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    music_entities::lock_sources(&mut tx, owner, Kind::Album, album).await?;
    let current = music_entities::find_conn(&mut tx, Kind::Album, album)
        .await?
        .ok_or(ApiError::NotFound)?;
    music_entities::lock_entity(&mut tx, owner, Kind::Album, album, current.revision).await?;
    if enabled {
        if !music_assets::lock_recipient(&mut tx, member).await? {
            return Err(ApiError::Unprocessable(
                "Choose an active local member".into(),
            ));
        }
        let inserted:Option<i64>=sqlx::query_scalar("INSERT INTO music_album_grants(album_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id").bind(album).bind(member).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        if let Some(grant) = inserted {
            crate::music_shares::notify_grant(
                state,
                &mut tx,
                owner,
                member,
                crate::music_saved::Kind::Album,
                grant,
            )
            .await?;
        }
    } else {
        sqlx::query("DELETE FROM music_album_grants WHERE album_id=$1 AND account_id=$2")
            .bind(album)
            .bind(member)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
