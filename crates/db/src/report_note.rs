//! Free-form moderator notes attached to a report (Mastodon's `ReportNote`).
//! Like [`crate::account_moderation_note`] this is the moderators' shared
//! scratchpad — here scoped to a single report rather than an account. The
//! schema landed schema-only (migration 0054); the write surface is
//! the admin web dashboard.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A moderator note about a report, with its author resolved for display.
#[derive(Debug, Clone)]
pub struct ReportNote {
    pub id: i64,
    pub content: String,
    /// The note's author, `None` once that account is gone (`ON DELETE SET
    /// NULL`).
    pub account_id: Option<i64>,
    pub report_id: i64,
    pub created_at: OffsetDateTime,
}

/// Records a report note, returning the stored row.
pub async fn create(
    pool: &PgPool,
    author_account_id: i64,
    report_id: i64,
    content: &str,
) -> Result<ReportNote, DbError> {
    let note = sqlx::query_as!(
        ReportNote,
        r#"
        INSERT INTO report_notes (id, content, account_id, report_id)
        VALUES ($1, $2, $3, $4)
        RETURNING id, content, account_id, report_id, created_at
        "#,
        id::next(),
        content,
        author_account_id,
        report_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(note)
}

/// The notes recorded against a report, oldest first (a chronological
/// conversation, matching Mastodon's `report.notes`).
pub async fn for_report(pool: &PgPool, report_id: i64) -> Result<Vec<ReportNote>, DbError> {
    let notes = sqlx::query_as!(
        ReportNote,
        r#"
        SELECT id, content, account_id, report_id, created_at
        FROM report_notes
        WHERE report_id = $1
        ORDER BY id ASC
        "#,
        report_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(notes)
}

/// Deletes a note by id, returning whether a row was removed.
pub async fn delete(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM report_notes WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::report::{self, NewReport};

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

    async fn a_report(pool: &PgPool, reporter: i64, target: i64) -> i64 {
        report::create(
            pool,
            NewReport {
                account_id: reporter,
                target_account_id: target,
                status_ids: &[],
                comment: "spam",
                category: "spam",
                forwarded: None,
                rule_ids: None,
                uri: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn notes_accumulate_chronologically_and_delete(pool: PgPool) {
        let mod_acct = local(&pool, "mod").await;
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carol").await;
        let report = a_report(&pool, reporter, target).await;

        assert!(for_report(&pool, report).await.unwrap().is_empty());

        let first = create(&pool, mod_acct, report, "first note").await.unwrap();
        let _second = create(&pool, mod_acct, report, "second note")
            .await
            .unwrap();

        // Oldest first, with the author resolved.
        let notes = for_report(&pool, report).await.unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, "first note");
        assert_eq!(notes[0].account_id, Some(mod_acct));
        assert_eq!(notes[1].content, "second note");

        // Deleting one leaves the other; deleting a missing id is a no-op.
        assert!(delete(&pool, first.id).await.unwrap());
        assert!(!delete(&pool, first.id).await.unwrap());
        let remaining = for_report(&pool, report).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "second note");
    }

    #[sqlx::test]
    async fn deleting_the_report_cascades(pool: PgPool) {
        let mod_acct = local(&pool, "mod").await;
        let reporter = local(&pool, "alice").await;
        let target = local(&pool, "carol").await;
        let report = a_report(&pool, reporter, target).await;
        create(&pool, mod_acct, report, "note").await.unwrap();

        sqlx::query!("DELETE FROM reports WHERE id = $1", report)
            .execute(&pool)
            .await
            .unwrap();
        // The note went with the report (ON DELETE CASCADE).
        assert!(for_report(&pool, report).await.unwrap().is_empty());
    }
}
