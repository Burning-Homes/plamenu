//! Status pins (Mastodon's `status_pins`): the statuses an account features
//! on its profile, served as the `featured` collection and federated via
//! `Add`/`Remove`.

use sqlx::PgPool;

use crate::status::Status;
use crate::{DbError, id};

/// Records a pin. Returns the row id, or `None` when the status was already
/// pinned (callers map that to Mastodon's "Duplicate record" 422).
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO status_pins (id, account_id, status_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, status_id) DO NOTHING
        RETURNING id
        "#,
        id::next(),
        account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Removes a pin; returns its row id if it existed.
pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM status_pins WHERE account_id = $1 AND status_id = $2 RETURNING id",
        account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Number of statuses an account has pinned (the local limit check).
pub async fn count_by_account(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM status_pins WHERE account_id = $1"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Which of `status_ids` the account has pinned.
pub async fn pinned_of(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT status_id AS "id!" FROM status_pins
           WHERE account_id = $1 AND status_id = ANY($2)"#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`pinned_of`] across a set of viewers in one query — `(viewer, status)`
/// pairs where the viewer pinned the status.
pub async fn pinned_of_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, status_id FROM status_pins
           WHERE account_id = ANY($1) AND status_id = ANY($2)"#,
        viewer_ids,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.status_id))
        .collect())
}

/// An account's pinned statuses, most recently pinned first (Mastodon's
/// `pinned_statuses` ordering; pin row ids are time-ordered snowflakes).
pub async fn pinned_statuses(pool: &PgPool, account_id: i64) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy, s.application_id,
               s.title, s.object_type, s.external_url
        FROM status_pins p
        JOIN statuses s ON s.id = p.status_id
        WHERE p.account_id = $1
          AND s.deleted_at IS NULL -- STUBFILTER
        ORDER BY p.id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    #[sqlx::test]
    async fn pin_lifecycle_and_ordering(pool: PgPool) {
        let alice = account::create_local(
            &pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let older = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>older</p>", "public", None),
        )
        .await
        .unwrap();
        let newer = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>newer</p>", "public", None),
        )
        .await
        .unwrap();

        // Pin the newer post first: ordering follows pin time, not post time.
        let first = create(&pool, alice.id, newer.id).await.unwrap();
        assert!(first.is_some());
        assert!(create(&pool, alice.id, older.id).await.unwrap().is_some());
        assert_eq!(
            create(&pool, alice.id, newer.id).await.unwrap(),
            None,
            "duplicate pin"
        );

        assert_eq!(count_by_account(&pool, alice.id).await.unwrap(), 2);
        assert_eq!(
            pinned_of(&pool, alice.id, &[older.id, newer.id])
                .await
                .unwrap()
                .len(),
            2
        );
        let pinned = pinned_statuses(&pool, alice.id).await.unwrap();
        assert_eq!(
            pinned.iter().map(|s| s.id).collect::<Vec<_>>(),
            [older.id, newer.id],
            "most recently pinned first"
        );

        assert_eq!(delete(&pool, alice.id, newer.id).await.unwrap(), first);
        assert_eq!(delete(&pool, alice.id, newer.id).await.unwrap(), None);

        // Deleting the status cascades the pin away.
        status::delete_local(&pool, older.id, alice.id)
            .await
            .unwrap();
        assert_eq!(count_by_account(&pool, alice.id).await.unwrap(), 0);
    }
}
