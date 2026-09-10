//! Per-account post read state used by clients with explicit read/unread verbs.

use std::collections::HashSet;

use sqlx::PgPool;

use crate::DbError;

/// Set `status_ids` read or unread for `account_id` in one statement.
///
/// IDs are deduplicated before reaching `PostgreSQL`. Re-marking a read post
/// refreshes neither the row nor its timestamp, making retries true no-ops.
pub async fn set_many(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
    read: bool,
) -> Result<(), DbError> {
    let mut ids = status_ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    if read {
        sqlx::query(
            r"
            INSERT INTO post_reads (account_id, status_id)
            SELECT $1, id FROM unnest($2::bigint[]) AS input(id)
            ON CONFLICT (account_id, status_id) DO NOTHING
            ",
        )
        .bind(account_id)
        .bind(&ids)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM post_reads WHERE account_id = $1 AND status_id = ANY($2)")
            .bind(account_id)
            .bind(&ids)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// The subset of `status_ids` that `account_id` has marked read.
pub async fn read_ids(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<HashSet<i64>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT status_id FROM post_reads WHERE account_id = $1 AND status_id = ANY($2)",
    )
    .bind(account_id)
    .bind(status_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Whether one post is marked read.
pub async fn contains(pool: &PgPool, account_id: i64, status_id: i64) -> Result<bool, DbError> {
    Ok(read_ids(pool, account_id, &[status_id])
        .await?
        .contains(&status_id))
}

#[cfg(test)]
mod tests {
    use super::{contains, read_ids, set_many};
    use crate::PgPool;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

    #[sqlx::test(migrations = "./migrations")]
    async fn batch_read_state_is_idempotent_and_reversible(pool: PgPool) {
        let account = account::create_local(
            &pool,
            NewLocalAccount {
                username: "reader",
                display_name: "Reader",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let first = status::create_local(
            &pool,
            NewLocalStatus::new(account.id, "first", "public", None),
        )
        .await
        .unwrap();
        let second = status::create_local(
            &pool,
            NewLocalStatus::new(account.id, "second", "public", None),
        )
        .await
        .unwrap();

        set_many(&pool, account.id, &[first.id, second.id, first.id], true)
            .await
            .unwrap();
        set_many(&pool, account.id, &[first.id], true)
            .await
            .unwrap();
        assert_eq!(
            read_ids(&pool, account.id, &[first.id, second.id])
                .await
                .unwrap()
                .len(),
            2
        );

        set_many(&pool, account.id, &[first.id], false)
            .await
            .unwrap();
        assert!(!contains(&pool, account.id, first.id).await.unwrap());
        assert!(contains(&pool, account.id, second.id).await.unwrap());
    }
}
