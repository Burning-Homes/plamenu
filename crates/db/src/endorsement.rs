//! Profile endorsements — Mastodon's `account_pins`. An account pins other
//! accounts to its public profile; the list is surfaced through the
//! Endorsements API and the `endorsed` flag on the Relationship entity.
//! Purely local; never federates.

use sqlx::PgPool;

use crate::{DbError, id};

/// One endorsement row: its own id (the keyset-pagination cursor) and the
/// endorsed account.
pub struct Endorsement {
    pub id: i64,
    pub target_account_id: i64,
}

/// Pins `target_account_id` to `account_id`'s profile. Idempotent, like
/// Mastodon's endorsement endpoint (unique on the pair).
pub async fn endorse(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO account_endorsements (id, account_id, target_account_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, target_account_id) DO NOTHING
        "#,
        id::next(),
        account_id,
        target_account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Unpins `target_account_id` from `account_id`'s profile.
pub async fn unendorse(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM account_endorsements WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether `account_id` endorses `target_account_id` — the `endorsed` flag of
/// the Relationship entity.
pub async fn exists(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one!"
        FROM account_endorsements
        WHERE account_id = $1 AND target_account_id = $2
        "#,
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Of `target_ids`, the subset `account_id` endorses, in one query — the
/// batched form of [`exists`].
pub async fn exists_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT target_account_id AS "id!"
           FROM account_endorsements
           WHERE account_id = $1 AND target_account_id = ANY($2)"#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `account_id`'s endorsements, most recently pinned first, keyset-paginated
/// by the endorsement id like Mastodon's endorsements endpoint.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Endorsement>, DbError> {
    let rows = sqlx::query_as!(
        Endorsement,
        r#"
        SELECT id AS "id!", target_account_id AS "target_account_id!"
        FROM account_endorsements
        WHERE account_id = $1
          AND ($2::bigint IS NULL OR id < $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn endorse_lists_and_clears(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;

        assert!(!exists(&pool, alice, bob).await.unwrap());
        assert!(list(&pool, alice, None, None, 40).await.unwrap().is_empty());

        endorse(&pool, alice, bob).await.unwrap();
        endorse(&pool, alice, carol).await.unwrap();
        // Idempotent.
        endorse(&pool, alice, bob).await.unwrap();

        assert!(exists(&pool, alice, bob).await.unwrap());
        let listed = list(&pool, alice, None, None, 40).await.unwrap();
        // Most recently pinned first.
        assert_eq!(
            listed
                .iter()
                .map(|e| e.target_account_id)
                .collect::<Vec<_>>(),
            vec![carol, bob]
        );
        // Directional — bob endorses nobody.
        assert!(list(&pool, bob, None, None, 40).await.unwrap().is_empty());

        unendorse(&pool, alice, bob).await.unwrap();
        assert!(!exists(&pool, alice, bob).await.unwrap());
        assert_eq!(list(&pool, alice, None, None, 40).await.unwrap().len(), 1);
    }
}
