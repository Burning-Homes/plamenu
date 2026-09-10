//! Free-form moderator notes attached to an account (Mastodon's
//! `AccountModerationNote`). Unlike a strike ([`crate::account_warning`]) a
//! note records no action — it is the moderators' shared scratchpad about an
//! account. The schema landed schema-only; the
//! write surface is the admin web dashboard.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A moderator note about an account, with its author resolved for display.
#[derive(Debug, Clone)]
pub struct ModerationNote {
    pub id: i64,
    pub content: String,
    /// The note's author, `None` once that account is gone (`ON DELETE SET
    /// NULL`).
    pub account_id: Option<i64>,
    pub target_account_id: i64,
    pub created_at: OffsetDateTime,
}

/// Records a moderator note, returning the stored row.
pub async fn create(
    pool: &PgPool,
    author_account_id: i64,
    target_account_id: i64,
    content: &str,
) -> Result<ModerationNote, DbError> {
    let note = sqlx::query_as!(
        ModerationNote,
        r#"
        INSERT INTO account_moderation_notes
            (id, content, account_id, target_account_id)
        VALUES ($1, $2, $3, $4)
        RETURNING id, content, account_id, target_account_id, created_at
        "#,
        id::next(),
        content,
        author_account_id,
        target_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(note)
}

/// The notes recorded against an account, oldest first (a chronological
/// conversation, matching Mastodon's `account.targeted_moderation_notes`).
pub async fn for_target(
    pool: &PgPool,
    target_account_id: i64,
) -> Result<Vec<ModerationNote>, DbError> {
    let notes = sqlx::query_as!(
        ModerationNote,
        r#"
        SELECT id, content, account_id, target_account_id, created_at
        FROM account_moderation_notes
        WHERE target_account_id = $1
        ORDER BY id ASC
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(notes)
}

/// Deletes a note by id, returning whether a row was removed.
pub async fn delete(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM account_moderation_notes WHERE id = $1", id,)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
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
    async fn notes_accumulate_chronologically_and_delete(pool: PgPool) {
        let mod_acct = local(&pool, "mod").await;
        let target = local(&pool, "target").await;

        assert!(for_target(&pool, target).await.unwrap().is_empty());

        let first = create(&pool, mod_acct, target, "first note").await.unwrap();
        let _second = create(&pool, mod_acct, target, "second note")
            .await
            .unwrap();

        // Oldest first, with the author resolved.
        let notes = for_target(&pool, target).await.unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, "first note");
        assert_eq!(notes[0].account_id, Some(mod_acct));
        assert_eq!(notes[1].content, "second note");

        // Deleting one leaves the other; deleting a missing id is a no-op.
        assert!(delete(&pool, first.id).await.unwrap());
        assert!(!delete(&pool, first.id).await.unwrap());
        let remaining = for_target(&pool, target).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "second note");
    }

    #[sqlx::test]
    async fn deleting_the_target_cascades(pool: PgPool) {
        let mod_acct = local(&pool, "mod").await;
        let target = local(&pool, "target").await;
        create(&pool, mod_acct, target, "note").await.unwrap();

        account::delete_by_id(&pool, target).await.unwrap();
        // The note went with the target (ON DELETE CASCADE).
        assert!(for_target(&pool, target).await.unwrap().is_empty());
    }
}
