//! Account mutes: purely local, optionally hiding notifications, optionally
//! expiring. Expired rows count as no mute everywhere.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// An active mute's client-visible state.
#[derive(Debug)]
pub struct Mute {
    pub hide_notifications: bool,
    pub expires_at: Option<OffsetDateTime>,
}

/// Records (or re-configures) `account_id` muting `target_account_id`.
/// Re-muting updates `hide_notifications` and the expiry, like Mastodon's
/// `mute!`.
pub async fn upsert(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
    hide_notifications: bool,
    expires_at: Option<OffsetDateTime>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO mutes (id, account_id, target_account_id, hide_notifications, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (account_id, target_account_id) DO UPDATE SET
            hide_notifications = EXCLUDED.hide_notifications,
            expires_at = EXCLUDED.expires_at
        "#,
        id::next(),
        account_id,
        target_account_id,
        hide_notifications,
        expires_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replays every live local mute of `source` onto `target` in two statements —
/// the account-migration carry (Mastodon's `skip_mute_move?` rule folded into
/// the eligibility query): the target itself, anyone already actively muting
/// the target, and anyone following the target are skipped; the rest are
/// upserted with their `hide_notifications`/`expires_at` carried over.
pub async fn carry_over(
    pool: &PgPool,
    source_account_id: i64,
    target_account_id: i64,
) -> Result<(), DbError> {
    let eligible = sqlx::query!(
        r#"
        SELECT m.account_id AS "account_id!", m.hide_notifications, m.expires_at
        FROM mutes m
        JOIN accounts a ON a.id = m.account_id
        WHERE m.target_account_id = $1 AND a.domain IS NULL
          AND (m.expires_at IS NULL OR m.expires_at > now())
          AND m.account_id <> $2
          AND NOT EXISTS (SELECT 1 FROM mutes t
                          WHERE t.account_id = m.account_id
                            AND t.target_account_id = $2
                            AND (t.expires_at IS NULL OR t.expires_at > now()))
          AND NOT EXISTS (SELECT 1 FROM follows f
                          WHERE f.account_id = m.account_id
                            AND f.target_account_id = $2)
        ORDER BY m.account_id
        "#,
        source_account_id,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    if eligible.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = eligible.iter().map(|_| id::next()).collect();
    let muter_ids: Vec<i64> = eligible.iter().map(|row| row.account_id).collect();
    let hides: Vec<bool> = eligible.iter().map(|row| row.hide_notifications).collect();
    let expiries: Vec<Option<OffsetDateTime>> = eligible.iter().map(|row| row.expires_at).collect();
    sqlx::query!(
        r#"
        INSERT INTO mutes (id, account_id, target_account_id, hide_notifications, expires_at)
        SELECT v.id, v.account_id, $2, v.hide_notifications, v.expires_at
        FROM unnest($1::bigint[], $3::bigint[], $4::bool[], $5::timestamptz[])
             AS v(id, account_id, hide_notifications, expires_at)
        ON CONFLICT (account_id, target_account_id) DO UPDATE SET
            hide_notifications = EXCLUDED.hide_notifications,
            expires_at = EXCLUDED.expires_at
        "#,
        &ids,
        target_account_id,
        &muter_ids,
        &hides,
        &expiries as &[Option<OffsetDateTime>],
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes a mute; returns whether one existed (expired or not).
pub async fn delete(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM mutes WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The active (unexpired) mute from `account_id` on `target_account_id`.
pub async fn find_active(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<Option<Mute>, DbError> {
    let mute = sqlx::query_as!(
        Mute,
        r#"
        SELECT hide_notifications, expires_at
        FROM mutes
        WHERE account_id = $1 AND target_account_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(mute)
}

/// The active mutes from `account_id` toward each of `target_ids`, keyed by
/// target, in one query — the batched form of [`find_active`].
pub async fn active_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Mute>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT target_account_id AS "id!", hide_notifications, expires_at
        FROM mutes
        WHERE account_id = $1 AND target_account_id = ANY($2)
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.id,
                Mute {
                    hide_notifications: row.hide_notifications,
                    expires_at: row.expires_at,
                },
            )
        })
        .collect())
}

/// One entry of the `/api/v1/mutes` listing: the mute row id (the pagination
/// key) and the muted account. Expired mutes are not listed.
#[derive(Debug)]
pub struct MuteListEntry {
    pub row_id: i64,
    pub target_account_id: i64,
}

/// A *local* account muting `target_account_id`, with the mute's settings —
/// the input for carrying mutes over to a migration target.
#[derive(Debug)]
pub struct Muter {
    pub account_id: i64,
    pub hide_notifications: bool,
    pub expires_at: Option<OffsetDateTime>,
}

/// Active mutes by local accounts targeting `target_account_id`. Account
/// migration replays each onto the migration target.
pub async fn local_muters_of(pool: &PgPool, target_account_id: i64) -> Result<Vec<Muter>, DbError> {
    let muters = sqlx::query_as!(
        Muter,
        r#"
        SELECT m.account_id AS "account_id!", m.hide_notifications, m.expires_at
        FROM mutes m
        JOIN accounts a ON a.id = m.account_id
        WHERE m.target_account_id = $1 AND a.domain IS NULL
          AND (m.expires_at IS NULL OR m.expires_at > now())
        ORDER BY m.account_id
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(muters)
}

/// Accounts `account_id` mutes (active only), newest mute first,
/// keyset-paginated by mute row id.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<MuteListEntry>, DbError> {
    let entries = sqlx::query_as!(
        MuteListEntry,
        r#"
        SELECT id AS "row_id!", target_account_id AS "target_account_id!"
        FROM mutes
        WHERE account_id = $1
          AND (expires_at IS NULL OR expires_at > now())
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
    Ok(entries)
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
    async fn mute_lifecycle_reconfigures_and_expires(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;

        upsert(&pool, alice, carol, true, None).await.unwrap();
        let mute = find_active(&pool, alice, carol).await.unwrap().unwrap();
        assert!(mute.hide_notifications);
        assert!(mute.expires_at.is_none());

        // Re-muting flips hide_notifications and sets an expiry.
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        upsert(&pool, alice, carol, false, Some(later))
            .await
            .unwrap();
        let mute = find_active(&pool, alice, carol).await.unwrap().unwrap();
        assert!(!mute.hide_notifications);
        assert!(mute.expires_at.is_some());
        assert_eq!(list(&pool, alice, None, None, 10).await.unwrap().len(), 1);

        // An expired mute is invisible to find_active and the listing,
        // but delete still reports it existed.
        let past = OffsetDateTime::now_utc() - time::Duration::minutes(1);
        upsert(&pool, alice, carol, true, Some(past)).await.unwrap();
        assert!(find_active(&pool, alice, carol).await.unwrap().is_none());
        assert!(list(&pool, alice, None, None, 10).await.unwrap().is_empty());
        assert!(delete(&pool, alice, carol).await.unwrap());
        assert!(!delete(&pool, alice, carol).await.unwrap());
    }

    #[sqlx::test]
    async fn listing_pages_newest_first(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        upsert(&pool, alice, carol, true, None).await.unwrap();
        upsert(&pool, alice, dave, true, None).await.unwrap();

        let entries = list(&pool, alice, None, None, 10).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.target_account_id)
                .collect::<Vec<_>>(),
            [dave, carol]
        );
        let older = list(&pool, alice, Some(entries[0].row_id), None, 10)
            .await
            .unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].target_account_id, carol);
    }
}
