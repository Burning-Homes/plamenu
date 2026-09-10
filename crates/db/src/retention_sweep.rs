//! Durable retention-sweep observability: each retention sweep
//! records its successful completion here, so "when did retention last run,
//! and what did it do?" is a query — surfaced on the admin dashboard — rather
//! than a log-rotation archaeology exercise. Only *successful* passes record;
//! a sweep that failed to even select its candidates leaves the previous
//! timestamp standing, which is exactly the staleness an operator needs to
//! see.

use time::OffsetDateTime;

use crate::{DbError, PgPool};

/// One sweep's durable status row.
#[derive(Debug, Clone)]
pub struct SweepStatus {
    pub name: String,
    pub last_success_at: OffsetDateTime,
    pub last_swept: i64,
    pub last_retained: i64,
}

/// Records a successful retention pass for `name` (upsert, stamped `now()`).
/// `swept` is what it removed; `retained` is what it deliberately kept for a
/// later retry (an archive whose stored file could not be deleted yet).
pub async fn record(pool: &PgPool, name: &str, swept: u64, retained: u64) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO retention_sweeps (name, last_success_at, last_swept, last_retained)
         VALUES ($1, now(), $2, $3)
         ON CONFLICT (name) DO UPDATE SET
             last_success_at = excluded.last_success_at,
             last_swept = excluded.last_swept,
             last_retained = excluded.last_retained",
        name,
        i64::try_from(swept).unwrap_or(i64::MAX),
        i64::try_from(retained).unwrap_or(i64::MAX),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Every sweep's latest status, alphabetically.
pub async fn statuses(pool: &PgPool) -> Result<Vec<SweepStatus>, DbError> {
    let rows = sqlx::query_as!(
        SweepStatus,
        "SELECT name, last_success_at, last_swept, last_retained
         FROM retention_sweeps
         ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn record_upserts_and_statuses_reads_back(pool: PgPool) {
        record(&pool, "account_archives", 3, 1).await.unwrap();
        record(&pool, "bulk_imports", 0, 0).await.unwrap();
        // A later pass replaces the row rather than accumulating history.
        record(&pool, "account_archives", 5, 0).await.unwrap();

        let statuses = statuses(&pool).await.unwrap();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].name, "account_archives");
        assert_eq!(statuses[0].last_swept, 5);
        assert_eq!(statuses[0].last_retained, 0);
        assert_eq!(statuses[1].name, "bulk_imports");
        let age = OffsetDateTime::now_utc() - statuses[0].last_success_at;
        assert!(age.whole_seconds() < 60, "stamped now");
    }
}
