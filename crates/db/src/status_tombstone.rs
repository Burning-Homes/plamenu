//! Short-lived tombstones for deleted local statuses. Plamenu still
//! hard-deletes the status row on delete; a tombstone recorded here lets the
//! Note's `ActivityPub` id keep resolving to `410 Gone` while the federated
//! `Delete` propagates, then a budgeted sweep prunes it.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// A recorded tombstone: enough to answer the AP object GET with a `Tombstone`.
#[derive(Debug, Clone)]
pub struct StatusTombstone {
    pub status_id: i64,
    pub uri: String,
    pub deleted_at: OffsetDateTime,
}

/// Records that a local status was deleted. Idempotent: re-recording the same
/// id (e.g. a retried delete) leaves the original `deleted_at` intact so the
/// window is measured from the first deletion.
pub async fn record<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
    uri: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO status_tombstones (status_id, account_id, uri)
        VALUES ($1, $2, $3)
        ON CONFLICT (status_id) DO NOTHING
        "#,
        status_id,
        account_id,
        uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The tombstone for a deleted status, if one is still within the window (the
/// sweep removes expired rows, so mere presence means "still gone recently").
pub async fn find(pool: &PgPool, status_id: i64) -> Result<Option<StatusTombstone>, DbError> {
    let row = sqlx::query_as!(
        StatusTombstone,
        r#"
        SELECT status_id, uri, deleted_at
        FROM status_tombstones
        WHERE status_id = $1
        "#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Prunes tombstones older than `days`, returning how many were removed. Cheap
/// (single indexed range delete); the caller runs it on the maintenance sweep.
pub async fn prune_expired(pool: &PgPool, days: i32) -> Result<u64, DbError> {
    let done = sqlx::query!(
        "DELETE FROM status_tombstones WHERE deleted_at < now() - make_interval(days => $1)",
        days,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{NewLocalAccount, create_local};

    async fn account(pool: &PgPool) -> i64 {
        create_local(
            pool,
            NewLocalAccount {
                username: "ghost",
                display_name: "",
                note: "",
                public_key_pem: "k",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn record_is_idempotent_and_find_round_trips(pool: PgPool) {
        let account_id = account(&pool).await;
        let uri = "https://plamenu.local/users/ghost/statuses/42";
        record(&pool, 42, account_id, uri).await.unwrap();
        // A retried delete keeps the first deleted_at.
        record(&pool, 42, account_id, "https://other")
            .await
            .unwrap();

        let found = find(&pool, 42).await.unwrap().unwrap();
        assert_eq!(found.status_id, 42);
        assert_eq!(found.uri, uri);
        assert!(find(&pool, 99).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn prune_removes_only_expired(pool: PgPool) {
        let account_id = account(&pool).await;
        record(&pool, 1, account_id, "u1").await.unwrap();
        // Age it past the window.
        sqlx::query!("UPDATE status_tombstones SET deleted_at = now() - interval '10 days' WHERE status_id = 1")
            .execute(&pool)
            .await
            .unwrap();
        record(&pool, 2, account_id, "u2").await.unwrap();

        assert_eq!(prune_expired(&pool, 7).await.unwrap(), 1);
        assert!(find(&pool, 1).await.unwrap().is_none());
        assert!(find(&pool, 2).await.unwrap().is_some());
    }
}
