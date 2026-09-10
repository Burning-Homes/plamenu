//! Self-service auto-deletion of old posts — Mastodon's
//! `AccountStatusesCleanupPolicy`. The policy row stores which of the
//! account's own local statuses are exempt; the server's sweep worker asks
//! [`statuses_to_delete`] for the next eligible batch and deletes them
//! through the normal (federating) delete path.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// Mastodon's `ALLOWED_MIN_STATUS_AGE` ladder, in seconds (`ActiveSupport`
/// durations: a month is 1/12 of a mean Gregorian year, not 30 days).
pub const ALLOWED_MIN_STATUS_AGE: [i32; 8] = [
    604_800,    // 1 week
    1_209_600,  // 2 weeks
    2_629_746,  // 1 month
    5_259_492,  // 2 months
    7_889_238,  // 3 months
    15_778_476, // 6 months
    31_556_952, // 1 year
    63_113_904, // 2 years
];

/// Mastodon's `EARLY_SEARCH_CUTOFF`: at most this many old statuses are
/// scanned past the cursor per run, bounding the eligibility query on
/// accounts with many exempt statuses.
const EARLY_SEARCH_CUTOFF: i64 = 5_000;

/// One account's cleanup policy row.
#[allow(clippy::struct_excessive_bools)] // independent exception toggles
#[derive(Debug, Clone)]
pub struct CleanupPolicy {
    pub account_id: i64,
    pub enabled: bool,
    /// Seconds a status must be older than to become eligible.
    pub min_status_age: i32,
    pub keep_direct: bool,
    pub keep_pinned: bool,
    pub keep_polls: bool,
    pub keep_media: bool,
    pub keep_self_fav: bool,
    pub keep_self_bookmark: bool,
    pub min_favs: Option<i32>,
    pub min_reblogs: Option<i32>,
    pub last_inspected_id: Option<i64>,
    pub updated_at: OffsetDateTime,
}

impl CleanupPolicy {
    /// The defaults Mastodon builds an unsaved policy with (enabled is what
    /// the form submits; the column default is only a schema artifact).
    #[must_use]
    pub fn defaults(account_id: i64) -> Self {
        Self {
            account_id,
            enabled: false,
            min_status_age: 1_209_600,
            keep_direct: true,
            keep_pinned: true,
            keep_polls: false,
            keep_media: false,
            keep_self_fav: true,
            keep_self_bookmark: true,
            min_favs: None,
            min_reblogs: None,
            last_inspected_id: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

/// The form-editable subset for [`upsert`].
#[allow(clippy::struct_excessive_bools)] // independent exception toggles
#[derive(Debug, Clone, Copy)]
pub struct PolicyUpdate {
    pub enabled: bool,
    pub min_status_age: i32,
    pub keep_direct: bool,
    pub keep_pinned: bool,
    pub keep_polls: bool,
    pub keep_media: bool,
    pub keep_self_fav: bool,
    pub keep_self_bookmark: bool,
    pub min_favs: Option<i32>,
    pub min_reblogs: Option<i32>,
}

/// The account's policy, if it ever saved one.
pub async fn get(pool: &PgPool, account_id: i64) -> Result<Option<CleanupPolicy>, DbError> {
    let policy = sqlx::query_as!(
        CleanupPolicy,
        r#"
        SELECT account_id, enabled, min_status_age, keep_direct, keep_pinned,
               keep_polls, keep_media, keep_self_fav, keep_self_bookmark,
               min_favs, min_reblogs, last_inspected_id, updated_at
        FROM account_statuses_cleanup_policies
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(policy)
}

/// Creates or replaces the account's policy. When an update *widens* the
/// policy — a keep-exception switched off, or a popularity threshold raised
/// or removed — previously-kept statuses become eligible again, so the scan
/// cursor is reset and the sweep starts over (Mastodon's
/// `update_last_inspected`).
pub async fn upsert(
    pool: &PgPool,
    account_id: i64,
    update: PolicyUpdate,
) -> Result<CleanupPolicy, DbError> {
    let policy = sqlx::query_as!(
        CleanupPolicy,
        r#"
        INSERT INTO account_statuses_cleanup_policies
            (account_id, enabled, min_status_age, keep_direct, keep_pinned,
             keep_polls, keep_media, keep_self_fav, keep_self_bookmark,
             min_favs, min_reblogs)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (account_id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            min_status_age = EXCLUDED.min_status_age,
            keep_direct = EXCLUDED.keep_direct,
            keep_pinned = EXCLUDED.keep_pinned,
            keep_polls = EXCLUDED.keep_polls,
            keep_media = EXCLUDED.keep_media,
            keep_self_fav = EXCLUDED.keep_self_fav,
            keep_self_bookmark = EXCLUDED.keep_self_bookmark,
            min_favs = EXCLUDED.min_favs,
            min_reblogs = EXCLUDED.min_reblogs,
            last_inspected_id = CASE WHEN
                (account_statuses_cleanup_policies.keep_direct AND NOT EXCLUDED.keep_direct)
                OR (account_statuses_cleanup_policies.keep_pinned AND NOT EXCLUDED.keep_pinned)
                OR (account_statuses_cleanup_policies.keep_polls AND NOT EXCLUDED.keep_polls)
                OR (account_statuses_cleanup_policies.keep_media AND NOT EXCLUDED.keep_media)
                OR (account_statuses_cleanup_policies.keep_self_fav AND NOT EXCLUDED.keep_self_fav)
                OR (account_statuses_cleanup_policies.keep_self_bookmark
                    AND NOT EXCLUDED.keep_self_bookmark)
                OR (account_statuses_cleanup_policies.min_favs IS NOT NULL
                    AND (EXCLUDED.min_favs IS NULL
                         OR EXCLUDED.min_favs > account_statuses_cleanup_policies.min_favs))
                OR (account_statuses_cleanup_policies.min_reblogs IS NOT NULL
                    AND (EXCLUDED.min_reblogs IS NULL
                         OR EXCLUDED.min_reblogs > account_statuses_cleanup_policies.min_reblogs))
                THEN NULL
                ELSE account_statuses_cleanup_policies.last_inspected_id
            END,
            updated_at = now()
        RETURNING account_id, enabled, min_status_age, keep_direct, keep_pinned,
                  keep_polls, keep_media, keep_self_fav, keep_self_bookmark,
                  min_favs, min_reblogs, last_inspected_id, updated_at
        "#,
        account_id,
        update.enabled,
        update.min_status_age,
        update.keep_direct,
        update.keep_pinned,
        update.keep_polls,
        update.keep_media,
        update.keep_self_fav,
        update.keep_self_bookmark,
        update.min_favs,
        update.min_reblogs,
    )
    .fetch_one(pool)
    .await?;
    Ok(policy)
}

/// Advances the scan cursor: everything at or below `last_id` has been
/// examined.
pub async fn record_last_inspected(
    pool: &PgPool,
    account_id: i64,
    last_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE account_statuses_cleanup_policies SET last_inspected_id = $2 WHERE account_id = $1",
        account_id,
        last_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Why a previously-kept status may have become eligible again.
#[derive(Debug, Clone, Copy)]
pub enum KeptReason {
    SelfFav,
    SelfBookmark,
    Pin,
}

/// Rolls the scan cursor back to `status_id` after the author un-favourites,
/// un-bookmarks or un-pins their own already-inspected status — the matching
/// keep-exception no longer shields it (Mastodon's
/// `invalidate_last_inspected`). A no-op unless that exception is on and the
/// cursor has moved past the status.
pub async fn rollback_last_inspected<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
    reason: KeptReason,
) -> Result<(), DbError> {
    let (unfav, unbookmark, unpin) = match reason {
        KeptReason::SelfFav => (true, false, false),
        KeptReason::SelfBookmark => (false, true, false),
        KeptReason::Pin => (false, false, true),
    };
    sqlx::query!(
        r#"
        UPDATE account_statuses_cleanup_policies
        SET last_inspected_id = $2
        WHERE account_id = $1
          AND last_inspected_id > $2
          AND (($3 AND keep_self_fav) OR ($4 AND keep_self_bookmark) OR ($5 AND keep_pinned))
        "#,
        account_id,
        status_id,
        unfav,
        unbookmark,
        unpin,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The upper bound of this run's scan window: the newest status id that is
/// both old enough and within `EARLY_SEARCH_CUTOFF` statuses of the cursor
/// (Mastodon's `compute_cutoff_id`). `None` means nothing is old enough yet.
pub async fn compute_cutoff_id(
    pool: &PgPool,
    policy: &CleanupPolicy,
) -> Result<Option<i64>, DbError> {
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .expect("current time fits in i64 milliseconds");
    let age_ms = i64::from(policy.min_status_age).saturating_mul(1_000);
    let old_enough = crate::id::id_at(now_ms - age_ms);
    let cutoff = sqlx::query_scalar!(
        r#"
        SELECT MAX(id) AS "max"
        FROM (
            SELECT id FROM statuses -- STUBKEEP: cursor math only; no row is displayed
            WHERE account_id = $1 AND id >= $2 AND id <= $3
            ORDER BY id ASC
            LIMIT $4
        ) scan_window
        "#,
        policy.account_id,
        policy.last_inspected_id.unwrap_or(0),
        old_enough,
        EARLY_SEARCH_CUTOFF,
    )
    .fetch_one(pool)
    .await?;
    Ok(cutoff)
}

/// The next `limit` statuses the policy allows deleting, oldest first, within
/// the scan window `(cursor ..= max_id]` — every filter mirrors Mastodon's
/// `statuses_to_delete` scopes. Popularity thresholds count live rows (a
/// boost is a status row with `reblog_of_id`), matching the counters the API
/// serves.
pub async fn statuses_to_delete(
    pool: &PgPool,
    policy: &CleanupPolicy,
    max_id: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT s.id
        FROM statuses s
        WHERE s.account_id = $1
          AND s.id <= $2
          AND s.id >= $3
          AND s.deleted_at IS NULL -- STUBFILTER: an existing stub has nothing left to clean up
          AND s.uri IS NULL
          AND (NOT $4::bool OR s.visibility <> 'direct')
          AND (NOT $5::bool OR NOT EXISTS (
              SELECT 1 FROM status_pins p
              WHERE p.status_id = s.id AND p.account_id = s.account_id))
          AND (NOT $6::bool OR NOT EXISTS (
              SELECT 1 FROM polls pl WHERE pl.status_id = s.id))
          AND (NOT $7::bool OR NOT EXISTS (
              SELECT 1 FROM media_attachments m WHERE m.status_id = s.id))
          AND (NOT $8::bool OR NOT EXISTS (
              SELECT 1 FROM favourites f
              WHERE f.status_id = s.id AND f.account_id = s.account_id))
          AND (NOT $9::bool OR NOT EXISTS (
              SELECT 1 FROM bookmarks b
              WHERE b.status_id = s.id AND b.account_id = s.account_id))
          AND ($10::int IS NULL OR
              (SELECT count(*) FROM favourites f2 WHERE f2.status_id = s.id) < $10)
          AND ($11::int IS NULL OR
              (SELECT count(*) FROM statuses b2 WHERE b2.reblog_of_id = s.id) < $11)
        ORDER BY s.id ASC
        LIMIT $12
        "#,
        policy.account_id,
        max_id,
        policy.last_inspected_id.unwrap_or(0),
        policy.keep_direct,
        policy.keep_pinned,
        policy.keep_polls,
        policy.keep_media,
        policy.keep_self_fav,
        policy.keep_self_bookmark,
        policy.min_favs,
        policy.min_reblogs,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// One sweep-worker page of enabled policies, round-robin: accounts after
/// `cursor` first, then wrapping around to the start.
pub async fn enabled_page(
    pool: &PgPool,
    cursor: i64,
    limit: i64,
) -> Result<Vec<CleanupPolicy>, DbError> {
    let policies = sqlx::query_as!(
        CleanupPolicy,
        r#"
        SELECT account_id, enabled, min_status_age, keep_direct, keep_pinned,
               keep_polls, keep_media, keep_self_fav, keep_self_bookmark,
               min_favs, min_reblogs, last_inspected_id, updated_at
        FROM account_statuses_cleanup_policies
        WHERE enabled
        ORDER BY (account_id <= $1), account_id
        LIMIT $2
        "#,
        cursor,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(policies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::NewLocalAccount;
    use crate::status::{self, NewLocalStatus};
    use crate::{account, bookmark, favourite, id, pin};

    async fn local_account(pool: &PgPool, username: &str) -> i64 {
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

    fn default_update() -> PolicyUpdate {
        PolicyUpdate {
            enabled: true,
            min_status_age: 1_209_600,
            keep_direct: true,
            keep_pinned: true,
            keep_polls: false,
            keep_media: false,
            keep_self_fav: true,
            keep_self_bookmark: true,
            min_favs: None,
            min_reblogs: None,
        }
    }

    /// Creates a local status, then backdates its snowflake id by `age_secs`
    /// (keeping the sequence bits so parallel backdates stay unique).
    async fn old_status(pool: &PgPool, account_id: i64, visibility: &str, age_secs: i64) -> i64 {
        let created = status::create_local(
            pool,
            NewLocalStatus::new(account_id, "<p>old</p>", visibility, None),
        )
        .await
        .unwrap();
        let now_ms =
            i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                .unwrap();
        let old_id = id::id_at(now_ms - age_secs * 1_000) | (created.id & 0xFFFF);
        sqlx::query!(
            "UPDATE statuses SET id = $1 WHERE id = $2",
            old_id,
            created.id
        )
        .execute(pool)
        .await
        .unwrap();
        old_id
    }

    const TWO_WEEKS: i64 = 1_209_600;

    #[sqlx::test]
    async fn upsert_roundtrips_and_widening_resets_cursor(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        assert!(get(&pool, alice).await.unwrap().is_none());

        let saved = upsert(&pool, alice, default_update()).await.unwrap();
        assert!(saved.enabled);
        assert_eq!(saved.min_status_age, 1_209_600);
        assert!(saved.keep_direct && saved.keep_pinned);
        assert!(!saved.keep_polls && !saved.keep_media);
        assert_eq!(saved.last_inspected_id, None);

        record_last_inspected(&pool, alice, 42).await.unwrap();
        // A no-op resave keeps the cursor…
        let same = upsert(&pool, alice, default_update()).await.unwrap();
        assert_eq!(same.last_inspected_id, Some(42));
        // …and so does *narrowing* (adding an exception).
        let narrowed = upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_polls: true,
                min_favs: Some(5),
                ..default_update()
            },
        )
        .await
        .unwrap();
        assert_eq!(narrowed.last_inspected_id, Some(42));
        // Raising a threshold widens the policy: cursor resets.
        let raised = upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_polls: true,
                min_favs: Some(10),
                ..default_update()
            },
        )
        .await
        .unwrap();
        assert_eq!(raised.last_inspected_id, None);

        record_last_inspected(&pool, alice, 42).await.unwrap();
        // Dropping a keep-exception widens too.
        let widened = upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_direct: false,
                keep_polls: true,
                min_favs: Some(10),
                ..default_update()
            },
        )
        .await
        .unwrap();
        assert_eq!(widened.last_inspected_id, None);
    }

    #[sqlx::test]
    async fn rollback_respects_exception_flags(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        upsert(&pool, alice, default_update()).await.unwrap();
        record_last_inspected(&pool, alice, 100).await.unwrap();

        // Higher than the cursor: no-op.
        rollback_last_inspected(&pool, alice, 200, KeptReason::SelfFav)
            .await
            .unwrap();
        assert_eq!(
            get(&pool, alice).await.unwrap().unwrap().last_inspected_id,
            Some(100)
        );
        // keep_polls-style reason with the flag off: no-op. (keep_media/
        // keep_polls have no rollback reason; test a disabled keep flag.)
        upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_self_fav: false,
                ..default_update()
            },
        )
        .await
        .unwrap();
        record_last_inspected(&pool, alice, 100).await.unwrap();
        rollback_last_inspected(&pool, alice, 50, KeptReason::SelfFav)
            .await
            .unwrap();
        assert_eq!(
            get(&pool, alice).await.unwrap().unwrap().last_inspected_id,
            Some(100)
        );
        // Enabled flag and a lower id: rolls back.
        rollback_last_inspected(&pool, alice, 50, KeptReason::Pin)
            .await
            .unwrap();
        assert_eq!(
            get(&pool, alice).await.unwrap().unwrap().last_inspected_id,
            Some(50)
        );
    }

    #[sqlx::test]
    async fn eligibility_honors_age_and_exceptions(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let carol = local_account(&pool, "carol").await;
        let policy = upsert(&pool, alice, default_update()).await.unwrap();

        // Fresh: not old enough regardless of policy.
        status::create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>new</p>", "public", None),
        )
        .await
        .unwrap();
        let plain = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        let direct = old_status(&pool, alice, "direct", 2 * TWO_WEEKS).await;
        let pinned = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        pin::create(&pool, alice, pinned).await.unwrap();
        let self_faved = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        favourite::create(&pool, alice, self_faved, None)
            .await
            .unwrap();
        let self_bookmarked = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        bookmark::create(&pool, alice, self_bookmarked)
            .await
            .unwrap();
        let with_poll = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        sqlx::query!(
            "INSERT INTO polls (id, status_id, account_id, options, cached_tallies)
             VALUES ($1, $2, $3, '{\"a\",\"b\"}', '{0,0}')",
            id::next(),
            with_poll,
            alice,
        )
        .execute(&pool)
        .await
        .unwrap();
        let with_media = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        sqlx::query!(
            "INSERT INTO media_attachments (id, account_id, status_id, content_type, file_name)
             VALUES ($1, $2, $3, 'image/png', 'x.png')",
            id::next(),
            alice,
            with_media,
        )
        .execute(&pool)
        .await
        .unwrap();
        let popular = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        favourite::create(&pool, carol, popular, None)
            .await
            .unwrap();

        // Default policy (keep direct/pinned/self-fav/self-bookmark, no
        // thresholds): polls, media and the merely-popular one are eligible.
        let cutoff = compute_cutoff_id(&pool, &policy).await.unwrap().unwrap();
        let mut eligible = statuses_to_delete(&pool, &policy, cutoff, 50)
            .await
            .unwrap();
        eligible.sort_unstable();
        let mut expected = vec![plain, with_poll, with_media, popular];
        expected.sort_unstable();
        assert_eq!(eligible, expected);

        // Thresholds keep the popular one; keep_polls/keep_media shield those.
        let strict = upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_polls: true,
                keep_media: true,
                min_favs: Some(1),
                ..default_update()
            },
        )
        .await
        .unwrap();
        let cutoff = compute_cutoff_id(&pool, &strict).await.unwrap().unwrap();
        let eligible = statuses_to_delete(&pool, &strict, cutoff, 50)
            .await
            .unwrap();
        assert_eq!(eligible, vec![plain]);

        // Everything-goes policy: all old statuses are eligible.
        let lax = upsert(
            &pool,
            alice,
            PolicyUpdate {
                keep_direct: false,
                keep_pinned: false,
                keep_self_fav: false,
                keep_self_bookmark: false,
                ..default_update()
            },
        )
        .await
        .unwrap();
        let cutoff = compute_cutoff_id(&pool, &lax).await.unwrap().unwrap();
        let eligible = statuses_to_delete(&pool, &lax, cutoff, 50).await.unwrap();
        let mut all = vec![
            plain,
            direct,
            pinned,
            self_faved,
            self_bookmarked,
            with_poll,
            with_media,
            popular,
        ];
        all.sort_unstable();
        assert_eq!(eligible, all);

        // The cursor floor excludes everything at or below it… almost: the
        // window is inclusive of the cursor itself (Mastodon's `min_id..`).
        record_last_inspected(&pool, alice, all[all.len() - 2])
            .await
            .unwrap();
        let resumed = get(&pool, alice).await.unwrap().unwrap();
        let cutoff = compute_cutoff_id(&pool, &resumed).await.unwrap().unwrap();
        let eligible = statuses_to_delete(&pool, &resumed, cutoff, 50)
            .await
            .unwrap();
        assert_eq!(eligible, all[all.len() - 2..]);
    }

    #[sqlx::test]
    async fn cutoff_is_none_until_something_is_old_enough(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let policy = upsert(&pool, alice, default_update()).await.unwrap();
        status::create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>new</p>", "public", None),
        )
        .await
        .unwrap();
        assert_eq!(compute_cutoff_id(&pool, &policy).await.unwrap(), None);

        let old = old_status(&pool, alice, "public", 2 * TWO_WEEKS).await;
        assert_eq!(compute_cutoff_id(&pool, &policy).await.unwrap(), Some(old));
    }

    #[sqlx::test]
    async fn enabled_page_wraps_around_the_cursor(pool: PgPool) {
        let a = local_account(&pool, "a").await;
        let b = local_account(&pool, "b").await;
        let c = local_account(&pool, "c").await;
        for id in [a, b, c] {
            upsert(&pool, id, default_update()).await.unwrap();
        }
        upsert(
            &pool,
            b,
            PolicyUpdate {
                enabled: false,
                ..default_update()
            },
        )
        .await
        .unwrap();

        let page = enabled_page(&pool, a, 10).await.unwrap();
        let ids: Vec<i64> = page.iter().map(|p| p.account_id).collect();
        assert_eq!(ids, vec![c, a], "after the cursor first, then wrap");
    }
}
