//! Wall-clock ⇄ instant conversions through Postgres's IANA tz database.
//! The server links no tz rules of its own (its `time_zones` module is just
//! the identifier inventory users pick from), while Postgres ships the full,
//! maintained database — so the handful of places that need a real conversion
//! (the web composer's schedule field, the scheduled-posts page) ask it.

use sqlx::PgPool;
use time::{OffsetDateTime, PrimitiveDateTime};

use crate::DbError;

/// Interprets `local` as a wall-clock reading in `zone` and returns the
/// instant it names (`timestamp AT TIME ZONE zone`). During a DST transition
/// Postgres resolves ambiguous or skipped readings deterministically. An
/// unknown zone identifier is a query error — validate against the server's
/// zone inventory before calling.
pub async fn local_to_utc(
    pool: &PgPool,
    local: PrimitiveDateTime,
    zone: &str,
) -> Result<OffsetDateTime, DbError> {
    let instant = sqlx::query_scalar("SELECT $1::timestamp AT TIME ZONE $2")
        .bind(local)
        .bind(zone)
        .fetch_one(pool)
        .await?;
    Ok(instant)
}

/// Renders each instant as the wall-clock reading in `zone`, preserving input
/// order — one round-trip for a whole page of timestamps.
pub async fn utc_to_local(
    pool: &PgPool,
    instants: &[OffsetDateTime],
    zone: &str,
) -> Result<Vec<PrimitiveDateTime>, DbError> {
    if instants.is_empty() {
        return Ok(Vec::new());
    }
    let locals = sqlx::query_scalar(
        "SELECT t AT TIME ZONE $2 \
         FROM unnest($1::timestamptz[]) WITH ORDINALITY AS u(t, ord) \
         ORDER BY ord",
    )
    .bind(instants)
    .bind(zone)
    .fetch_all(pool)
    .await?;
    Ok(locals)
}
