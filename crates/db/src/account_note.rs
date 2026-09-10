//! Private notes an account keeps about another — Mastodon's `account_notes`.
//! Purely local; the text rides on the Relationship entity's `note` field.

use sqlx::PgPool;

use crate::{DbError, id};

/// Sets `account_id`'s note about `target_account_id`. An empty comment
/// clears it, like Mastodon's personal-notes endpoint (blank clears it).
pub async fn set(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
    comment: &str,
) -> Result<(), DbError> {
    if comment.is_empty() {
        sqlx::query!(
            "DELETE FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
            account_id,
            target_account_id,
        )
        .execute(pool)
        .await?;
        return Ok(());
    }
    sqlx::query!(
        r#"
        INSERT INTO account_notes (id, account_id, target_account_id, comment)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, target_account_id) DO UPDATE SET
            comment = EXCLUDED.comment
        "#,
        id::next(),
        account_id,
        target_account_id,
        comment,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `account_id`'s note about `target_account_id` — the empty string when
/// there is none, matching the Relationship serializer's default.
pub async fn get(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<String, DbError> {
    let comment = sqlx::query_scalar!(
        "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(comment.unwrap_or_default())
}

/// The notes `account_id` has written about each of `target_ids`, keyed by
/// target, in one query — the batched form of [`get`]. Targets with no note
/// are absent (treat as the empty string).
pub async fn get_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<std::collections::HashMap<i64, String>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT target_account_id AS "id!", comment
           FROM account_notes
           WHERE account_id = $1 AND target_account_id = ANY($2)"#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.id, row.comment)).collect())
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
    async fn set_updates_and_clears(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        assert_eq!(get(&pool, alice, bob).await.unwrap(), "");

        set(&pool, alice, bob, "old friend").await.unwrap();
        assert_eq!(get(&pool, alice, bob).await.unwrap(), "old friend");

        // Re-setting overwrites; it is the viewer's own private note.
        set(&pool, alice, bob, "blocked me once").await.unwrap();
        assert_eq!(get(&pool, alice, bob).await.unwrap(), "blocked me once");
        // The note is directional — bob has none about alice.
        assert_eq!(get(&pool, bob, alice).await.unwrap(), "");

        // An empty comment clears it.
        set(&pool, alice, bob, "").await.unwrap();
        assert_eq!(get(&pool, alice, bob).await.unwrap(), "");
    }
}
