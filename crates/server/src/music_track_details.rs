//! Track notes, credits and exact album assignment use the retained source fence.
use crate::{
    AppState,
    compose::PostFormat,
    error::ApiError,
    music_assets::{self, Asset},
};
use plamenu_db::DbError;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Details {
    pub revision: String,
    #[serde(default)]
    pub description: String,
    pub content_type: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub extra_artists: Vec<String>,
}
#[derive(Debug, Deserialize)]
pub struct Album {
    pub revision: String,
    pub album_id: Option<String>,
}
fn revision(value: &str) -> Result<i64, ApiError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|r| *r > 0)
        .ok_or_else(|| ApiError::Unprocessable("Track revision is required".into()))
}
/// Credits are names, not actor identities. Preserve their display case.
fn names(values: &[String]) -> Result<Vec<String>, ApiError> {
    if values.len() > 20 {
        return Err(ApiError::Unprocessable(
            "Use at most 20 additional artist names".into(),
        ));
    }
    let mut result = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() || value.chars().count() > 200 || value.chars().any(char::is_control) {
            return Err(ApiError::Unprocessable(
                "Invalid additional artist name".into(),
            ));
        }
        if !result
            .iter()
            .any(|s: &String| s.eq_ignore_ascii_case(value))
        {
            result.push(value.to_owned());
        }
    }
    Ok(result)
}
pub(crate) fn validate_description(text: &str, content_type: &str) -> Result<(), ApiError> {
    if text.chars().count() > 50_000
        || text.contains('\0')
        || !PostFormat::ADVERTISED.contains(&content_type)
    {
        return Err(ApiError::Unprocessable(
            "Invalid track description or format".into(),
        ));
    }
    Ok(())
}
async fn lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    source: i64,
    expected: i64,
) -> Result<Asset, ApiError> {
    music_assets::lock_owner(conn, owner).await?;
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE",
    )
    .bind(source)
    .bind(owner)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if found.ok_or(ApiError::NotFound)? != expected {
        return Err(ApiError::Conflict(
            "Track changed; reload before editing".into(),
        ));
    }
    music_assets::owned_conn(conn, owner, source).await
}
pub async fn update(
    state: &AppState,
    owner: i64,
    source: i64,
    input: &Details,
) -> Result<Asset, ApiError> {
    let expected = revision(&input.revision)?;
    let tags = crate::music_entities::terms(&input.tags)?;
    let credits = names(&input.extra_artists)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let previous = lock(&mut tx, owner, source, expected).await?;
    let content_type = input
        .content_type
        .as_deref()
        .unwrap_or(&previous.description_type);
    validate_description(&input.description, content_type)?;
    if previous.description == input.description
        && previous.description_type == content_type
        && previous.tags == tags
        && previous.extra_artists == credits
    {
        tx.commit().await.map_err(DbError::from)?;
        return music_assets::get(state, source, Some(owner)).await;
    }
    sqlx::query("UPDATE music_assets SET description=$3,description_type=$4,tags=$5,extra_artists=$6,revision=revision+1 WHERE id=$1 AND owner_id=$2").bind(source).bind(owner).bind(&input.description).bind(content_type).bind(tags).bind(credits).execute(&mut *tx).await.map_err(DbError::from)?;
    let updated =
        crate::music_publications::synchronize_conn(state, &mut tx, owner, source).await?;
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_update(state, updated.as_ref()).await;
    music_assets::get(state, source, Some(owner)).await
}
pub async fn set_album(
    state: &AppState,
    owner: i64,
    source: i64,
    input: &Album,
) -> Result<Asset, ApiError> {
    let expected = revision(&input.revision)?;
    let album = input
        .album_id
        .as_deref()
        .map(|s| {
            s.parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| ApiError::Unprocessable("Invalid album".into()))
        })
        .transpose()?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let previous = lock(&mut tx, owner, source, expected).await?;
    let mut references = crate::music_entities::references(&previous);
    let (year, genre) = if let Some(id) = album {
        sqlx::query_scalar::<_, i64>(
            "SELECT id FROM music_albums WHERE id=$1 AND owner_id=$2 FOR SHARE",
        )
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)?;
        let entity =
            crate::music_entities::find_conn(&mut tx, crate::music_entities::Kind::Album, id)
                .await?
                .ok_or(ApiError::NotFound)?;
        references.push((crate::music_entities::Kind::Album, id));
        if let Some(artist) = entity.artist_id {
            references.push((crate::music_entities::Kind::Artist, artist));
        }
        (entity.release_year, entity.genres.join("; "))
    } else {
        (None, String::new())
    };
    if previous.album_id == album {
        tx.commit().await.map_err(DbError::from)?;
        return music_assets::get(state, source, Some(owner)).await;
    }
    sqlx::query("UPDATE music_assets SET album_id=$3,release_year=COALESCE(release_year,$4),genre=CASE WHEN genre='' THEN $5 ELSE genre END,revision=revision+1 WHERE id=$1 AND owner_id=$2").bind(source).bind(owner).bind(album).bind(year).bind(genre).execute(&mut *tx).await.map_err(DbError::from)?;
    let updated =
        crate::music_publications::synchronize_conn(state, &mut tx, owner, source).await?;
    if previous.visibility == "public" {
        crate::music_entities::reconcile_conn(state, &mut tx, owner, &references).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_update(state, updated.as_ref()).await;
    music_assets::get(state, source, Some(owner)).await
}
