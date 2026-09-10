//! Notification filtering policy — Mastodon's `NotificationPolicy` plus the
//! `NotifyService` Drop/Filter decision, collapsed into one [`evaluate`] call.
//!
//! A policy assigns each of six sender categories a [`Disposition`]
//! (`accept`/`filter`/`drop`). When a notification's sender matches an
//! enabled category, the notification is either silently dropped (no row) or
//! stored with `filtered = TRUE` and rolled up into a
//! [`notification_request`](crate::notification_request). The non-policy drops
//! (self, blocks, mutes, muted conversations) live elsewhere — at insert in
//! [`notification::create`](crate::notification::create) and at listing via the
//! `sender_filtered` SQL function — so this module is purely the policy layer.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::DbError;

/// Notification kinds Mastodon marks `filterable: true` — only these are
/// subject to policy filtering; everything else always passes.
pub const FILTERABLE_KINDS: &[&str] = &[
    "mention",
    "reblog",
    "follow",
    "follow_request",
    "favourite",
    "quote",
    "added_to_collection",
];

/// An account younger than this is a "new account" for the policy.
const NEW_ACCOUNT_THRESHOLD_DAYS: i64 = 30;
/// A follow younger than this still counts the sender as "not a follower".
const NEW_FOLLOWER_THRESHOLD_DAYS: i64 = 3;

/// What to do with a notification whose sender matches a policy category.
/// Stored as Mastodon's integer enum (`0`/`1`/`2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Deliver normally.
    Accept,
    /// Store but hide (`filtered = TRUE`); rolled up into a request.
    Filter,
    /// Never store.
    Drop,
}

impl Disposition {
    #[must_use]
    fn from_i16(value: i16) -> Self {
        match value {
            1 => Self::Filter,
            2 => Self::Drop,
            _ => Self::Accept,
        }
    }

    #[must_use]
    pub fn as_i16(self) -> i16 {
        match self {
            Self::Accept => 0,
            Self::Filter => 1,
            Self::Drop => 2,
        }
    }

    /// The Mastodon REST string (`accept`/`filter`/`drop`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Filter => "filter",
            Self::Drop => "drop",
        }
    }

    /// Parses a REST string; unknown values fall back to `accept`.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "filter" => Self::Filter,
            "drop" => Self::Drop,
            _ => Self::Accept,
        }
    }
}

/// A recipient's six-category notification policy.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub for_not_following: Disposition,
    pub for_not_followers: Disposition,
    pub for_new_accounts: Disposition,
    pub for_private_mentions: Disposition,
    pub for_limited_accounts: Disposition,
    pub for_bots: Disposition,
}

impl Default for Policy {
    /// Plamenu's product default: unsolicited private mentions are accepted so
    /// third-party clients that do not support notification requests still show
    /// DMs. Limited accounts keep Mastodon's filtered default.
    fn default() -> Self {
        Self {
            for_not_following: Disposition::Accept,
            for_not_followers: Disposition::Accept,
            for_new_accounts: Disposition::Accept,
            for_private_mentions: Disposition::Accept,
            for_limited_accounts: Disposition::Filter,
            for_bots: Disposition::Accept,
        }
    }
}

/// The stored policy for `account_id`, or [`Policy::default`] when no row
/// exists yet (Mastodon's `find_or_initialize_by`).
pub async fn get_or_default(pool: &PgPool, account_id: i64) -> Result<Policy, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT for_not_following, for_not_followers, for_new_accounts,
               for_private_mentions, for_limited_accounts, for_bots
        FROM notification_policies
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map_or_else(Policy::default, |r| Policy {
        for_not_following: Disposition::from_i16(r.for_not_following),
        for_not_followers: Disposition::from_i16(r.for_not_followers),
        for_new_accounts: Disposition::from_i16(r.for_new_accounts),
        for_private_mentions: Disposition::from_i16(r.for_private_mentions),
        for_limited_accounts: Disposition::from_i16(r.for_limited_accounts),
        for_bots: Disposition::from_i16(r.for_bots),
    }))
}

/// Inserts or updates the policy row, returning the stored policy.
pub async fn upsert(pool: &PgPool, account_id: i64, policy: Policy) -> Result<Policy, DbError> {
    sqlx::query!(
        r#"
        INSERT INTO notification_policies
            (account_id, for_not_following, for_not_followers, for_new_accounts,
             for_private_mentions, for_limited_accounts, for_bots)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id) DO UPDATE SET
            for_not_following = EXCLUDED.for_not_following,
            for_not_followers = EXCLUDED.for_not_followers,
            for_new_accounts = EXCLUDED.for_new_accounts,
            for_private_mentions = EXCLUDED.for_private_mentions,
            for_limited_accounts = EXCLUDED.for_limited_accounts,
            for_bots = EXCLUDED.for_bots,
            updated_at = now()
        "#,
        account_id,
        policy.for_not_following.as_i16(),
        policy.for_not_followers.as_i16(),
        policy.for_new_accounts.as_i16(),
        policy.for_private_mentions.as_i16(),
        policy.for_limited_accounts.as_i16(),
        policy.for_bots.as_i16(),
    )
    .execute(pool)
    .await?;
    Ok(policy)
}

/// Mastodon's policy `summary`: how many pending requests, and the total
/// filtered notifications they roll up (both capped for display by callers).
pub async fn summary(pool: &PgPool, account_id: i64) -> Result<(i64, i64), DbError> {
    // Mastodon counts over at most `MAX_MEANINGFUL_COUNT` (100) requests.
    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "requests!",
               COALESCE(SUM(notifications_count), 0)::bigint AS "notifications!"
        FROM (
            SELECT notifications_count FROM notification_requests
            WHERE account_id = $1
            LIMIT 100
        ) capped
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((row.requests, row.notifications))
}

/// The policy [`Disposition`] for a would-be notification, i.e. the strongest
/// disposition across every matching sender category — Mastodon's
/// `DropCondition`/`FilterCondition` (`drop` beats `filter` beats `accept`).
///
/// `Accept` for non-filterable kinds and for senders the recipient has
/// already accepted (a [`notification_permission`](crate::notification_request)
/// override).
pub async fn evaluate(
    pool: &PgPool,
    recipient_id: i64,
    sender_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<Disposition, DbError> {
    let dispositions = evaluate_many(pool, &[recipient_id], sender_id, kind, status_id).await?;
    Ok(dispositions
        .get(&recipient_id)
        .copied()
        .unwrap_or(Disposition::Accept))
}

/// Batched [`evaluate`]: one policy decision per recipient for a single
/// would-be notification (one sender, one kind, one status). This is the
/// *only* implementation of the policy — [`evaluate`] is a one-element call
/// into it — so the two shapes cannot drift.
///
/// The per-sender facts are fetched once and every per-pair fact is one set
/// query across all recipients, so the statement count is O(1) in the
/// recipient count: permission overrides 1, policies 1, sender facts 1,
/// follow edges 1, plus one conditional query each for the not-followers and
/// unsolicited-DM categories over only the recipients whose policy enables
/// them (the same conditional shape the per-recipient path had).
pub async fn evaluate_many(
    pool: &PgPool,
    recipient_ids: &[i64],
    sender_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<HashMap<i64, Disposition>, DbError> {
    let mut result: HashMap<i64, Disposition> = recipient_ids
        .iter()
        .map(|&id| (id, Disposition::Accept))
        .collect();
    if recipient_ids.is_empty() || !FILTERABLE_KINDS.contains(&kind) {
        return Ok(result);
    }
    let mut targets: Vec<i64> = result.keys().copied().collect();
    targets.sort_unstable();

    // Explicitly accepted senders bypass all policy categories.
    let permitted: HashSet<i64> = sqlx::query_scalar!(
        r#"
        SELECT account_id FROM notification_permissions
        WHERE from_account_id = $1 AND account_id = ANY($2)
        "#,
        sender_id,
        &targets,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    targets.retain(|id| !permitted.contains(id));
    if targets.is_empty() {
        return Ok(result);
    }

    let policies = policies_of(pool, &targets).await?;
    let policy_of = |id: i64| policies.get(&id).copied().unwrap_or_default();

    let sender = sender_facts(pool, sender_id).await?;

    // Recipients who actively follow the sender (accepted, non-pending edge).
    let following: HashSet<i64> = sqlx::query_scalar!(
        r#"
        SELECT account_id FROM follows
        WHERE target_account_id = $1 AND NOT pending AND account_id = ANY($2)
        "#,
        sender_id,
        &targets,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    // The strongest disposition across every matching category wins —
    // Mastodon's `DropCondition`/`FilterCondition` (`drop` beats `filter`
    // beats `accept`).
    for &id in &targets {
        let policy = policy_of(id);
        let not_following = !following.contains(&id);
        let entry = result.get_mut(&id).expect("seeded above");
        let mut apply = |cond: bool, disposition: Disposition| {
            if cond && (disposition as u8) > (*entry as u8) {
                *entry = disposition;
            }
        };
        // `not_following` applies to every filterable kind.
        apply(not_following, policy.for_not_following);
        // New accounts and bots only count when the recipient does not follow them.
        apply(not_following && sender.is_new, policy.for_new_accounts);
        apply(not_following && sender.is_bot, policy.for_bots);
        // `for_limited_accounts` keys off the sender being silenced by a
        // moderator; Plamenu has no moderation surface, so it never fires
        // (kept for API compat).
    }

    // Mastodon's `not_follower?`: the sender does not follow the recipient, or
    // only started following within the new-follower window. Checked only for
    // recipients whose policy enables the category.
    let follower_subset: Vec<i64> = targets
        .iter()
        .copied()
        .filter(|&id| policy_of(id).for_not_followers != Disposition::Accept)
        .collect();
    for id in recent_or_absent_follower_of(pool, sender_id, &follower_subset).await? {
        let disposition = policy_of(id).for_not_followers;
        let entry = result.get_mut(&id).expect("seeded above");
        if (disposition as u8) > (*entry as u8) {
            *entry = disposition;
        }
    }

    // Private mentions: a direct mention from a non-followed sender that is not
    // a reply within a thread the recipient already DM'd the sender in.
    if kind == "mention"
        && let Some(status_id) = status_id
    {
        let dm_subset: Vec<i64> = targets
            .iter()
            .copied()
            .filter(|&id| {
                let disposition = policy_of(id).for_private_mentions;
                !following.contains(&id)
                    && disposition != Disposition::Accept
                    && (disposition as u8) > (result[&id] as u8)
            })
            .collect();
        if !dm_subset.is_empty() {
            for id in unsolicited_private_mention_of(pool, &dm_subset, sender_id, status_id).await?
            {
                let entry = result.get_mut(&id).expect("seeded above");
                *entry = policy_of(id).for_private_mentions;
            }
        }
    }

    Ok(result)
}

/// The stored policies for `account_ids` in one read; accounts without a row
/// are absent (callers fall back to [`Policy::default`], matching
/// [`get_or_default`]).
async fn policies_of(pool: &PgPool, account_ids: &[i64]) -> Result<HashMap<i64, Policy>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT account_id, for_not_following, for_not_followers, for_new_accounts,
               for_private_mentions, for_limited_accounts, for_bots
        FROM notification_policies
        WHERE account_id = ANY($1)
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.account_id,
                Policy {
                    for_not_following: Disposition::from_i16(r.for_not_following),
                    for_not_followers: Disposition::from_i16(r.for_not_followers),
                    for_new_accounts: Disposition::from_i16(r.for_new_accounts),
                    for_private_mentions: Disposition::from_i16(r.for_private_mentions),
                    for_limited_accounts: Disposition::from_i16(r.for_limited_accounts),
                    for_bots: Disposition::from_i16(r.for_bots),
                },
            )
        })
        .collect())
}

/// Whether `recipient` has accepted `sender` (override row present).
pub async fn permission_exists(
    pool: &PgPool,
    recipient_id: i64,
    sender_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM notification_permissions
            WHERE account_id = $1 AND from_account_id = $2
        ) AS "found!"
        "#,
        recipient_id,
        sender_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(found)
}

struct SenderFacts {
    is_new: bool,
    is_bot: bool,
}

async fn sender_facts(pool: &PgPool, sender_id: i64) -> Result<SenderFacts, DbError> {
    let threshold = OffsetDateTime::now_utc() - Duration::days(NEW_ACCOUNT_THRESHOLD_DAYS);
    let row = sqlx::query!(
        r#"SELECT created_at, is_bot FROM accounts WHERE id = $1"#,
        sender_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(SenderFacts {
        is_new: row.created_at > threshold,
        is_bot: row.is_bot,
    })
}

/// Mastodon's `not_follower?`, batched: of `recipient_ids`, the ones the
/// sender does not follow — or only started following within the new-follower
/// window.
async fn recent_or_absent_follower_of(
    pool: &PgPool,
    sender_id: i64,
    recipient_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    if recipient_ids.is_empty() {
        return Ok(Vec::new());
    }
    let follow_created: HashMap<i64, OffsetDateTime> = sqlx::query!(
        r#"
        SELECT target_account_id, created_at FROM follows
        WHERE account_id = $1 AND target_account_id = ANY($2)
        "#,
        sender_id,
        recipient_ids,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.target_account_id, r.created_at))
    .collect();
    let threshold = OffsetDateTime::now_utc() - Duration::days(NEW_FOLLOWER_THRESHOLD_DAYS);
    Ok(recipient_ids
        .iter()
        .copied()
        .filter(|id| follow_created.get(id).is_none_or(|&at| at > threshold))
        .collect())
}

/// Mastodon's `private_mention_not_in_response?`, batched: of `recipient_ids`,
/// the ones for whom the mentioning status is an *unsolicited* private mention
/// — it is `direct` and the recipient has not, somewhere up the reply thread,
/// sent the sender a `direct` status mentioning them (a conversation they
/// opted into). The ancestor walk is shared by every recipient, so it runs
/// once with each recipient's opt-in checked against it.
async fn unsolicited_private_mention_of(
    pool: &PgPool,
    recipient_ids: &[i64],
    sender_id: i64,
    status_id: i64,
) -> Result<Vec<i64>, DbError> {
    let row = sqlx::query!(
        r#"SELECT visibility, in_reply_to_id FROM statuses WHERE id = $1 -- STUBKEEP: reads the just-arrived status, never a stub"#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Vec::new());
    };
    if row.visibility != "direct" {
        return Ok(Vec::new());
    }
    let Some(parent_id) = row.in_reply_to_id else {
        // Not a reply at all — an unsolicited DM for everyone asked about.
        return Ok(recipient_ids.to_vec());
    };
    // Walk ancestors once, looking per recipient for a direct status by that
    // recipient which mentions the sender (the recipient previously DM'd them
    // in this thread). Bounded to 100 hops like Mastodon's
    // `statuses_that_mention_sender`.
    let responded: HashSet<i64> = sqlx::query_scalar!(
        r#"
        WITH RECURSIVE ancestors(id, in_reply_to_id, depth) AS (
            SELECT s.id, s.in_reply_to_id, 0
            FROM statuses s WHERE s.id = $1
          UNION ALL
            SELECT s.id, s.in_reply_to_id, a.depth + 1
            FROM ancestors a
            JOIN statuses s ON s.id = a.in_reply_to_id
            WHERE a.depth < 100
        )
        SELECT r.recipient AS "recipient!"
        FROM unnest($2::bigint[]) AS r(recipient)
        WHERE EXISTS (
            SELECT 1
            FROM ancestors a
            JOIN statuses s ON s.id = a.id -- STUBKEEP: the walk crosses stubs; their kept mention rows still witness the opt-in
            JOIN status_mentions m ON m.status_id = s.id AND m.account_id = $3
            WHERE s.account_id = r.recipient AND s.visibility = 'direct'
        )
        "#,
        parent_id,
        recipient_ids,
        sender_id,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(recipient_ids
        .iter()
        .copied()
        .filter(|id| !responded.contains(id))
        .collect())
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

    async fn set_not_following(pool: &PgPool, account_id: i64, disposition: Disposition) {
        let policy = Policy {
            for_not_following: disposition,
            ..Default::default()
        };
        upsert(pool, account_id, policy).await.unwrap();
    }

    #[sqlx::test]
    async fn default_policy_accepts_public_kinds_from_strangers(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        // bob is brand-new and not followed, but the default `for_new_accounts`
        // / `for_not_following` are `accept`, so a favourite passes.
        let disposition = evaluate(&pool, alice, bob, "favourite", None)
            .await
            .unwrap();
        assert_eq!(disposition, Disposition::Accept);
    }

    #[sqlx::test]
    async fn not_following_policy_filters_and_drops(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        set_not_following(&pool, alice, Disposition::Filter).await;
        assert_eq!(
            evaluate(&pool, alice, bob, "favourite", None)
                .await
                .unwrap(),
            Disposition::Filter
        );

        set_not_following(&pool, alice, Disposition::Drop).await;
        assert_eq!(
            evaluate(&pool, alice, bob, "favourite", None)
                .await
                .unwrap(),
            Disposition::Drop
        );
    }

    #[sqlx::test]
    async fn following_sender_is_never_filtered(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        set_not_following(&pool, alice, Disposition::Drop).await;
        // alice follows bob, so the `not_following` category no longer matches.
        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();
        assert_eq!(
            evaluate(&pool, alice, bob, "favourite", None)
                .await
                .unwrap(),
            Disposition::Accept
        );
    }

    #[sqlx::test]
    async fn accepted_sender_overrides_policy(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        set_not_following(&pool, alice, Disposition::Drop).await;
        // A stored permission (Mastodon's NotificationPermission) bypasses all
        // categories.
        crate::notification_request::accept(&pool, alice, bob)
            .await
            .unwrap();
        assert_eq!(
            evaluate(&pool, alice, bob, "favourite", None)
                .await
                .unwrap(),
            Disposition::Accept
        );
    }

    #[sqlx::test]
    async fn non_filterable_kinds_always_accept(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        set_not_following(&pool, alice, Disposition::Drop).await;
        // `poll`/`update` are not filterable, so policy never applies.
        assert_eq!(
            evaluate(&pool, alice, bob, "update", None).await.unwrap(),
            Disposition::Accept
        );
    }
}
