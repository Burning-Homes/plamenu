//! Notifications for local accounts.

use std::collections::HashSet;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Notification {
    pub id: i64,
    /// Recipient (local) account.
    pub account_id: i64,
    pub from_account_id: i64,
    /// `follow` | `follow_request` | `favourite` | `reblog` | `mention` |
    /// `quote` | `update` | `quoted_update` | `poll` | `added_to_collection` |
    /// `collection_update` | `pleroma:emoji_reaction` | `admin.sign_up` |
    /// `admin.report` | `moderation_warning` | `live`.
    pub kind: String,
    pub status_id: Option<i64>,
    /// The collection a `added_to_collection`/`collection_update` notification
    /// points at (Mastodon's polymorphic `activity`); `None` otherwise.
    pub collection_id: Option<i64>,
    /// The reacted emoji of a `pleroma:emoji_reaction` notification (Unicode
    /// or `:shortcode:`); `None` for every other kind.
    pub emoji: Option<String>,
    pub created_at: OffsetDateTime,
    /// Grouping key (Mastodon's `group_key`); set for groupable kinds only.
    pub group_key: Option<String>,
    /// Hidden by the recipient's notification policy (Mastodon's `filtered`):
    /// excluded from the default listings, rolled up into a notification
    /// request. The REST serializer only emits the attribute when true.
    pub filtered: bool,
    /// The strike a `moderation_warning` notification announces; `None` for
    /// every other kind.
    pub account_warning_id: Option<i64>,
    /// The report an `admin.report` staff notification announces; `None` for
    /// every other kind. Rows die with the report (FK cascade).
    pub report_id: Option<i64>,
}

/// Kinds that fold into notification groups — Mastodon's
/// `GROUPABLE_NOTIFICATION_TYPES` minus `admin.sign_up`, which Plamenu emits
/// but never groups (staff see each applicant on their own row).
pub const GROUPABLE_KINDS: &[&str] = &["favourite", "reblog", "follow"];

/// A group stops absorbing new notifications once it spans this many hours
/// (Mastodon's `MAXIMUM_GROUP_SPAN_HOURS`).
const MAX_GROUP_SPAN_HOURS: i64 = 12;

/// The group key of a new notification: `{kind}[-{status_id}]-{hour_bucket}`,
/// reusing the latest group's bucket while it is younger than the maximum
/// span — Mastodon's `set_group_key!`, with the previous bucket read back
/// from the notifications table instead of Redis.
async fn next_group_key(
    pool: &PgPool,
    account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<Option<String>, DbError> {
    if !GROUPABLE_KINDS.contains(&kind) {
        return Ok(None);
    }
    let previous = sqlx::query_scalar!(
        r#"
        SELECT group_key AS "group_key!"
        FROM notifications
        WHERE account_id = $1 AND kind = $2
          AND status_id IS NOT DISTINCT FROM $3
          AND group_key IS NOT NULL
        ORDER BY id DESC
        LIMIT 1
        "#,
        account_id,
        kind,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    let mut hour_bucket = OffsetDateTime::now_utc().unix_timestamp() / 3600;
    if let Some(previous_bucket) = previous
        .as_deref()
        .and_then(|key| key.rsplit('-').next())
        .and_then(|bucket| bucket.parse::<i64>().ok())
        && hour_bucket < previous_bucket + MAX_GROUP_SPAN_HOURS
    {
        hour_bucket = previous_bucket;
    }
    let prefix = match status_id {
        Some(status_id) => format!("{kind}-{status_id}"),
        None => kind.to_owned(),
    };
    Ok(Some(format!("{prefix}-{hour_bucket}")))
}

/// Records a notification. Self-notifications are silently skipped — except
/// `poll` ("your poll has ended"), the one self-notifying kind Plamenu can
/// emit (Mastodon's `NotifyService` drop-exemption list).
pub async fn create(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<(), DbError> {
    create_inner(
        pool,
        account_id,
        from_account_id,
        kind,
        status_id,
        None,
        None,
    )
    .await
}

/// Batched [`create`] for the `status` kind — one statement notifies every
/// follower who opted into per-follow `notify` about a fresh post.
///
/// Faithful to the per-recipient pipeline because `status` is in neither
/// [`crate::notification_policy::FILTERABLE_KINDS`] (policy evaluation
/// statically accepts) nor [`GROUPABLE_KINDS`] (no group key), so what remains
/// per recipient is exactly the self-notification skip and the
/// muted-conversation drop — both folded into the statement. The parity test
/// below pins this against [`create`]; if `status` ever becomes filterable or
/// groupable this must go back through the per-recipient path.
pub async fn create_status_many(
    pool: &PgPool,
    recipient_ids: &[i64],
    from_account_id: i64,
    status_id: i64,
) -> Result<(), DbError> {
    if recipient_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = recipient_ids.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, status_id)
        SELECT v.id, v.account_id, $3, 'status', $4
        FROM unnest($1::bigint[], $2::bigint[]) AS v(id, account_id)
        WHERE v.account_id <> $3
          AND NOT EXISTS (
              SELECT 1
              FROM status_conversations sc
              JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
              WHERE sc.status_id = $4 AND cm.account_id = v.account_id
          )
        "#,
        &ids,
        recipient_ids,
        from_account_id,
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`create`] for any kind that is in neither
/// [`crate::notification_policy::FILTERABLE_KINDS`] nor [`GROUPABLE_KINDS`]
/// (`update`, `quoted_update`, `event.changed`, `live`) — one statement
/// notifies a whole audience, each recipient about its own target status.
/// Faithful for the same reason as [`create_status_many`]: policy evaluation
/// statically accepts such kinds and no group key exists, so what remains per
/// recipient is exactly the self-notification skip and the
/// muted-conversation drop, both folded into the statement. The parity test
/// below pins it against [`create`].
pub async fn create_ungroupable_many(
    pool: &PgPool,
    recipients: &[(i64, i64)],
    from_account_id: i64,
    kind: &str,
) -> Result<(), DbError> {
    debug_assert!(
        !crate::notification_policy::FILTERABLE_KINDS.contains(&kind)
            && !GROUPABLE_KINDS.contains(&kind),
        "kind {kind:?} is filterable or groupable and must go through the per-recipient pipeline"
    );
    if recipients.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = recipients.iter().map(|_| id::next()).collect();
    let account_ids: Vec<i64> = recipients.iter().map(|(account, _)| *account).collect();
    let status_ids: Vec<i64> = recipients.iter().map(|(_, status)| *status).collect();
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, status_id)
        SELECT v.id, v.account_id, $4, $5, v.status_id
        FROM unnest($1::bigint[], $2::bigint[], $3::bigint[]) AS v(id, account_id, status_id)
        WHERE v.account_id <> $4
          AND NOT EXISTS (
              SELECT 1
              FROM status_conversations sc
              JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
              WHERE sc.status_id = v.status_id AND cm.account_id = v.account_id
          )
        "#,
        &ids,
        &account_ids,
        &status_ids,
        from_account_id,
        kind,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`create`] for the `mention` kind — one call notifies every newly
/// mentioned local account about one status.
///
/// `mention` is filterable but not groupable, so the per-recipient pipeline
/// is: self-skip, muted-conversation drop, policy evaluation, INSERT (with
/// `filtered` for a `Filter` disposition), and a request-row upsert for the
/// filtered remainder. Every stage here is the batch form of exactly that —
/// policy evaluation goes through [`notification_policy::evaluate_many`],
/// which *is* the singular implementation — and the parity test below pins
/// this against [`create`] across every policy arm. `next_group_key` is
/// statically `None` for `mention`, so no group-key read exists to batch.
pub async fn create_mentions_many(
    pool: &PgPool,
    recipient_ids: &[i64],
    from_account_id: i64,
    status_id: i64,
) -> Result<(), DbError> {
    // Dedup (a repeated recipient must not double-notify, and the request
    // upsert below may not hit one row twice in a statement) and self-skip.
    let mut targets: Vec<i64> = recipient_ids
        .iter()
        .copied()
        .filter(|&id| id != from_account_id)
        .collect();
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() {
        return Ok(());
    }
    let muted: HashSet<i64> = crate::conversation::status_muted_of(pool, &targets, status_id)
        .await?
        .into_iter()
        .collect();
    targets.retain(|id| !muted.contains(id));
    if targets.is_empty() {
        return Ok(());
    }
    let dispositions = crate::notification_policy::evaluate_many(
        pool,
        &targets,
        from_account_id,
        "mention",
        Some(status_id),
    )
    .await?;
    let mut accounts: Vec<i64> = Vec::new();
    let mut filtered_flags: Vec<bool> = Vec::new();
    let mut filtered_accounts: Vec<i64> = Vec::new();
    for &recipient in &targets {
        match dispositions
            .get(&recipient)
            .copied()
            .unwrap_or(crate::notification_policy::Disposition::Accept)
        {
            crate::notification_policy::Disposition::Drop => {}
            crate::notification_policy::Disposition::Accept => {
                accounts.push(recipient);
                filtered_flags.push(false);
            }
            crate::notification_policy::Disposition::Filter => {
                accounts.push(recipient);
                filtered_flags.push(true);
                filtered_accounts.push(recipient);
            }
        }
    }
    if accounts.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = accounts.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, status_id, filtered)
        SELECT v.id, v.account_id, $4, 'mention', $5, v.filtered
        FROM unnest($1::bigint[], $2::bigint[], $3::boolean[]) AS v(id, account_id, filtered)
        "#,
        &ids,
        &accounts,
        &filtered_flags,
        from_account_id,
        status_id,
    )
    .execute(pool)
    .await?;
    // After the INSERT, like the per-recipient path: the request row's count
    // includes the notification just stored.
    crate::notification_request::record_filtered_many(
        pool,
        &filtered_accounts,
        from_account_id,
        "mention",
        Some(status_id),
    )
    .await?;
    Ok(())
}

/// Records the Mastodon-compatible `admin.report` staff notification —
/// `from_account` is the reporter, `report_id` the filed report the entity
/// embeds. Same pipeline as every other kind (the self-notification guard
/// covers a staff member reporting, and the recipient's notification policy
/// applies exactly as it does to `admin.sign_up`).
pub async fn create_admin_report(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    report_id: i64,
) -> Result<(), DbError> {
    create_inner(
        pool,
        account_id,
        from_account_id,
        "admin.report",
        None,
        None,
        Some(report_id),
    )
    .await
}

/// Whether any staff `admin.sign_up` notification already points at this
/// applicant. The idempotency guard behind the sign-up ping: it fires at the
/// earliest of {awaiting review, becoming functional}, and this check keeps a
/// later approval from notifying staff about the same applicant twice.
pub async fn sign_up_exists(pool: &PgPool, from_account_id: i64) -> Result<bool, DbError> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM notifications
            WHERE kind = 'admin.sign_up' AND from_account_id = $1
        ) AS "exists!"
        "#,
        from_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Records a `pleroma:emoji_reaction` notification, carrying the reacted
/// `emoji` (Unicode or `:shortcode:`) so the client can render it. Subject to
/// the same self/mute/policy gating as every other notification.
pub async fn create_reaction(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    status_id: i64,
    emoji: &str,
) -> Result<(), DbError> {
    create_inner(
        pool,
        account_id,
        from_account_id,
        "pleroma:emoji_reaction",
        Some(status_id),
        Some(emoji),
        None,
    )
    .await
}

async fn create_inner(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
    emoji: Option<&str>,
    report_id: Option<i64>,
) -> Result<(), DbError> {
    if account_id == from_account_id && kind != "poll" {
        return Ok(());
    }
    // A notification whose target status sits in a thread the recipient has
    // muted is dropped, never stored — matching the muted-conversation drop
    // in Mastodon's `NotifyService`.
    if let Some(status_id) = status_id
        && crate::conversation::status_muted(pool, account_id, status_id).await?
    {
        return Ok(());
    }
    // Notification filtering policy (Mastodon's NotifyService): a matching
    // sender is either dropped (no row) or stored hidden (`filtered = TRUE`)
    // and rolled up into a per-sender request.
    let disposition =
        crate::notification_policy::evaluate(pool, account_id, from_account_id, kind, status_id)
            .await?;
    if disposition == crate::notification_policy::Disposition::Drop {
        return Ok(());
    }
    let filtered = disposition == crate::notification_policy::Disposition::Filter;
    let group_key = next_group_key(pool, account_id, kind, status_id).await?;
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, status_id, emoji, group_key, filtered, report_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        id::next(),
        account_id,
        from_account_id,
        kind,
        status_id,
        emoji,
        group_key,
        filtered,
        report_id,
    )
    .execute(pool)
    .await?;
    if filtered {
        crate::notification_request::record_filtered(
            pool,
            account_id,
            from_account_id,
            kind,
            status_id,
        )
        .await?;
    }
    Ok(())
}

/// Records a `moderation_warning` notification pointing at a strike. Like
/// Mastodon, the sender is the warned account itself — the acting moderator is
/// never exposed — which also exempts it from the self-notification skip,
/// thread mutes and filtering policies (`NotifyService`'s `DropCondition`
/// lists `moderation_warning` among the never-dropped kinds).
pub async fn create_moderation_warning(
    pool: &PgPool,
    account_id: i64,
    warning_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, account_warning_id)
        VALUES ($1, $2, $2, 'moderation_warning', $3)
        "#,
        id::next(),
        account_id,
        warning_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the `pleroma:emoji_reaction` notification a now-undone reaction
/// raised, so an `Undo(EmojiReact)` leaves no dangling notification.
pub async fn clear_reaction(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    status_id: i64,
    emoji: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = $1 AND from_account_id = $2 AND status_id = $3
           AND kind = 'pleroma:emoji_reaction' AND emoji = $4",
        account_id,
        from_account_id,
        status_id,
        emoji,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`clear_reaction`] for a whole set of withdrawn reactions on one status in
/// a single statement.
pub async fn clear_reactions(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    status_id: i64,
    emojis: &[String],
) -> Result<(), DbError> {
    if emojis.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = $1 AND from_account_id = $2 AND status_id = $3
           AND kind = 'pleroma:emoji_reaction' AND emoji = ANY($4)",
        account_id,
        from_account_id,
        status_id,
        emojis,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether an equivalent notification has already been stored.
pub async fn exists(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM notifications
            WHERE account_id = $1
              AND from_account_id = $2
              AND kind = $3
              AND status_id IS NOT DISTINCT FROM $4
        ) AS "found!"
        "#,
        account_id,
        from_account_id,
        kind,
        status_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(found)
}

/// Records a collection notification (`added_to_collection` or
/// `collection_update`) — it points at a collection rather than a status, and
/// its sender is the collection owner. Self-notifications are skipped, like
/// Mastodon's `NotifyService`.
pub async fn create_for_collection(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    collection_id: i64,
) -> Result<(), DbError> {
    if account_id == from_account_id {
        return Ok(());
    }
    sqlx::query!(
        r#"
        INSERT INTO notifications (id, account_id, from_account_id, kind, collection_id)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id::next(),
        account_id,
        from_account_id,
        kind,
        collection_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Optional narrowing of a notification listing — Mastodon's `browserable`
/// scope: by kind (`types[]`/`exclude_types[]`, pre-resolved by the caller
/// into the allowed set) and by sender (`account_id`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationFilter<'a> {
    /// `Some` restricts to these kinds; `None` means no kind filter.
    pub kinds: Option<&'a [String]>,
    pub from_account_id: Option<i64>,
    /// Include policy-filtered notifications (Mastodon's `include_filtered`).
    /// Filtering by `from_account_id` also surfaces them, matching Mastodon's
    /// `browserable` scope.
    pub include_filtered: bool,
}

impl NotificationFilter<'_> {
    /// Whether policy-filtered rows should be shown — explicitly requested, or
    /// implied by a per-sender filter.
    fn shows_filtered(&self) -> bool {
        self.include_filtered || self.from_account_id.is_some()
    }
}

/// Newest-first notifications for an account, keyset-paginated. `min_id`
/// selects the page just above it (Mastodon's `paginate_by_min_id`),
/// `since_id` the newest page bounded below. Notifications from blocked
/// senders, or from senders muted with `hide_notifications`, never appear.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    filter: NotificationFilter<'_>,
    limit: i64,
) -> Result<Vec<Notification>, DbError> {
    let shows_filtered = filter.shows_filtered();
    if let Some(min_id) = min_id {
        // The page of oldest-first results just above min_id, presented
        // newest-first like every other page.
        let mut notifications = sqlx::query_as!(
            Notification,
            r#"
            SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
            FROM notifications n
            WHERE account_id = $1 AND id > $2
              AND ($3::bigint IS NULL OR id < $3)
              AND ($5::text[] IS NULL OR kind = ANY($5))
              AND ($6::bigint IS NULL OR from_account_id = $6)
              AND ($7 OR NOT filtered)
              AND NOT sender_filtered($1, n.from_account_id)
            ORDER BY id ASC
            LIMIT $4
            "#,
            account_id,
            min_id,
            max_id,
            limit,
            filter.kinds,
            filter.from_account_id,
            shows_filtered,
        )
        .fetch_all(pool)
        .await?;
        notifications.reverse();
        return Ok(notifications);
    }
    let notifications = sqlx::query_as!(
        Notification,
        r#"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications n
        WHERE account_id = $1
          AND ($2::bigint IS NULL OR id < $2)
          AND ($3::bigint IS NULL OR id > $3)
          AND ($5::text[] IS NULL OR kind = ANY($5))
          AND ($6::bigint IS NULL OR from_account_id = $6)
          AND ($7 OR NOT filtered)
          AND NOT sender_filtered($1, n.from_account_id)
        ORDER BY id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
        filter.kinds,
        filter.from_account_id,
        shows_filtered,
    )
    .fetch_all(pool)
    .await?;
    Ok(notifications)
}

/// The grouped-notification key of a row: its `group_key` when the kind is
/// being grouped, else the synthetic `ungrouped-{id}`.
#[must_use]
pub fn effective_group_key(item: &Notification, grouped_kinds: Option<&[String]>) -> String {
    let grouped = grouped_kinds.is_none_or(|kinds| kinds.iter().any(|k| k == &item.kind));
    match (&item.group_key, grouped) {
        (Some(key), true) => key.clone(),
        _ => format!("ungrouped-{}", item.id),
    }
}

/// One page of notifications with at most one row per group — Mastodon's
/// `paginate_groups` recursive CTE. The newest row of each of the first
/// `limit` distinct groups below `max_id` (and above `since_id`) is
/// returned, newest first. `grouped_kinds` limits which kinds group
/// (`None` = every groupable kind); the [`list`] filters apply unchanged.
pub async fn list_grouped(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    filter: NotificationFilter<'_>,
    grouped_kinds: Option<&[String]>,
    limit: i64,
) -> Result<Vec<Notification>, DbError> {
    let shows_filtered = filter.shows_filtered();
    let notifications = sqlx::query_as!(
        Notification,
        r#"
        WITH RECURSIVE grouped AS (
            (
                SELECT n.id, n.account_id, n.from_account_id, n.kind, n.status_id,
                       n.collection_id, n.emoji, n.created_at, n.group_key, n.filtered,
                       n.account_warning_id, n.report_id,
                       ARRAY[COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                           THEN n.group_key END,
                                      'ungrouped-' || n.id)] AS seen
                FROM notifications n
                WHERE n.account_id = $1
                  AND ($2::bigint IS NULL OR n.id < $2)
                  AND ($3::bigint IS NULL OR n.id > $3)
                  AND ($6::text[] IS NULL OR n.kind = ANY($6))
                  AND ($7::bigint IS NULL OR n.from_account_id = $7)
                  AND ($8 OR NOT n.filtered)
                  AND NOT sender_filtered($1, n.from_account_id)
                ORDER BY n.id DESC
                LIMIT 1
            )
            UNION ALL
            SELECT step.id, step.account_id, step.from_account_id, step.kind,
                   step.status_id, step.collection_id, step.emoji, step.created_at, step.group_key,
                   step.filtered, step.account_warning_id, step.report_id, wt.seen || step.gkey
            FROM (SELECT id, seen FROM grouped WHERE cardinality(seen) < $4::bigint) AS wt
            CROSS JOIN LATERAL (
                SELECT n.id, n.account_id, n.from_account_id, n.kind, n.status_id,
                       n.collection_id, n.emoji, n.created_at, n.group_key, n.filtered,
                       n.account_warning_id, n.report_id,
                       COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                     THEN n.group_key END,
                                'ungrouped-' || n.id) AS gkey
                FROM notifications n
                WHERE n.account_id = $1 AND n.id < wt.id
                  AND ($3::bigint IS NULL OR n.id > $3)
                  AND ($6::text[] IS NULL OR n.kind = ANY($6))
                  AND ($7::bigint IS NULL OR n.from_account_id = $7)
                  AND ($8 OR NOT n.filtered)
                  AND NOT sender_filtered($1, n.from_account_id)
                  AND COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                    THEN n.group_key END,
                               'ungrouped-' || n.id) <> ALL(wt.seen)
                ORDER BY n.id DESC
                LIMIT 1
            ) AS step
        )
        SELECT id AS "id!", account_id AS "account_id!",
               from_account_id AS "from_account_id!", kind AS "kind!",
               status_id, collection_id, emoji, created_at AS "created_at!", group_key,
               filtered AS "filtered!", account_warning_id, report_id
        FROM grouped
        ORDER BY id DESC
        "#,
        account_id,
        max_id,
        since_id,
        limit,
        grouped_kinds,
        filter.kinds,
        filter.from_account_id,
        shows_filtered,
    )
    .fetch_all(pool)
    .await?;
    Ok(notifications)
}

/// The min-id sibling of [`list_grouped`]: the first `limit` distinct groups
/// strictly above `min_id`, walking upward (Mastodon's
/// `paginate_groups_by_min_id`), presented newest-first like every page.
pub async fn list_grouped_above(
    pool: &PgPool,
    account_id: i64,
    min_id: i64,
    max_id: Option<i64>,
    filter: NotificationFilter<'_>,
    grouped_kinds: Option<&[String]>,
    limit: i64,
) -> Result<Vec<Notification>, DbError> {
    let shows_filtered = filter.shows_filtered();
    let mut notifications = sqlx::query_as!(
        Notification,
        r#"
        WITH RECURSIVE grouped AS (
            (
                SELECT n.id, n.account_id, n.from_account_id, n.kind, n.status_id,
                       n.collection_id, n.emoji, n.created_at, n.group_key, n.filtered,
                       n.account_warning_id, n.report_id,
                       ARRAY[COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                           THEN n.group_key END,
                                      'ungrouped-' || n.id)] AS seen
                FROM notifications n
                WHERE n.account_id = $1 AND n.id > $2
                  AND ($3::bigint IS NULL OR n.id < $3)
                  AND ($6::text[] IS NULL OR n.kind = ANY($6))
                  AND ($7::bigint IS NULL OR n.from_account_id = $7)
                  AND ($8 OR NOT n.filtered)
                  AND NOT sender_filtered($1, n.from_account_id)
                ORDER BY n.id ASC
                LIMIT 1
            )
            UNION ALL
            SELECT step.id, step.account_id, step.from_account_id, step.kind,
                   step.status_id, step.collection_id, step.emoji, step.created_at, step.group_key,
                   step.filtered, step.account_warning_id, step.report_id, wt.seen || step.gkey
            FROM (SELECT id, seen FROM grouped WHERE cardinality(seen) < $4::bigint) AS wt
            CROSS JOIN LATERAL (
                SELECT n.id, n.account_id, n.from_account_id, n.kind, n.status_id,
                       n.collection_id, n.emoji, n.created_at, n.group_key, n.filtered,
                       n.account_warning_id, n.report_id,
                       COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                     THEN n.group_key END,
                                'ungrouped-' || n.id) AS gkey
                FROM notifications n
                WHERE n.account_id = $1 AND n.id > wt.id
                  AND ($3::bigint IS NULL OR n.id < $3)
                  AND ($6::text[] IS NULL OR n.kind = ANY($6))
                  AND ($7::bigint IS NULL OR n.from_account_id = $7)
                  AND ($8 OR NOT n.filtered)
                  AND NOT sender_filtered($1, n.from_account_id)
                  AND COALESCE(CASE WHEN $5::text[] IS NULL OR n.kind = ANY($5)
                                    THEN n.group_key END,
                               'ungrouped-' || n.id) <> ALL(wt.seen)
                ORDER BY n.id ASC
                LIMIT 1
            ) AS step
        )
        SELECT id AS "id!", account_id AS "account_id!",
               from_account_id AS "from_account_id!", kind AS "kind!",
               status_id, collection_id, emoji, created_at AS "created_at!", group_key,
               filtered AS "filtered!", account_warning_id, report_id
        FROM grouped
        ORDER BY id ASC
        "#,
        account_id,
        min_id,
        max_id,
        limit,
        grouped_kinds,
        filter.kinds,
        filter.from_account_id,
        shows_filtered,
    )
    .fetch_all(pool)
    .await?;
    notifications.reverse();
    Ok(notifications)
}

/// Aggregates of one notification group, computed over the recipient's
/// whole group history (bounded above by the page window, like Mastodon).
#[derive(Debug, Clone)]
pub struct GroupData {
    pub group_key: String,
    pub most_recent_id: i64,
    /// Newest-first senders, at most 8 (Mastodon's `SAMPLE_ACCOUNTS_SIZE`).
    pub sample_account_ids: Vec<i64>,
    pub notifications_count: i64,
    /// Oldest group member at/above the page's lower bound.
    pub min_id: Option<i64>,
    pub latest_at: OffsetDateTime,
}

/// Group aggregates for a set of group keys. `upper_bound` (inclusive)
/// caps every aggregate to the current page window so pages stay stable
/// while new notifications arrive; `lower_bound` only anchors `min_id`.
pub async fn groups_data(
    pool: &PgPool,
    account_id: i64,
    group_keys: &[String],
    lower_bound: i64,
    upper_bound: Option<i64>,
) -> Result<Vec<GroupData>, DbError> {
    if group_keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        GroupData,
        r#"
        SELECT g.group_key AS "group_key!",
               (SELECT max(n.id) FROM notifications n
                 WHERE n.account_id = $1 AND n.group_key = g.group_key
                   AND ($4::bigint IS NULL OR n.id <= $4)) AS "most_recent_id!",
               ARRAY(SELECT n.from_account_id FROM notifications n
                      WHERE n.account_id = $1 AND n.group_key = g.group_key
                        AND ($4::bigint IS NULL OR n.id <= $4)
                      ORDER BY n.id DESC LIMIT 8) AS "sample_account_ids!",
               (SELECT count(*) FROM notifications n
                 WHERE n.account_id = $1 AND n.group_key = g.group_key
                   AND ($4::bigint IS NULL OR n.id <= $4)) AS "notifications_count!",
               (SELECT min(n.id) FROM notifications n
                 WHERE n.account_id = $1 AND n.group_key = g.group_key
                   AND n.id >= $3) AS min_id,
               (SELECT max(n.created_at) FROM notifications n
                 WHERE n.account_id = $1 AND n.group_key = g.group_key
                   AND ($4::bigint IS NULL OR n.id <= $4)) AS "latest_at!"
        FROM unnest($2::text[]) AS g(group_key)
        "#,
        account_id,
        group_keys,
        lower_bound,
        upper_bound,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The newest notification of a group — `ungrouped-{id}` keys resolve by
/// id, like Mastodon's `by_group_key` scope.
pub async fn find_group_head(
    pool: &PgPool,
    account_id: i64,
    group_key: &str,
) -> Result<Option<Notification>, DbError> {
    if let Some(raw_id) = group_key.strip_prefix("ungrouped-") {
        let Ok(id) = raw_id.parse::<i64>() else {
            return Ok(None);
        };
        return find_by_id(pool, account_id, id).await;
    }
    let notification = sqlx::query_as!(
        Notification,
        r#"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications
        WHERE account_id = $1 AND group_key = $2
        ORDER BY id DESC
        LIMIT 1
        "#,
        account_id,
        group_key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(notification)
}

/// Deletes a whole notification group (Mastodon's v2 dismiss). Dismissing
/// a group that does not exist is a no-op, like Mastodon's `destroy_all`.
pub async fn dismiss_group(pool: &PgPool, account_id: i64, group_key: &str) -> Result<(), DbError> {
    if let Some(raw_id) = group_key.strip_prefix("ungrouped-") {
        if let Ok(id) = raw_id.parse::<i64>() {
            dismiss(pool, account_id, id).await?;
        }
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND group_key = $2",
        account_id,
        group_key,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// One page of a group's members, newest first — the v2 accounts listing.
/// Synthetic `ungrouped-…` keys never match (the column comparison fails),
/// like Mastodon's accounts controller, which queries the raw column.
pub async fn group_members(
    pool: &PgPool,
    account_id: i64,
    group_key: &str,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Notification>, DbError> {
    let notifications = sqlx::query_as!(
        Notification,
        r#"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications
        WHERE account_id = $1 AND group_key = $2
          AND ($3::bigint IS NULL OR id < $3)
          AND ($4::bigint IS NULL OR id > $4)
        ORDER BY id DESC
        LIMIT $5
        "#,
        account_id,
        group_key,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(notifications)
}

/// A single notification by id, unscoped — for background workers (the
/// push dispatcher) that hold a job reference rather than a viewer.
pub async fn find(pool: &PgPool, id: i64) -> Result<Option<Notification>, DbError> {
    let notification = sqlx::query_as!(
        Notification,
        r#"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(notification)
}

/// A single notification, scoped to its recipient.
pub async fn find_by_id(
    pool: &PgPool,
    account_id: i64,
    id: i64,
) -> Result<Option<Notification>, DbError> {
    let notification = sqlx::query_as!(
        Notification,
        r#"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id, emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications
        WHERE account_id = $1 AND id = $2
        "#,
        account_id,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(notification)
}

/// Deletes one notification; `false` when it does not exist (or belongs to
/// someone else).
pub async fn dismiss(pool: &PgPool, account_id: i64, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND id = $2",
        account_id,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes all of an account's notifications.
pub async fn clear(pool: &PgPool, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1",
        account_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes an account's notifications from one sender — blocking someone
/// erases their traces, like Mastodon's `AfterBlockService`. Executor-generic
/// so the erase commits with the block row and its outbox job.
pub async fn clear_from<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    from_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        from_account_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Deletes an account's notifications from every sender on a blocked domain.
pub async fn clear_from_domain<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    domain: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        DELETE FROM notifications n
        USING accounts sender
        WHERE n.account_id = $1
          AND n.from_account_id = sender.id
          AND sender.domain = $2
        "#,
        account_id,
        domain,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes an account's notifications of one kind from one sender — e.g.
/// the `follow_request` notification once the request is resolved (Mastodon
/// destroys it with the `FollowRequest` row). Executor-generic so the removal
/// commits with the follow-state change and its outbox job.
pub async fn clear_kind_from<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = $1 AND from_account_id = $2 AND kind = $3",
        account_id,
        from_account_id,
        kind,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Batched [`clear_kind_from`]: one statement clears the kind from every
/// sender in `from_account_ids` — the `Move` replay's unfollow cleanup over
/// the whole follower set.
pub async fn clear_kind_from_many<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    from_account_ids: &[i64],
    kind: &str,
) -> Result<(), DbError> {
    if from_account_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = $1 AND from_account_id = ANY($2) AND kind = $3",
        account_id,
        from_account_ids,
        kind,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Batched [`clear_from`] across recipients: one statement erases one
/// sender's notifications for every account in `account_ids` — the `Move`
/// replay's carried-block cleanup over the whole blocker set.
pub async fn clear_from_many<'e, E: PgExecutor<'e>>(
    executor: E,
    account_ids: &[i64],
    from_account_id: i64,
) -> Result<(), DbError> {
    if account_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = ANY($1) AND from_account_id = $2",
        account_ids,
        from_account_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Deletes an account's notifications of one kind from one sender about one
/// status — a retracted favourite or boost takes its notification with it
/// (Mastodon destroys the notification with the interaction row).
pub async fn clear_kind_for_status<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM notifications
         WHERE account_id = $1 AND from_account_id = $2 AND kind = $3 AND status_id = $4",
        account_id,
        from_account_id,
        kind,
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Notifications newer than `min_id` (all of them when `None`), counted up
/// to `limit` — Mastodon caps the count rather than scanning everything.
/// The same browsing filter as [`list`] applies, like Mastodon's
/// `unread_count` reusing its `browserable` scope.
pub async fn unread_count(
    pool: &PgPool,
    account_id: i64,
    min_id: Option<i64>,
    filter: NotificationFilter<'_>,
    limit: i64,
) -> Result<i64, DbError> {
    let shows_filtered = filter.shows_filtered();
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM (
            SELECT 1 AS one
            FROM notifications n
            WHERE account_id = $1 AND ($2::bigint IS NULL OR id > $2)
              AND ($4::text[] IS NULL OR kind = ANY($4))
              AND ($5::bigint IS NULL OR from_account_id = $5)
              AND ($6 OR NOT filtered)
              AND NOT sender_filtered($1, n.from_account_id)
            LIMIT $3
        ) AS capped
        "#,
        account_id,
        min_id,
        limit,
        filter.kinds,
        filter.from_account_id,
        shows_filtered,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Lemmy's three unread badge counters projected from Plamenu's native state.
///
/// A visible `mention` notification is a reply when its target status replies
/// to the recipient; otherwise it remains a mention. Plamenu stores direct
/// read state per conversation rather than per message, so
/// `private_messages` is the number of distinct unread, unmuted conversations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LemmyUnreadCounts {
    pub replies: i64,
    pub mentions: i64,
    pub private_messages: i64,
}

pub async fn lemmy_unread_counts(
    pool: &PgPool,
    user_id: i64,
    account_id: i64,
) -> Result<LemmyUnreadCounts, DbError> {
    let (replies, mentions, private_messages) = sqlx::query_as::<_, (i64, i64, i64)>(
        r"
        WITH marker_value AS (
            SELECT COALESCE(
                (SELECT last_read_id FROM markers
                 WHERE user_id = $1 AND timeline = 'notifications'),
                0) AS last_read_id
        ), unread_mentions AS (
            -- The hot branch retains the notifications(account_id, id) range
            -- scan. A sparse explicit-read override only subtracts from it.
            SELECT n.id, n.status_id, n.from_account_id, n.filtered
            FROM notifications n CROSS JOIN marker_value m
            WHERE n.account_id = $2
              AND n.kind = 'mention'
              AND n.id > m.last_read_id
              AND NOT EXISTS (
                  SELECT 1 FROM lemmy_notification_read_overrides o
                  WHERE o.account_id = $2 AND o.notification_id = n.id AND o.read)
            UNION ALL
            -- Explicit unread below the marker is sparse and starts from the
            -- override table's account/id primary key, never a notification
            -- history scan.
            SELECT n.id, n.status_id, n.from_account_id, n.filtered
            FROM lemmy_notification_read_overrides o
            JOIN notifications n ON n.id = o.notification_id
            CROSS JOIN marker_value m
            WHERE o.account_id = $2 AND NOT o.read
              AND n.account_id = $2 AND n.kind = 'mention'
              AND n.id <= m.last_read_id
        ), mention_counts AS (
            SELECT
                COUNT(*) FILTER (
                    WHERE s.in_reply_to_account_id = $2
                )::bigint AS replies,
                COUNT(*) FILTER (
                    WHERE s.in_reply_to_account_id IS DISTINCT FROM $2
                )::bigint AS mentions
            FROM unread_mentions n
            JOIN statuses s ON s.id = n.status_id
            WHERE NOT n.filtered
              AND s.deleted_at IS NULL -- STUBFILTER
              AND NOT sender_filtered($2, n.from_account_id)
        ), direct_counts AS (
            SELECT COUNT(DISTINCT ac.conversation_id)::bigint AS private_messages
            FROM account_conversations ac
            WHERE ac.account_id = $2
              AND ac.unread
              AND NOT EXISTS (
                  SELECT 1 FROM conversation_mutes cm
                  WHERE cm.account_id = $2
                    AND cm.conversation_id = ac.conversation_id)
        )
        SELECT mention_counts.replies,
               mention_counts.mentions,
               direct_counts.private_messages
        FROM mention_counts CROSS JOIN direct_counts
        ",
    )
    .bind(user_id)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(LemmyUnreadCounts {
        replies,
        mentions,
        private_messages,
    })
}

/// Whether any notification the account can browse (the default listing's
/// visibility: policy-filtered rows and filtered senders excluded) is newer
/// than the user's `notifications` marker. A single indexed probe, cheap
/// enough to run on every web page render for the navigation bell's dot.
pub async fn has_unread(pool: &PgPool, user_id: i64, account_id: i64) -> Result<bool, DbError> {
    let unread = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM notifications n
            WHERE n.account_id = $2
              AND n.id > COALESCE(
                  (SELECT last_read_id FROM markers
                   WHERE user_id = $1 AND timeline = 'notifications'),
                  0)
              AND NOT n.filtered
              AND NOT sender_filtered($2, n.from_account_id)
        ) AS "unread!"
        "#,
        user_id,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(unread)
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

    async fn stored_status_rows(pool: &PgPool) -> Vec<(i64, i64, String, Option<i64>, bool)> {
        sqlx::query!(
            "SELECT account_id, from_account_id, kind, status_id, group_key, filtered
             FROM notifications ORDER BY account_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            assert!(row.group_key.is_none(), "status never groups");
            (
                row.account_id,
                row.from_account_id,
                row.kind,
                row.status_id,
                row.filtered,
            )
        })
        .collect()
    }

    /// [`create_status_many`] must store exactly what per-recipient [`create`]
    /// stores for `kind = status`: the self-notification skip and the
    /// muted-conversation drop apply, and nothing is filtered or grouped.
    #[sqlx::test]
    async fn create_status_many_matches_the_per_recipient_pipeline(pool: PgPool) {
        let author = local(&pool, "author").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let status = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        let conversation = crate::conversation::ensure_for_status(
            &pool,
            &crate::conversation::EnsureConversation {
                status_id: status.id,
                account_id: author,
                in_reply_to_id: None,
                is_reply: false,
                refs: crate::conversation::ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        crate::conversation::mute(&pool, carol, conversation)
            .await
            .unwrap();

        let recipients = [bob, author, carol];
        for recipient in recipients {
            create(&pool, recipient, author, "status", Some(status.id))
                .await
                .unwrap();
        }
        let per_item = stored_status_rows(&pool).await;
        assert_eq!(
            per_item,
            vec![(bob, author, "status".to_string(), Some(status.id), false)],
            "author self-skips, carol's thread mute drops hers"
        );
        sqlx::query!("DELETE FROM notifications")
            .execute(&pool)
            .await
            .unwrap();

        create_status_many(&pool, &recipients, author, status.id)
            .await
            .unwrap();
        assert_eq!(stored_status_rows(&pool).await, per_item);
    }

    /// [`create_ungroupable_many`] must store exactly what per-recipient
    /// [`create`] stores for an unfilterable, ungroupable kind (`update` here):
    /// the self-notification skip and the muted-conversation drop apply per
    /// recipient's own target status, and nothing is filtered or grouped.
    #[sqlx::test]
    async fn create_ungroupable_many_matches_the_per_recipient_pipeline(pool: PgPool) {
        let author = local(&pool, "author").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let status = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        let other = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>again</p>", "public", None),
        )
        .await
        .unwrap();
        let conversation = crate::conversation::ensure_for_status(
            &pool,
            &crate::conversation::EnsureConversation {
                status_id: status.id,
                account_id: author,
                in_reply_to_id: None,
                is_reply: false,
                refs: crate::conversation::ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        crate::conversation::mute(&pool, carol, conversation)
            .await
            .unwrap();

        // Carol's thread mute covers `status`, not `other` — her second pair
        // must survive while the first drops, proving the drop is evaluated
        // against each recipient's own target status.
        let recipients = [
            (bob, status.id),
            (author, status.id),
            (carol, status.id),
            (carol, other.id),
        ];
        for (recipient, target) in recipients {
            create(&pool, recipient, author, "update", Some(target))
                .await
                .unwrap();
        }
        let per_item = stored_update_rows(&pool).await;
        assert_eq!(
            per_item,
            vec![
                (bob, author, "update".to_string(), Some(status.id), false),
                (carol, author, "update".to_string(), Some(other.id), false),
            ],
            "author self-skips, carol's thread mute drops only the muted status"
        );
        sqlx::query!("DELETE FROM notifications")
            .execute(&pool)
            .await
            .unwrap();

        create_ungroupable_many(&pool, &recipients, author, "update")
            .await
            .unwrap();
        assert_eq!(stored_update_rows(&pool).await, per_item);
    }

    /// Every stored notification row plus every request row — the complete
    /// observable output of the mention-filing pipeline.
    #[allow(clippy::type_complexity)]
    async fn stored_mention_output(
        pool: &PgPool,
    ) -> (
        Vec<(i64, i64, Option<i64>, bool)>,
        Vec<(i64, i64, Option<i64>, i64)>,
    ) {
        let notifications = sqlx::query!(
            "SELECT account_id, from_account_id, kind, status_id, group_key, filtered
             FROM notifications ORDER BY account_id, status_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            assert_eq!(row.kind, "mention");
            assert!(row.group_key.is_none(), "mention never groups");
            (
                row.account_id,
                row.from_account_id,
                row.status_id,
                row.filtered,
            )
        })
        .collect();
        let requests = sqlx::query!(
            "SELECT account_id, from_account_id, last_status_id, notifications_count
             FROM notification_requests ORDER BY account_id, from_account_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.account_id,
                row.from_account_id,
                row.last_status_id,
                row.notifications_count,
            )
        })
        .collect();
        (notifications, requests)
    }

    /// The batched mention filing must produce exactly the rows and request
    /// upserts of the per-recipient pipeline, across every policy arm: default
    /// accept, muted conversation, not-following filter/drop, permission
    /// override, active follow, not-follower filter (recent and old edges),
    /// new-account drop, self-skip, and the unsolicited-DM walk (root DM,
    /// unsolicited reply, solicited reply).
    #[sqlx::test]
    async fn create_mentions_many_matches_the_per_recipient_pipeline(pool: PgPool) {
        use crate::notification_policy::{Disposition, Policy, upsert as upsert_policy};

        let author = local(&pool, "author").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dana = local(&pool, "dana").await;
        let erin = local(&pool, "erin").await;
        let frank = local(&pool, "frank").await;
        let grace = local(&pool, "grace").await;
        let kate = local(&pool, "kate").await;
        let leo = local(&pool, "leo").await;
        let mel = local(&pool, "mel").await;
        let ivan = local(&pool, "ivan").await;
        let judy = local(&pool, "judy").await;

        let filter_not_following = Policy {
            for_not_following: Disposition::Filter,
            ..Policy::default()
        };
        upsert_policy(&pool, dana, filter_not_following)
            .await
            .unwrap();
        upsert_policy(&pool, frank, filter_not_following)
            .await
            .unwrap();
        upsert_policy(&pool, grace, filter_not_following)
            .await
            .unwrap();
        upsert_policy(
            &pool,
            erin,
            Policy {
                for_not_following: Disposition::Drop,
                ..Policy::default()
            },
        )
        .await
        .unwrap();
        let filter_not_followers = Policy {
            for_not_followers: Disposition::Filter,
            ..Policy::default()
        };
        upsert_policy(&pool, kate, filter_not_followers)
            .await
            .unwrap();
        upsert_policy(&pool, leo, filter_not_followers)
            .await
            .unwrap();
        upsert_policy(
            &pool,
            mel,
            Policy {
                for_new_accounts: Disposition::Drop,
                ..Policy::default()
            },
        )
        .await
        .unwrap();
        let filter_dms = Policy {
            for_private_mentions: Disposition::Filter,
            ..Policy::default()
        };
        upsert_policy(&pool, ivan, filter_dms).await.unwrap();
        upsert_policy(&pool, judy, filter_dms).await.unwrap();

        // frank holds a permission override; grace actively follows the
        // author; the author follows leo since well before the new-follower
        // window (kate gets no edge, so she stays a non-follower).
        sqlx::query("INSERT INTO notification_permissions (id, account_id, from_account_id) VALUES ($1, $2, $3)")
            .bind(id::next())
            .bind(frank)
            .bind(author)
            .execute(&pool)
            .await
            .unwrap();
        crate::follow::create(&pool, grace, author, None)
            .await
            .unwrap();
        crate::follow::create(&pool, author, leo, None)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE follows SET created_at = now() - interval '10 days'
             WHERE account_id = $1 AND target_account_id = $2",
        )
        .bind(author)
        .bind(leo)
        .execute(&pool)
        .await
        .unwrap();

        let post = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>hi all</p>", "public", None),
        )
        .await
        .unwrap();
        let conversation = crate::conversation::ensure_for_status(
            &pool,
            &crate::conversation::EnsureConversation {
                status_id: post.id,
                account_id: author,
                in_reply_to_id: None,
                is_reply: false,
                refs: crate::conversation::ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        crate::conversation::mute(&pool, carol, conversation)
            .await
            .unwrap();

        // A root DM (unsolicited for everyone), and a DM replying into a
        // thread judy opened by DM'ing the author (solicited for judy only).
        let root_dm = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>psst</p>", "direct", None),
        )
        .await
        .unwrap();
        let judy_dm = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(judy, "<p>hello author</p>", "direct", None),
        )
        .await
        .unwrap();
        crate::mention::attach_many(&pool, judy_dm.id, &[(author, false)])
            .await
            .unwrap();
        let reply_dm = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(author, "<p>re</p>", "direct", Some(judy_dm.id)),
        )
        .await
        .unwrap();

        let post_recipients = [author, bob, carol, dana, erin, frank, grace, kate, leo, mel];
        let dm_recipients = [ivan, judy];

        // Per-recipient pipeline first, in call-site order (post, then DMs).
        for recipient in post_recipients {
            create(&pool, recipient, author, "mention", Some(post.id))
                .await
                .unwrap();
        }
        for recipient in dm_recipients {
            create(&pool, recipient, author, "mention", Some(root_dm.id))
                .await
                .unwrap();
        }
        for recipient in dm_recipients {
            create(&pool, recipient, author, "mention", Some(reply_dm.id))
                .await
                .unwrap();
        }
        let per_item = stored_mention_output(&pool).await;
        assert_eq!(
            per_item.0,
            vec![
                (bob, author, Some(post.id), false),
                (dana, author, Some(post.id), true),
                (frank, author, Some(post.id), false),
                (grace, author, Some(post.id), false),
                (kate, author, Some(post.id), true),
                (leo, author, Some(post.id), false),
                (ivan, author, Some(root_dm.id), true),
                (ivan, author, Some(reply_dm.id), true),
                (judy, author, Some(root_dm.id), true),
                (judy, author, Some(reply_dm.id), false),
            ],
            "the per-recipient pipeline exercises every arm as intended"
        );
        assert_eq!(
            per_item.1,
            vec![
                (dana, author, Some(post.id), 1),
                (kate, author, Some(post.id), 1),
                (ivan, author, Some(reply_dm.id), 2),
                (judy, author, Some(root_dm.id), 1),
            ],
            "filtered mentions roll up into per-sender requests"
        );
        sqlx::query!("DELETE FROM notifications")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query!("DELETE FROM notification_requests")
            .execute(&pool)
            .await
            .unwrap();

        create_mentions_many(&pool, &post_recipients, author, post.id)
            .await
            .unwrap();
        create_mentions_many(&pool, &dm_recipients, author, root_dm.id)
            .await
            .unwrap();
        create_mentions_many(&pool, &dm_recipients, author, reply_dm.id)
            .await
            .unwrap();
        assert_eq!(stored_mention_output(&pool).await, per_item);
    }

    async fn stored_update_rows(pool: &PgPool) -> Vec<(i64, i64, String, Option<i64>, bool)> {
        sqlx::query!(
            "SELECT account_id, from_account_id, kind, status_id, group_key, filtered
             FROM notifications ORDER BY account_id, status_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            assert!(row.group_key.is_none(), "update never groups");
            (
                row.account_id,
                row.from_account_id,
                row.kind,
                row.status_id,
                row.filtered,
            )
        })
        .collect()
    }

    #[sqlx::test]
    async fn has_unread_follows_the_marker(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let user_id = crate::user::create(&pool, alice, Some("alice@example.test"), "hash")
            .await
            .unwrap()
            .id;

        assert!(!has_unread(&pool, user_id, alice).await.unwrap());

        create(&pool, alice, carol, "follow", None).await.unwrap();
        create(&pool, alice, carol, "favourite", None)
            .await
            .unwrap();
        assert!(has_unread(&pool, user_id, alice).await.unwrap());

        let newest = list(
            &pool,
            alice,
            None,
            None,
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap()[0]
            .id;
        // A marker just below the newest notification still counts as unread;
        // one at the newest clears it.
        crate::marker::advance(&pool, user_id, "notifications", newest - 1)
            .await
            .unwrap();
        assert!(has_unread(&pool, user_id, alice).await.unwrap());
        crate::marker::advance(&pool, user_id, "notifications", newest)
            .await
            .unwrap();
        assert!(!has_unread(&pool, user_id, alice).await.unwrap());
    }

    #[sqlx::test]
    async fn notifications_list_newest_first_and_skip_self(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;

        create(&pool, alice, carol, "follow", None).await.unwrap();
        create(&pool, alice, carol, "favourite", None)
            .await
            .unwrap();
        // Self-notification is a no-op.
        create(&pool, alice, alice, "follow", None).await.unwrap();

        let items = list(
            &pool,
            alice,
            None,
            None,
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "favourite");
        assert_eq!(items[1].kind, "follow");

        // Pagination.
        let page = list(
            &pool,
            alice,
            Some(items[0].id),
            None,
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].kind, "follow");
        let newer = list(
            &pool,
            alice,
            None,
            Some(items[1].id),
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].kind, "favourite");
        // min_id pages oldest-first above the bound, presented newest-first.
        let above = list(
            &pool,
            alice,
            None,
            None,
            Some(items[1].id),
            NotificationFilter::default(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(above.len(), 1);
        assert_eq!(above[0].kind, "favourite");
        assert!(
            list(
                &pool,
                carol,
                None,
                None,
                None,
                NotificationFilter::default(),
                10
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[sqlx::test]
    async fn unread_count_dismiss_and_clear(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        create(&pool, alice, carol, "follow", None).await.unwrap();
        create(&pool, alice, carol, "favourite", None)
            .await
            .unwrap();
        create(&pool, alice, carol, "reblog", None).await.unwrap();
        let items = list(
            &pool,
            alice,
            None,
            None,
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap();

        // No marker counts everything, capped by the limit.
        assert_eq!(
            unread_count(&pool, alice, None, NotificationFilter::default(), 100)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            unread_count(&pool, alice, None, NotificationFilter::default(), 2)
                .await
                .unwrap(),
            2
        );
        // Only notifications above the read position count.
        let middle = items[1].id;
        assert_eq!(
            unread_count(
                &pool,
                alice,
                Some(middle),
                NotificationFilter::default(),
                100
            )
            .await
            .unwrap(),
            1
        );

        // Dismiss is recipient-scoped.
        assert!(!dismiss(&pool, carol, items[0].id).await.unwrap());
        assert!(dismiss(&pool, alice, items[0].id).await.unwrap());
        assert!(!dismiss(&pool, alice, items[0].id).await.unwrap());
        assert!(
            find_by_id(&pool, alice, items[0].id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            list(
                &pool,
                alice,
                None,
                None,
                None,
                NotificationFilter::default(),
                10
            )
            .await
            .unwrap()
            .len(),
            2
        );

        clear(&pool, alice).await.unwrap();
        assert!(
            list(
                &pool,
                alice,
                None,
                None,
                None,
                NotificationFilter::default(),
                10
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[sqlx::test]
    async fn clear_kind_for_status_scopes_to_the_one_interaction(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let post = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(alice, "<p>a</p>", "public", None),
        )
        .await
        .unwrap();
        let other = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(alice, "<p>b</p>", "public", None),
        )
        .await
        .unwrap();
        create(&pool, alice, carol, "favourite", Some(post.id))
            .await
            .unwrap();
        create(&pool, alice, carol, "favourite", Some(other.id))
            .await
            .unwrap();
        create(&pool, alice, carol, "reblog", Some(post.id))
            .await
            .unwrap();
        create(&pool, alice, dave, "favourite", Some(post.id))
            .await
            .unwrap();

        clear_kind_for_status(&pool, alice, carol, "favourite", post.id)
            .await
            .unwrap();

        let remaining = list(
            &pool,
            alice,
            None,
            None,
            None,
            NotificationFilter::default(),
            10,
        )
        .await
        .unwrap();
        let mut kinds: Vec<(String, i64, Option<i64>)> = remaining
            .iter()
            .map(|n| (n.kind.clone(), n.from_account_id, n.status_id))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                ("favourite".to_owned(), carol, Some(other.id)),
                ("favourite".to_owned(), dave, Some(post.id)),
                ("reblog".to_owned(), carol, Some(post.id)),
            ]
        );
    }

    #[sqlx::test]
    async fn list_filters_by_kind_and_sender(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        create(&pool, alice, carol, "follow", None).await.unwrap();
        create(&pool, alice, carol, "favourite", None)
            .await
            .unwrap();
        create(&pool, alice, dave, "mention", None).await.unwrap();

        let kinds = vec!["favourite".to_owned(), "mention".to_owned()];
        let by_kind = NotificationFilter {
            kinds: Some(&kinds),
            from_account_id: None,
            ..Default::default()
        };
        let items = list(&pool, alice, None, None, None, by_kind, 10)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "mention");
        assert_eq!(items[1].kind, "favourite");
        assert_eq!(
            unread_count(&pool, alice, None, by_kind, 100)
                .await
                .unwrap(),
            2
        );

        let by_sender = NotificationFilter {
            kinds: None,
            from_account_id: Some(carol),
            ..Default::default()
        };
        let items = list(&pool, alice, None, None, None, by_sender, 10)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|n| n.from_account_id == carol));

        let none = NotificationFilter {
            kinds: Some(&[]),
            from_account_id: None,
            ..Default::default()
        };
        assert!(
            list(&pool, alice, None, None, None, none, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
