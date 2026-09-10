//! Status edit history, Mastodon's `status_edits` model: a baseline snapshot
//! of the original is created on first edit, then a snapshot of the new state
//! after every edit. Statuses never edited have no rows.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StatusEdit {
    pub id: i64,
    pub status_id: i64,
    pub account_id: i64,
    /// Rendered HTML of this version.
    pub content: String,
    /// Raw source of this version (local statuses only).
    pub text: String,
    pub spoiler_text: String,
    pub sensitive: bool,
    pub media_ids: Vec<i64>,
    pub created_at: OffsetDateTime,
}

/// One status version to record.
#[derive(Debug)]
pub struct NewStatusEdit<'a> {
    pub status_id: i64,
    pub account_id: i64,
    pub content: &'a str,
    pub text: &'a str,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub media_ids: &'a [i64],
    /// The status' `created_at` for baseline rows, the edit time otherwise.
    pub created_at: OffsetDateTime,
}

pub async fn snapshot<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    new: NewStatusEdit<'_>,
) -> Result<StatusEdit, DbError> {
    let edit = sqlx::query_as!(
        StatusEdit,
        r#"
        INSERT INTO status_edits (id, status_id, account_id, content, text,
                                  spoiler_text, sensitive, media_ids, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, status_id, account_id, content, text, spoiler_text, sensitive,
                  media_ids, created_at
        "#,
        id::next(),
        new.status_id,
        new.account_id,
        new.content,
        new.text,
        new.spoiler_text,
        new.sensitive,
        new.media_ids,
        new.created_at,
    )
    .fetch_one(pool)
    .await?;
    Ok(edit)
}

/// All recorded versions of a status, oldest first. Empty for never-edited
/// statuses (the API synthesizes the single current version).
pub async fn for_status(pool: &PgPool, status_id: i64) -> Result<Vec<StatusEdit>, DbError> {
    let edits = sqlx::query_as!(
        StatusEdit,
        r#"
        SELECT id, status_id, account_id, content, text, spoiler_text, sensitive,
               media_ids, created_at
        FROM status_edits
        WHERE status_id = $1
        ORDER BY id
        "#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(edits)
}

/// Whether a status has recorded versions (i.e. has been edited).
pub async fn any_for_status<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM status_edits WHERE status_id = $1 LIMIT 1"#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    #[sqlx::test]
    async fn snapshots_are_ordered_and_cascade(pool: PgPool) {
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
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>v2</p>", "public", None),
        )
        .await
        .unwrap();

        assert!(!any_for_status(&pool, post.id).await.unwrap());
        let version = |content: &'static str, media_ids: &'static [i64]| NewStatusEdit {
            status_id: post.id,
            account_id: alice.id,
            content,
            text: "",
            spoiler_text: "",
            sensitive: false,
            media_ids,
            created_at: post.created_at,
        };
        snapshot(&pool, version("<p>v1</p>", &[])).await.unwrap();
        snapshot(&pool, version("<p>v2</p>", &[7])).await.unwrap();
        assert!(any_for_status(&pool, post.id).await.unwrap());

        let versions = for_status(&pool, post.id).await.unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>(),
            ["<p>v1</p>", "<p>v2</p>"]
        );
        assert_eq!(versions[1].media_ids, [7]);

        // Deleting the status removes its history.
        status::delete_local(&pool, post.id, alice.id)
            .await
            .unwrap();
        assert!(for_status(&pool, post.id).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn source_text_is_stored_and_editable(pool: PgPool) {
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
        let post = status::create_local(
            &pool,
            status::NewLocalStatus {
                text: "hello",
                ..status::NewLocalStatus::new(alice.id, "<p>hello</p>", "public", None)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            status::source_of(&pool, post.id)
                .await
                .unwrap()
                .map(|s| (s.text, s.content_type)),
            Some(("hello".to_owned(), "text/plain".to_owned()))
        );

        let edited = status::apply_local_edit(
            &pool,
            post.id,
            alice.id,
            status::LocalStatusEdit {
                title: None,
                content: "<p>bye</p>",
                text: "bye",
                content_type: "text/markdown",
                spoiler_text: "cw",
                sensitive: true,
                language: Some("en"),
                edited_at: OffsetDateTime::now_utc(),
                quote_approval_policy: post.quote_approval_policy,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(edited.content, "<p>bye</p>");
        assert_eq!(edited.spoiler_text, "cw");
        assert!(edited.sensitive && edited.edited_at.is_some());
        assert_eq!(
            status::source_of(&pool, post.id)
                .await
                .unwrap()
                .map(|s| (s.text, s.content_type)),
            Some(("bye".to_owned(), "text/markdown".to_owned()))
        );

        // Scoped to the owner; remote/foreign rows are untouchable.
        assert!(
            status::apply_local_edit(
                &pool,
                post.id,
                alice.id + 1,
                status::LocalStatusEdit {
                    title: None,
                    content: "<p>hax</p>",
                    text: "hax",
                    content_type: "text/plain",
                    spoiler_text: "",
                    sensitive: false,
                    language: None,
                    edited_at: OffsetDateTime::now_utc(),
                    quote_approval_policy: post.quote_approval_policy,
                },
            )
            .await
            .unwrap()
            .is_none()
        );
    }
}
