//! The admin audit log (Mastodon's `Admin::ActionLog`) — one row per
//! moderator/admin mutation, whether it came through the web dashboard or the
//! admin REST API. Rows are append-only; nothing ever updates or deletes them.
//! Both the *target* (via `human_identifier`) and the *acting moderator* (via
//! `actor_username`) are snapshotted at write time, so a row still reads
//! correctly after either account is hard-deleted — the actor reference is
//! `ON DELETE SET NULL`, not cascade, precisely so deleting a moderator does
//! not erase the trail of what they did.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// One recorded admin action, carrying the acting moderator's handle
/// snapshotted at write time (not joined, so it survives their deletion).
#[derive(Debug, Clone)]
pub struct AdminActionLog {
    pub id: i64,
    /// The acting moderator's account id, or `None` once that account has been
    /// hard-deleted. The row itself survives (`ON DELETE SET NULL`); the
    /// display name comes from the [`Self::account_username`] snapshot below,
    /// not from a join that would have dropped it.
    pub account_id: Option<i64>,
    /// The acting moderator's username, snapshotted at write time so it
    /// outlives their account (moderators are always local).
    pub account_username: String,
    /// The verb, Mastodon's vocabulary: `suspend`, `resolve`, `create`, …
    pub action: String,
    /// The target's kind, Mastodon's class names: `Account`, `User`,
    /// `Report`, `DomainBlock`, …
    pub target_type: String,
    pub target_id: i64,
    pub human_identifier: String,
    pub permalink: Option<String>,
    pub created_at: OffsetDateTime,
}

/// The fields of a fresh log line.
#[derive(Debug, Default)]
pub struct NewActionLog<'a> {
    pub account_id: i64,
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_id: i64,
    pub human_identifier: &'a str,
    pub permalink: Option<&'a str>,
}

/// Filters for [`list`]; `None` fields match everything.
#[derive(Debug, Default)]
pub struct LogFilter {
    /// Only actions performed by this moderator.
    pub account_id: Option<i64>,
    /// Only this action verb.
    pub action: Option<String>,
    /// Only actions on this kind of target.
    pub target_type: Option<String>,
    /// Only rows older than this id (pagination cursor).
    pub max_id: Option<i64>,
    pub limit: i64,
}

/// Appends a line to the audit log.
pub async fn record(pool: &PgPool, new: NewActionLog<'_>) -> Result<(), DbError> {
    insert(pool, new).await
}

/// Appends a line to the audit log inside an open transaction, so a
/// destructive mutation and its audit line commit or roll back as one unit —
/// there is never a hard deletion without its audit trail, nor a trail entry
/// for a deletion that was rolled back. The acting
/// moderator must still exist within the transaction (they are never the
/// account being deleted — self-destroy is refused upstream).
pub async fn record_tx(tx: &mut sqlx::PgConnection, new: NewActionLog<'_>) -> Result<(), DbError> {
    insert(&mut *tx, new).await
}

/// The shared INSERT. The acting moderator's handle is snapshotted from the
/// `accounts` row in the same statement, so display no longer depends on a
/// join that would vanish once the actor is deleted. The moderator is
/// authenticated (hence present) at write time, so the `NOT NULL` snapshot
/// column always resolves; a missing actor fails the insert loudly rather than
/// recording a headless action.
async fn insert<'e, E>(executor: E, new: NewActionLog<'_>) -> Result<(), DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO admin_action_logs
            (id, account_id, actor_username, action, target_type, target_id,
             human_identifier, permalink)
        VALUES ($1, $2, (SELECT username FROM accounts WHERE id = $2),
                $3, $4, $5, $6, $7)
        "#,
        id::next(),
        new.account_id,
        new.action,
        new.target_type,
        new.target_id,
        new.human_identifier,
        new.permalink,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// The log, newest first, each line carrying its acting moderator's
/// snapshotted username (so a deleted moderator's actions still read).
pub async fn list(pool: &PgPool, filter: &LogFilter) -> Result<Vec<AdminActionLog>, DbError> {
    let rows = sqlx::query_as!(
        AdminActionLog,
        r#"
        SELECT l.id, l.account_id, l.actor_username AS account_username,
               l.action, l.target_type, l.target_id, l.human_identifier,
               l.permalink, l.created_at
        FROM admin_action_logs l
        WHERE ($1::bigint IS NULL OR l.account_id = $1)
          AND ($2::text   IS NULL OR l.target_type = $2)
          AND ($3::bigint IS NULL OR l.id < $3)
          AND ($5::text   IS NULL OR l.action = $5)
        ORDER BY l.id DESC
        LIMIT $4
        "#,
        filter.account_id,
        filter.target_type.as_deref(),
        filter.max_id,
        filter.limit,
        filter.action.as_deref(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A moderator who has at least one log line, for the filter dropdown.
#[derive(Debug, Clone)]
pub struct LogActor {
    pub account_id: i64,
    pub username: String,
}

/// The distinct *still-existing* moderators appearing in the log, sorted by
/// their current username — the options for the "filter by moderator"
/// dropdown, which filters on `account_id`. A hard-deleted moderator has no id
/// to filter on and so drops out of the dropdown, but their (snapshotted)
/// entries still appear in the unfiltered listing.
pub async fn actors(pool: &PgPool) -> Result<Vec<LogActor>, DbError> {
    let rows = sqlx::query_as!(
        LogActor,
        r#"
        SELECT DISTINCT a.id AS account_id, a.username
        FROM admin_action_logs l
        JOIN accounts a ON a.id = l.account_id
        ORDER BY a.username
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The distinct target types appearing in the log, for the filter dropdown.
pub async fn target_types(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT DISTINCT target_type FROM admin_action_logs ORDER BY target_type"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The distinct action verbs appearing in the log, for the filter dropdown.
pub async fn actions(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let rows =
        sqlx::query_scalar!(r#"SELECT DISTINCT action FROM admin_action_logs ORDER BY action"#)
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn moderator(pool: &PgPool, name: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: name,
                display_name: name,
                note: "",
                public_key_pem: "-----BEGIN PUBLIC KEY-----\ntest\n-----END PUBLIC KEY-----\n",
            },
        )
        .await
        .expect("create moderator")
        .id
    }

    #[sqlx::test]
    async fn record_and_list_newest_first(pool: PgPool) {
        let mod_id = moderator(&pool, "mod").await;
        for (action, target_type) in [("suspend", "Account"), ("resolve", "Report")] {
            record(
                &pool,
                NewActionLog {
                    account_id: mod_id,
                    action,
                    target_type,
                    target_id: 1,
                    human_identifier: "victim",
                    permalink: None,
                },
            )
            .await
            .unwrap();
        }

        let rows = list(
            &pool,
            &LogFilter {
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        let actions: Vec<_> = rows.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, ["resolve", "suspend"]);
        assert_eq!(rows[0].account_username, "mod");
    }

    #[sqlx::test]
    async fn deleting_the_actor_preserves_and_displays_the_log(pool: PgPool) {
        let mod_id = moderator(&pool, "mod").await;
        record(
            &pool,
            NewActionLog {
                account_id: mod_id,
                action: "suspend",
                target_type: "Account",
                target_id: 42,
                human_identifier: "@victim",
                permalink: None,
            },
        )
        .await
        .unwrap();

        // The line snapshots the acting moderator's id and handle at write time.
        let before = list(
            &pool,
            &LogFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].account_id, Some(mod_id));
        assert_eq!(before[0].account_username, "mod");

        // Hard-delete the moderator. The append-only history must survive: the
        // reference is nulled (`ON DELETE SET NULL`), not cascaded away.
        assert!(account::delete_by_id(&pool, mod_id).await.unwrap());

        let after = list(
            &pool,
            &LogFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            after.len(),
            1,
            "the audit line outlives the actor's deletion"
        );
        assert_eq!(
            after[0].account_id, None,
            "the actor reference is nulled, not cascaded"
        );
        assert_eq!(
            after[0].account_username, "mod",
            "the snapshotted handle still displays after deletion"
        );
        assert_eq!(after[0].action, "suspend");
        assert_eq!(after[0].human_identifier, "@victim");

        // The deleted actor drops out of the filter dropdown (no live id to
        // filter on), but the listing itself is intact.
        assert!(actors(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn filters_narrow_by_actor_type_and_cursor(pool: PgPool) {
        let alice = moderator(&pool, "alice").await;
        let bob = moderator(&pool, "bob").await;
        for (who, target_type) in [(alice, "Account"), (bob, "DomainBlock"), (alice, "Report")] {
            record(
                &pool,
                NewActionLog {
                    account_id: who,
                    action: "create",
                    target_type,
                    target_id: 7,
                    human_identifier: "x",
                    permalink: None,
                },
            )
            .await
            .unwrap();
        }

        let only_alice = list(
            &pool,
            &LogFilter {
                account_id: Some(alice),
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_alice.len(), 2);

        let only_blocks = list(
            &pool,
            &LogFilter {
                target_type: Some("DomainBlock".into()),
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_blocks.len(), 1);
        assert_eq!(only_blocks[0].account_username, "bob");

        let all = list(
            &pool,
            &LogFilter {
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        let older = list(
            &pool,
            &LogFilter {
                max_id: Some(all[0].id),
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(older.len(), 2);

        let actor_names: Vec<_> = actors(&pool).await.unwrap();
        let names: Vec<_> = actor_names.iter().map(|a| a.username.as_str()).collect();
        assert_eq!(names, ["alice", "bob"]);

        let types = target_types(&pool).await.unwrap();
        assert_eq!(types, ["Account", "DomainBlock", "Report"]);
    }

    #[sqlx::test]
    async fn filters_narrow_by_action_verb(pool: PgPool) {
        let mod_id = moderator(&pool, "mod").await;
        for action in ["suspend", "unsuspend", "suspend"] {
            record(
                &pool,
                NewActionLog {
                    account_id: mod_id,
                    action,
                    target_type: "Account",
                    target_id: 1,
                    human_identifier: "victim",
                    permalink: None,
                },
            )
            .await
            .unwrap();
        }

        let suspends = list(
            &pool,
            &LogFilter {
                action: Some("suspend".into()),
                limit: 10,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(suspends.len(), 2);
        assert!(suspends.iter().all(|row| row.action == "suspend"));

        assert_eq!(actions(&pool).await.unwrap(), ["suspend", "unsuspend"]);
    }
}
