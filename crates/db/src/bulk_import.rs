//! CSV data-import storage — Mastodon's `BulkImport` /
//! `BulkImportRow`.
//!
//! An upload becomes an `unconfirmed` import plus one typed row per CSV line.
//! Confirming it flips the state to `scheduled`; the import worker claims it
//! (`in_progress`), applies each row and deletes the ones that succeed. When
//! the last row is processed the import is `finished` and whatever rows remain
//! are the failures (re-emitted as a `{type}_failures.csv`). The enum-ish
//! `import_type`/`state` columns are constrained text, like the `email_jobs`
//! queue.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// A bulk import job. `import_type` and `state` are also constrained in the
/// database so a buggy worker cannot create an unreadable queue row.
#[derive(Debug, Clone)]
pub struct BulkImport {
    pub id: i64,
    pub account_id: i64,
    pub import_type: String,
    pub state: String,
    pub overwrite: bool,
    pub total_items: i32,
    pub processed_items: i32,
    pub imported_items: i32,
    pub original_filename: String,
    pub likely_mismatched: bool,
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

impl BulkImport {
    /// How many rows failed to import — Mastodon's `failure_count`.
    #[must_use]
    pub fn failure_count(&self) -> i32 {
        self.processed_items - self.imported_items
    }
}

/// Server-wide ceiling on the total number of rows across every account's
/// in-flight (`unconfirmed`/`scheduled`/`in_progress`) imports.
/// The per-account cap (`MAX_UNFINISHED_IMPORTS_PER_ACCOUNT` × the 20,000-row
/// per-import cap) already bounds one account; this bounds the *sum* so a
/// mass-signup flood cannot each stay under its own cap yet collectively pile
/// millions of rows into the queue. It sits far above any realistic instance's
/// concurrent legitimate imports — ~100 maximum-size imports outstanding at
/// once — so it never bothers real use.
pub const MAX_GLOBAL_PENDING_IMPORT_ROWS: i64 = 2_000_000;

/// The `classid` half of the two-key advisory lock that serializes the global
/// backlog check in [`create_with_rows_capped`]. Any fixed pair works — the
/// (int, int) advisory lock space never intersects the per-account bigint lock
/// space — but a stable, recognizable value keeps the intent legible in
/// `pg_locks` (the bytes spell "PLIM").
const GLOBAL_LOCK_CLASS: i32 = 0x504C_494D;

/// The outcome of [`create_with_rows_capped`]: admitted, or refused by one of
/// the two admission gates so the caller can tell the user which limit applied.
#[derive(Debug)]
pub enum ImportAdmission {
    /// The import (and its rows) were created.
    Admitted(BulkImport),
    /// The account already holds `max_unfinished` not-yet-finished imports.
    AccountFull,
    /// The server-wide pending-import-row backlog
    /// ([`MAX_GLOBAL_PENDING_IMPORT_ROWS`]) is full; the account is under its own
    /// cap but the instance is momentarily saturated.
    ServerBusy,
}

/// One parsed CSV line. Every supported field has a real column; languages
/// live in an ordered child table.
#[derive(Debug, Clone)]
pub struct BulkImportRow {
    pub id: i64,
    pub acct: Option<String>,
    pub show_reblogs: Option<bool>,
    /// The per-follow reply switch. `None` — the column was absent from the
    /// file — leaves the actor-aware default the `follows` trigger chose.
    pub with_replies: Option<bool>,
    pub notify: Option<bool>,
    pub languages: Vec<String>,
    pub hide_notifications: Option<bool>,
    pub domain: Option<String>,
    pub uri: Option<String>,
    pub list_name: Option<String>,
}

/// An account currently on the far side of a relationship, for overwrite
/// reconciliation: the row that would be removed when it is absent from the
/// uploaded file. `domain` is `None` for local and portable local-namespace
/// accounts, matching the CSV export representation.
#[derive(Debug, Clone)]
pub struct ReconcileTarget {
    pub account_id: i64,
    pub username: String,
    pub domain: Option<String>,
}

/// Accounts `account_id` actively follows (pending requests excluded, matching
/// Mastodon's `account.following`).
pub async fn following_targets(
    pool: &PgPool,
    account_id: i64,
) -> Result<Vec<ReconcileTarget>, DbError> {
    let targets = sqlx::query_as!(
        ReconcileTarget,
        r#"
        SELECT a.id AS "account_id!", a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain
        FROM follows f JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND NOT f.pending
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(targets)
}

/// Accounts `account_id` currently blocks.
pub async fn blocking_targets(
    pool: &PgPool,
    account_id: i64,
) -> Result<Vec<ReconcileTarget>, DbError> {
    let targets = sqlx::query_as!(
        ReconcileTarget,
        r#"
        SELECT a.id AS "account_id!", a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain
        FROM blocks b JOIN accounts a ON a.id = b.target_account_id
        WHERE b.account_id = $1
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(targets)
}

/// Accounts `account_id` currently mutes (expired mutes excluded).
pub async fn muting_targets(
    pool: &PgPool,
    account_id: i64,
) -> Result<Vec<ReconcileTarget>, DbError> {
    let targets = sqlx::query_as!(
        ReconcileTarget,
        r#"
        SELECT a.id AS "account_id!", a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain
        FROM mutes m JOIN accounts a ON a.id = m.target_account_id
        WHERE m.account_id = $1 AND (m.expires_at IS NULL OR m.expires_at > now())
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(targets)
}

/// Counts an account's imports that are still in flight — `unconfirmed`,
/// `scheduled`, or `in_progress`. `finished` imports (which retain at most their
/// bounded failure rows and are swept after a week) do not count. Used by the
/// upload handler as a cheap pre-check before it buffers/parses a body;
/// [`create_with_rows_capped`] is the authoritative, race-free gate.
pub async fn count_unfinished_for_account(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM bulk_imports
        WHERE account_id = $1 AND state IN ('unconfirmed', 'scheduled', 'in_progress')
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Creates an `unconfirmed` import and inserts its rows in one transaction,
/// setting `total_items` to the row count — but only when the account holds
/// fewer than `max_unfinished` not-yet-finished imports *and* the server-wide
/// pending-import-row backlog stays at or below `max_global_pending_rows` (the
/// upload handler passes [`MAX_GLOBAL_PENDING_IMPORT_ROWS`]), returning the
/// matching [`ImportAdmission`] refusal otherwise (finding #51).
///
/// Both counts are exact under concurrency. The per-account count and insert
/// share an account-scoped `pg_advisory_xact_lock` so a burst of concurrent
/// uploads cannot each observe a below-cap count under READ COMMITTED (their
/// not-yet-committed rows are invisible to one another) and all insert past the
/// ceiling — the same check-then-insert race
/// [`crate::archive::create_if_none_within`] closes. That lock shares the
/// account-id keyspace with the archive gate, so a same-account archive request
/// and import upload serialize briefly on their short create transactions;
/// distinct accounts never contend. The *global* sum is made exact by a second,
/// two-key advisory lock in a distinct lock space (so it never conflicts with a
/// per-account bigint lock); it serializes only the short create transactions of
/// concurrent uploads from *different* accounts — rare, and cheap — so the
/// global backlog cannot be overshot by uncommitted concurrent inserts.
///
/// The parser still hands this layer transient JSON values, but only their
/// known typed fields cross into SQL.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one transaction admits, then expands every typed CSV field and its ordered languages"
)]
pub async fn create_with_rows_capped(
    pool: &PgPool,
    account_id: i64,
    import_type: &str,
    overwrite: bool,
    original_filename: &str,
    likely_mismatched: bool,
    rows: &[Value],
    max_unfinished: i64,
    max_global_pending_rows: i64,
) -> Result<ImportAdmission, DbError> {
    let mut tx = pool.begin().await?;
    // Global backlog lock first, then the per-account lock — a fixed order every
    // caller follows, so the two lock types never deadlock. The (int, int) form
    // lives in a separate advisory lock space from the account bigint lock.
    sqlx::query!("SELECT pg_advisory_xact_lock($1, $2)", GLOBAL_LOCK_CLASS, 0)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", account_id)
        .execute(&mut *tx)
        .await?;
    let unfinished = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM bulk_imports
        WHERE account_id = $1 AND state IN ('unconfirmed', 'scheduled', 'in_progress')
        "#,
        account_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if unfinished >= max_unfinished {
        tx.commit().await?;
        return Ok(ImportAdmission::AccountFull);
    }
    let pending_rows = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(total_items), 0)::bigint AS "rows!"
        FROM bulk_imports
        WHERE state IN ('unconfirmed', 'scheduled', 'in_progress')
        "#,
    )
    .fetch_one(&mut *tx)
    .await?;
    let new_rows = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    if pending_rows.saturating_add(new_rows) > max_global_pending_rows {
        tx.commit().await?;
        return Ok(ImportAdmission::ServerBusy);
    }
    let import = sqlx::query_as!(
        BulkImport,
        r#"
        INSERT INTO bulk_imports
            (account_id, import_type, overwrite, original_filename, likely_mismatched, total_items)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, account_id, import_type, state, overwrite, total_items, processed_items,
                  imported_items, original_filename, likely_mismatched, created_at, finished_at
        "#,
        account_id,
        import_type,
        overwrite,
        original_filename,
        likely_mismatched,
        i32::try_from(rows.len()).unwrap_or(i32::MAX),
    )
    .fetch_one(&mut *tx)
    .await?;

    if !rows.is_empty() {
        let positions: Vec<i32> = (0..rows.len())
            .map(|position| i32::try_from(position).unwrap_or(i32::MAX))
            .collect();
        let string_field = |key: &str| {
            rows.iter()
                .map(|row| row.get(key).and_then(Value::as_str).map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let bool_field = |key: &str| {
            rows.iter()
                .map(|row| row.get(key).and_then(Value::as_bool))
                .collect::<Vec<_>>()
        };
        let accts = string_field("acct");
        let show_reblogs = bool_field("show_reblogs");
        let with_replies = bool_field("with_replies");
        let notify = bool_field("notify");
        let hide_notifications = bool_field("hide_notifications");
        let domains = string_field("domain");
        let uris = string_field("uri");
        let list_names = string_field("list_name");
        sqlx::query!(
            r#"
            INSERT INTO bulk_import_rows
                (bulk_import_id, row_position, acct, show_reblogs, with_replies, notify,
                 hide_notifications, domain, uri, list_name)
            SELECT $1, input.position, input.acct, input.show_reblogs, input.with_replies,
                   input.notify,
                   input.hide_notifications, input.domain, input.uri, input.list_name
            FROM unnest(
                $2::integer[], $3::text[], $4::boolean[], $5::boolean[], $6::boolean[],
                $7::boolean[], $8::text[], $9::text[], $10::text[]
            ) AS input(
                position, acct, show_reblogs, with_replies, notify, hide_notifications,
                domain, uri, list_name
            )
            "#,
            import.id,
            &positions,
            &accts as &[Option<String>],
            &show_reblogs as &[Option<bool>],
            &with_replies as &[Option<bool>],
            &notify as &[Option<bool>],
            &hide_notifications as &[Option<bool>],
            &domains as &[Option<String>],
            &uris as &[Option<String>],
            &list_names as &[Option<String>],
        )
        .execute(&mut *tx)
        .await?;

        let mut language_rows = Vec::new();
        for (row_position, row) in rows.iter().enumerate() {
            if let Some(languages) = row.get("languages").and_then(Value::as_array) {
                for (language_position, language) in
                    languages.iter().filter_map(Value::as_str).enumerate()
                {
                    language_rows.push((
                        i32::try_from(row_position).unwrap_or(i32::MAX),
                        i32::try_from(language_position).unwrap_or(i32::MAX),
                        language.to_owned(),
                    ));
                }
            }
        }
        if !language_rows.is_empty() {
            let row_positions: Vec<i32> = language_rows.iter().map(|row| row.0).collect();
            let language_positions: Vec<i32> = language_rows.iter().map(|row| row.1).collect();
            let languages: Vec<String> = language_rows.into_iter().map(|row| row.2).collect();
            sqlx::query!(
                r#"
                INSERT INTO bulk_import_row_languages
                    (bulk_import_row_id, position, language)
                SELECT r.id, input.language_position, input.language
                FROM unnest($2::integer[], $3::integer[], $4::text[])
                     AS input(row_position, language_position, language)
                JOIN bulk_import_rows r
                  ON r.bulk_import_id = $1 AND r.row_position = input.row_position
                "#,
                import.id,
                &row_positions,
                &language_positions,
                &languages,
            )
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(ImportAdmission::Admitted(import))
}

/// An import owned by `account_id`, in any state.
pub async fn find_for_account(
    pool: &PgPool,
    account_id: i64,
    id: i64,
) -> Result<Option<BulkImport>, DbError> {
    let import = sqlx::query_as!(
        BulkImport,
        r#"
        SELECT id, account_id, import_type, state, overwrite, total_items, processed_items,
               imported_items, original_filename, likely_mismatched, created_at, finished_at
        FROM bulk_imports WHERE id = $1 AND account_id = $2
        "#,
        id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(import)
}

/// An account's imports, newest first — Mastodon's recent-imports list.
pub async fn recent_for_account(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> Result<Vec<BulkImport>, DbError> {
    let imports = sqlx::query_as!(
        BulkImport,
        r#"
        SELECT id, account_id, import_type, state, overwrite, total_items, processed_items,
               imported_items, original_filename, likely_mismatched, created_at, finished_at
        FROM bulk_imports WHERE account_id = $1 ORDER BY id DESC LIMIT $2
        "#,
        account_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(imports)
}

/// Moves an `unconfirmed` import owned by `account_id` to `scheduled`, ready
/// for the worker. Returns whether a matching row was updated (a
/// confirmed/foreign import matches nothing).
pub async fn mark_scheduled(pool: &PgPool, account_id: i64, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE bulk_imports SET state = 'scheduled'
         WHERE id = $1 AND account_id = $2 AND state = 'unconfirmed'",
        id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes an import owned by `account_id` (its rows cascade). Returns whether
/// a row matched.
pub async fn delete_for_account(pool: &PgPool, account_id: i64, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM bulk_imports WHERE id = $1 AND account_id = $2",
        id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Claims up to `limit` `scheduled` imports, flipping them to `in_progress`
/// and stamping `claimed_at` as the lease marker.
/// `FOR UPDATE SKIP LOCKED` keeps concurrent workers off the same import.
///
/// Imports owned by a suspended account are skipped: a
/// self-deletion or moderator suspension is the tombstone state, so its queued
/// import must never start and recreate social edges or emit federation from an
/// actor peers were told is gone. A lift (`unsuspend`) makes them claimable
/// again; the retention sweep eventually clears any that stay suspended.
pub async fn claim_scheduled(pool: &PgPool, limit: i64) -> Result<Vec<BulkImport>, DbError> {
    let imports = sqlx::query_as!(
        BulkImport,
        r#"
        UPDATE bulk_imports SET state = 'in_progress', claimed_at = now()
        WHERE id IN (
            SELECT bi.id FROM bulk_imports bi
            JOIN accounts a ON a.id = bi.account_id
            WHERE bi.state = 'scheduled'
              AND a.suspended_at IS NULL
            ORDER BY bi.id
            LIMIT $1
            FOR UPDATE OF bi SKIP LOCKED
        )
        RETURNING id, account_id, import_type, state, overwrite, total_items, processed_items,
                  imported_items, original_filename, likely_mismatched, created_at, finished_at
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(imports)
}

/// Returns crashed imports to the queue: an `in_progress` row whose worker died
/// never reaches `finished` and is never re-claimed (the claim only takes
/// `scheduled` rows), so it stalls until the weekly stale sweep deletes it
/// unfinished. This flips any `in_progress` import whose lease has
/// expired — claimed longer than `lease` ago, or (a pre-lease row) never
/// stamped — back to `scheduled` so a worker finishes it. Re-running is safe:
/// successfully imported rows were deleted as they succeeded, and the follow /
/// block / mute / bookmark / list actions are idempotent, so a resumed import
/// only processes the rows left. `lease` must exceed the longest expected import
/// so a still-running one is never reclaimed under it. Returns how many were
/// requeued.
pub async fn reclaim_stale(pool: &PgPool, lease: std::time::Duration) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE bulk_imports SET state = 'scheduled', claimed_at = NULL
        WHERE state = 'in_progress'
          AND (claimed_at IS NULL OR claimed_at < now() - make_interval(secs => $1))
        "#,
        lease.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Every row still attached to an import (successful rows have been deleted,
/// so mid-run this is the not-yet-imported set and post-run it is the
/// failures).
pub async fn rows(pool: &PgPool, import_id: i64) -> Result<Vec<BulkImportRow>, DbError> {
    let rows = sqlx::query_as!(
        BulkImportRow,
        r#"
        SELECT r.id, r.acct, r.show_reblogs, r.with_replies, r.notify,
               COALESCE(
                   ARRAY(
                       SELECT l.language
                       FROM bulk_import_row_languages l
                       WHERE l.bulk_import_row_id = r.id
                       ORDER BY l.position
                   ),
                   '{}'::text[]
               ) AS "languages!",
               r.hide_notifications, r.domain, r.uri, r.list_name
        FROM bulk_import_rows r
        WHERE r.bulk_import_id = $1
        ORDER BY r.row_position
        "#,
        import_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Deletes one successfully-imported row.
pub async fn delete_row(pool: &PgPool, row_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM bulk_import_rows WHERE id = $1", row_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Adds to the processed/imported counters as rows are handled.
pub async fn bump_counts(
    pool: &PgPool,
    import_id: i64,
    processed_delta: i32,
    imported_delta: i32,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE bulk_imports
         SET processed_items = processed_items + $2, imported_items = imported_items + $3
         WHERE id = $1",
        import_id,
        processed_delta,
        imported_delta,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks an import `finished` and stamps `finished_at`.
pub async fn mark_finished(pool: &PgPool, import_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE bulk_imports SET state = 'finished', finished_at = now() WHERE id = $1",
        import_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes imports Mastodon's `Vacuum::ImportsVacuum` would sweep: unconfirmed
/// ones the user never confirmed within 10 minutes, and any import older than
/// a week.
pub async fn delete_stale(pool: &PgPool) -> Result<u64, DbError> {
    let result = sqlx::query!(
        "DELETE FROM bulk_imports
         WHERE (state = 'unconfirmed' AND created_at < now() - interval '10 minutes')
            OR created_at < now() - interval '1 week'"
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// The age in whole seconds of the oldest surviving import, for the retention
/// "oldest retained object" gauge; `None` when none remain.
pub async fn oldest_age_seconds(pool: &PgPool) -> Result<Option<i64>, DbError> {
    let age = sqlx::query_scalar!(
        r#"SELECT EXTRACT(EPOCH FROM now() - min(created_at))::bigint AS "age" FROM bulk_imports"#
    )
    .fetch_one(pool)
    .await?;
    Ok(age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    /// Unwraps an expected [`ImportAdmission::Admitted`] for tests whose subject
    /// is the created import, not the admission gate.
    fn expect_admitted(admission: ImportAdmission) -> BulkImport {
        match admission {
            ImportAdmission::Admitted(import) => import,
            other => panic!("expected an admitted import, got {other:?}"),
        }
    }

    async fn seed_account(pool: &PgPool) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn rows_of(pool: &PgPool, import_id: i64) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM bulk_import_rows WHERE bulk_import_id = $1"#,
            import_id,
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn create_confirm_claim_lifecycle(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        let rows_in = vec![
            serde_json::json!({ "acct": "bob@example.com" }),
            serde_json::json!({ "acct": "carol@example.com" }),
        ];
        let import = create_with_rows_capped(
            &pool,
            account_id,
            "following",
            false,
            "following_accounts.csv",
            false,
            &rows_in,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        let import = expect_admitted(import);
        assert_eq!(import.state, "unconfirmed");
        assert_eq!(import.total_items, 2);
        assert_eq!(rows_of(&pool, import.id).await, 2);

        // Nothing is claimable until confirmed.
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());
        assert!(mark_scheduled(&pool, account_id, import.id).await.unwrap());
        // A second confirm is a no-op (already scheduled).
        assert!(!mark_scheduled(&pool, account_id, import.id).await.unwrap());

        let claimed = claim_scheduled(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].state, "in_progress");
        // Leased: nothing else claimable.
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());

        // Import one row: delete it and bump the counters; fail the other.
        let row = rows(&pool, import.id).await.unwrap().remove(0);
        delete_row(&pool, row.id).await.unwrap();
        bump_counts(&pool, import.id, 1, 1).await.unwrap();
        bump_counts(&pool, import.id, 1, 0).await.unwrap();
        mark_finished(&pool, import.id).await.unwrap();

        let finished = find_for_account(&pool, account_id, import.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.state, "finished");
        assert_eq!(finished.processed_items, 2);
        assert_eq!(finished.imported_items, 1);
        assert_eq!(finished.failure_count(), 1);
        assert!(finished.finished_at.is_some());
        // One failure row remains.
        assert_eq!(rows_of(&pool, import.id).await, 1);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn claim_scheduled_skips_suspended_accounts(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        let import = create_with_rows_capped(
            &pool,
            account_id,
            "following",
            false,
            "following_accounts.csv",
            false,
            &[serde_json::json!({ "acct": "bob@example.com" })],
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        let import = expect_admitted(import);
        assert!(mark_scheduled(&pool, account_id, import.id).await.unwrap());

        // Suspending the account (self-deletion / moderator suspension) makes
        // its scheduled import unclaimable, so the worker never applies its
        // rows and recreates social edges from a tombstone.
        account::suspend(&pool, account_id, "local").await.unwrap();
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());

        // Lifting the suspension makes it claimable again.
        account::unsuspend(&pool, account_id).await.unwrap();
        assert_eq!(claim_scheduled(&pool, 10).await.unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn purge_local_data_cancels_the_accounts_imports(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        let import = create_with_rows_capped(
            &pool,
            account_id,
            "following",
            false,
            "following_accounts.csv",
            false,
            &[serde_json::json!({ "acct": "bob@example.com" })],
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        let import = expect_admitted(import);
        assert!(mark_scheduled(&pool, account_id, import.id).await.unwrap());
        assert_eq!(rows_of(&pool, import.id).await, 1);

        // A self-deletion purge cancels the account's import queue and its rows,
        // so nothing survives to run after the actor is gone.
        account::purge_local_data(&pool, account_id).await.unwrap();
        assert!(
            find_for_account(&pool, account_id, import.id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(rows_of(&pool, import.id).await, 0);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn reclaim_stale_requeues_only_crashed_imports(pool: PgPool) {
        use std::time::Duration;
        let alice = seed_account(&pool).await;
        let import = create_with_rows_capped(
            &pool,
            alice,
            "following",
            false,
            "a.csv",
            false,
            &[],
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        let import = expect_admitted(import);
        assert!(mark_scheduled(&pool, alice, import.id).await.unwrap());
        let claimed = claim_scheduled(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // A fresh claim is within its lease: a reclaim must not requeue a
        // still-running import out from under its worker.
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            0
        );
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());

        // Age the lease past the window (a crashed worker) → requeued.
        sqlx::query!(
            "UPDATE bulk_imports SET claimed_at = now() - interval '2 hours' WHERE id = $1",
            import.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            1
        );
        let requeued = find_for_account(&pool, alice, import.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requeued.state, "scheduled");
        assert_eq!(claim_scheduled(&pool, 10).await.unwrap().len(), 1);

        // A finished import is never reclaimed.
        mark_finished(&pool, import.id).await.unwrap();
        sqlx::query!(
            "UPDATE bulk_imports SET claimed_at = now() - interval '2 hours' WHERE id = $1",
            import.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            0
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn scoping_and_stale_sweep(pool: PgPool) {
        let alice = seed_account(&pool).await;
        let import = create_with_rows_capped(
            &pool,
            alice,
            "blocking",
            false,
            "b.csv",
            false,
            &[],
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
        let import = expect_admitted(import);
        // A foreign account can neither see, confirm nor delete it.
        assert!(
            find_for_account(&pool, alice + 1, import.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!mark_scheduled(&pool, alice + 1, import.id).await.unwrap());
        assert!(
            !delete_for_account(&pool, alice + 1, import.id)
                .await
                .unwrap()
        );

        // Ageing the unconfirmed import past the 10-minute window sweeps it.
        sqlx::query!(
            "UPDATE bulk_imports SET created_at = now() - interval '11 minutes' WHERE id = $1",
            import.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(delete_stale(&pool).await.unwrap(), 1);
        assert!(
            find_for_account(&pool, alice, import.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A huge global ceiling, so per-account tests never trip the backlog gate.
    const NO_GLOBAL_CAP: i64 = i64::MAX;

    async fn make_import(pool: &PgPool, account_id: i64, cap: i64) -> Option<BulkImport> {
        match create_with_rows_capped(
            pool,
            account_id,
            "blocking",
            false,
            "b.csv",
            false,
            &[],
            cap,
            NO_GLOBAL_CAP,
        )
        .await
        .unwrap()
        {
            ImportAdmission::Admitted(import) => Some(import),
            ImportAdmission::AccountFull | ImportAdmission::ServerBusy => None,
        }
    }

    /// Creates an import carrying `row_count` real rows, so the global
    /// pending-row sum moves — the per-account cap here is deliberately generous.
    async fn make_import_with_rows(
        pool: &PgPool,
        account_id: i64,
        row_count: usize,
        max_global_pending_rows: i64,
    ) -> ImportAdmission {
        let rows: Vec<Value> = (0..row_count)
            .map(|i| serde_json::json!({ "acct": format!("user{i}@example.com") }))
            .collect();
        create_with_rows_capped(
            pool,
            account_id,
            "blocking",
            false,
            "b.csv",
            false,
            &rows,
            1000,
            max_global_pending_rows,
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn capped_creation_bounds_unfinished_imports_per_account(pool: PgPool) {
        let alice = seed_account(&pool).await;

        // Up to the cap of in-flight imports is admitted; the next is refused.
        assert!(make_import(&pool, alice, 2).await.is_some());
        assert!(make_import(&pool, alice, 2).await.is_some());
        assert_eq!(count_unfinished_for_account(&pool, alice).await.unwrap(), 2);
        assert!(
            make_import(&pool, alice, 2).await.is_none(),
            "the third in-flight import is refused at the cap"
        );

        // Discarding one frees a slot.
        let existing = recent_for_account(&pool, alice, 10).await.unwrap();
        assert!(
            delete_for_account(&pool, alice, existing[0].id)
                .await
                .unwrap()
        );
        assert!(
            make_import(&pool, alice, 2).await.is_some(),
            "a freed slot admits a new import"
        );

        // A finished import no longer counts against the cap.
        let survivors = recent_for_account(&pool, alice, 10).await.unwrap();
        mark_finished(&pool, survivors[0].id).await.unwrap();
        assert_eq!(count_unfinished_for_account(&pool, alice).await.unwrap(), 1);
        assert!(
            make_import(&pool, alice, 2).await.is_some(),
            "a finished import is excluded, so a fresh one is admitted"
        );

        // The cap is per account: a second account starts from zero.
        let bob = account::create_local(
            &pool,
            NewLocalAccount {
                username: "bob",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;
        assert!(make_import(&pool, bob, 1).await.is_some());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn capped_creation_serializes_concurrent_requests(pool: PgPool) {
        let alice = seed_account(&pool).await;

        // Fire several uploads at once with a cap of 1. Without the account-scoped
        // advisory lock a bare count-then-insert lets every request observe an
        // empty count and insert (finding #51); with it, exactly one is admitted.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::spawn(
                async move { make_import(&pool, alice, 1).await },
            ));
        }
        let mut admitted = 0;
        for handle in handles {
            if handle.await.unwrap().is_some() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 1, "exactly one concurrent upload is admitted");
        assert_eq!(count_unfinished_for_account(&pool, alice).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn global_backlog_ceiling_refuses_under_account_cap(pool: PgPool) {
        let alice = seed_account(&pool).await;
        let bob = account::create_local(
            &pool,
            NewLocalAccount {
                username: "bob",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;

        // A tiny global ceiling of 10 rows. Alice fills it with 8 rows — well
        // under her generous per-account cap.
        assert!(matches!(
            make_import_with_rows(&pool, alice, 8, 10).await,
            ImportAdmission::Admitted(_)
        ));
        // Bob is a fresh account (zero imports, far under his own cap), but the
        // server-wide backlog now has 8 rows and his 5 would overshoot 10, so he
        // is refused as ServerBusy — proving the ceiling is global, not
        // per-account (finding #51).
        assert!(matches!(
            make_import_with_rows(&pool, bob, 5, 10).await,
            ImportAdmission::ServerBusy
        ));
        // A request that exactly reaches the ceiling is admitted; the next row
        // over is not.
        assert!(matches!(
            make_import_with_rows(&pool, bob, 2, 10).await,
            ImportAdmission::Admitted(_)
        ));
        assert!(matches!(
            make_import_with_rows(&pool, bob, 1, 10).await,
            ImportAdmission::ServerBusy
        ));
    }
}
