//! Source tags follow external edits until an owner personalizes the track.
use crate::{AppState, error::ApiError, music_assets};
use plamenu_db::{DbError, status::Status};
pub(crate) async fn update(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    asset: i64,
    description: &music_assets::ImportDescription,
) -> Result<Option<Status>, ApiError> {
    let previous = music_assets::owned_conn(conn, owner, asset).await?;
    if !previous.indexed_original || previous.indexed_tags_revision != Some(previous.revision) {
        return Ok(None);
    }
    let input = &description.input;
    if previous.title == input.title.trim() && previous.metadata() == input.music {
        return Ok(None);
    }
    let (artist, album) = music_assets::entities(conn, owner, &input.music).await?;
    sqlx::query("UPDATE music_assets SET title=$3,artist_id=$4,album_id=$5,genre=$6,track_number=$7,disc_number=$8,musicbrainz_id=$9,release_year=$10,revision=revision+1,indexed_tags_revision=revision+1 WHERE id=$1 AND owner_id=$2")
        .bind(asset).bind(owner).bind(input.title.trim()).bind(artist).bind(album).bind(&input.music.genre).bind(input.music.track_number).bind(input.music.disc_number).bind(&input.music.musicbrainz_id).bind(input.music.release_year).execute(&mut *conn).await.map_err(DbError::from)?;
    if previous.visibility == "public" {
        let updated =
            crate::music_publications::synchronize_conn(state, conn, owner, asset).await?;
        let mut references = crate::music_entities::references(&previous);
        references.push((crate::music_entities::Kind::Artist, artist));
        if let Some(album) = album {
            references.push((crate::music_entities::Kind::Album, album));
        }
        crate::music_entities::reconcile_conn(state, conn, owner, &references).await?;
        return Ok(updated);
    }
    Ok(None)
}
