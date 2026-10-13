//! Typed snapshots are bound to a known, verified publication and canonical Track.
use super::document::{Document, Kind};
use crate::error::ApiError;
use plamenu_db::DbError;
use time::OffsetDateTime;

pub async fn store(
    conn: &mut sqlx::PgConnection,
    doc: &Document,
    controller: i64,
    started: OffsetDateTime,
) -> Result<(), ApiError> {
    if doc.kind != Kind::Track || doc.proof_kind != Kind::Audio || doc.proof_uri == doc.uri {
        return Ok(());
    }
    sqlx::query("INSERT INTO music_remote_track_proofs(proof_uri,uri,controller_id,name,summary,details,parent_uri,fetched_at,changed_at) SELECT $1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp() FROM music_remote_resources p JOIN music_remote_resources target ON target.uri=$2 AND target.controller_id=$3 WHERE p.uri=$1 AND p.kind='audio' AND p.media_of=$2 AND p.controller_id=$3 AND p.unavailable_at IS NULL ON CONFLICT(proof_uri) DO UPDATE SET uri=EXCLUDED.uri,controller_id=EXCLUDED.controller_id,name=EXCLUDED.name,summary=EXCLUDED.summary,details=EXCLUDED.details,parent_uri=EXCLUDED.parent_uri,fetched_at=EXCLUDED.fetched_at,changed_at=clock_timestamp() WHERE music_remote_track_proofs.controller_id=EXCLUDED.controller_id AND music_remote_track_proofs.changed_at<=EXCLUDED.fetched_at")
        .bind(&doc.proof_uri).bind(&doc.uri).bind(controller).bind(&doc.name).bind(&doc.summary).bind(&doc.details).bind(&doc.parent).bind(started).execute(conn).await.map_err(DbError::from)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Snapshot {
    uri: String,
    proof_uri: String,
    proof_kind: String,
    name: String,
    summary: String,
    #[sqlx(try_from = "super::details::Details")]
    details: sqlx::types::Json<super::details::Details>,
    parent_uri: Option<String>,
    fetched_at: OffsetDateTime,
}
pub(super) async fn decorate(
    pool: &sqlx::PgPool,
    resources: &mut [super::Resource],
    viewer: Option<i64>,
) -> Result<(), ApiError> {
    let uris = resources
        .iter()
        .filter(|row| row.kind == "track" && row.proof_uri != row.uri)
        .map(|row| row.uri.clone())
        .collect::<Vec<_>>();
    if uris.is_empty() {
        return Ok(());
    }
    let rows:Vec<Snapshot>=sqlx::query_as(concat!("SELECT DISTINCT ON(snapshot.uri) snapshot.* FROM music_remote_track_proofs snapshot JOIN music_remote_resources resource ON resource.uri=snapshot.uri WHERE snapshot.uri=ANY($1) AND snapshot.controller_id=resource.controller_id AND ",super::live_proof!("snapshot")," ORDER BY snapshot.uri,(snapshot.proof_uri=resource.proof_uri) DESC,snapshot.fetched_at DESC,snapshot.proof_uri"))
        .bind(&uris).bind(viewer).fetch_all(pool).await.map_err(DbError::from)?;
    let mut snapshots = rows
        .into_iter()
        .map(|row| (row.uri.clone(), row))
        .collect::<std::collections::HashMap<_, _>>();
    for resource in resources {
        if let Some(snapshot) = snapshots.remove(&resource.uri) {
            resource.proof_uri = snapshot.proof_uri;
            resource.proof_kind = snapshot.proof_kind;
            resource.name = snapshot.name;
            resource.summary = snapshot.summary;
            resource.details = snapshot.details;
            resource.parent_uri = snapshot.parent_uri;
            resource.fetched_at = snapshot.fetched_at;
        }
    }
    Ok(())
}
