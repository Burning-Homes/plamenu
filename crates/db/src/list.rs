//! User lists (Mastodon's model): private groupings of followed accounts,
//! each with its own timeline. Memberships ride on the follow edge
//! (`list_accounts.follow_id` cascades away with the follow); a pending
//! edge — a follow request toward a locked account — may be listed but
//! stays inactive until accepted.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::status::Status;
use crate::user::TimelineOrder;
use crate::{DbError, id};

/// Mastodon's `List::PER_ACCOUNT_LIMIT`.
pub const PER_ACCOUNT_LIMIT: i64 = 50;
/// Mastodon's `List::TITLE_LENGTH_LIMIT` (characters).
pub const TITLE_LENGTH_LIMIT: usize = 256;

/// The `replies_policy` values Mastodon's enum admits.
pub const REPLIES_POLICIES: [&str; 3] = ["list", "followed", "none"];

#[derive(Debug, Clone)]
pub struct List {
    pub id: i64,
    pub account_id: i64,
    pub title: String,
    pub replies_policy: String,
    pub exclusive: bool,
}

/// Creates a list. Caller validates title/policy and the per-account limit.
pub async fn create(
    pool: &PgPool,
    account_id: i64,
    title: &str,
    replies_policy: &str,
    exclusive: bool,
) -> Result<List, DbError> {
    let list = sqlx::query_as!(
        List,
        r#"
        INSERT INTO lists (id, account_id, title, replies_policy, exclusive)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, account_id, title, replies_policy, exclusive
        "#,
        id::next(),
        account_id,
        title,
        replies_policy,
        exclusive,
    )
    .fetch_one(pool)
    .await?;
    Ok(list)
}

/// How many lists `account_id` owns (for the 50-list limit).
pub async fn count_owned(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM lists WHERE account_id = $1"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Every list `account_id` owns, oldest first.
pub async fn owned_by(pool: &PgPool, account_id: i64) -> Result<Vec<List>, DbError> {
    let lists = sqlx::query_as!(
        List,
        r#"SELECT id, account_id, title, replies_policy, exclusive
           FROM lists WHERE account_id = $1 ORDER BY id"#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(lists)
}

/// A list by id, only if `account_id` owns it — every list endpoint scopes
/// like this (Mastodon's `List.where(account: current_account).find`).
pub async fn find_owned(
    pool: &PgPool,
    account_id: i64,
    list_id: i64,
) -> Result<Option<List>, DbError> {
    let list = sqlx::query_as!(
        List,
        r#"SELECT id, account_id, title, replies_policy, exclusive
           FROM lists WHERE id = $1 AND account_id = $2"#,
        list_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(list)
}

/// The first list `account_id` owns with the given title, if any — the lookup
/// half of CSV import's find-or-create (Mastodon's
/// `owned_lists.find_or_create_by!(title:)`).
pub async fn find_owned_by_title(
    pool: &PgPool,
    account_id: i64,
    title: &str,
) -> Result<Option<List>, DbError> {
    let list = sqlx::query_as!(
        List,
        r#"SELECT id, account_id, title, replies_policy, exclusive
           FROM lists WHERE account_id = $1 AND title = $2 ORDER BY id LIMIT 1"#,
        account_id,
        title,
    )
    .fetch_optional(pool)
    .await?;
    Ok(list)
}

/// Deletes every list `account_id` owns whose title is not in `titles` — the
/// overwrite pre-step of CSV list import (Mastodon's
/// `owned_lists.where.not(title: included).destroy_all`).
pub async fn delete_owned_not_in_titles(
    pool: &PgPool,
    account_id: i64,
    titles: &[String],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM lists WHERE account_id = $1 AND title <> ALL($2)",
        account_id,
        titles,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Clears every membership from all lists `account_id` owns — the other half
/// of overwrite list import (Mastodon clears memberships because list changes
/// do not retroactively rewrite timelines).
pub async fn clear_owned_memberships(pool: &PgPool, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM list_accounts
         WHERE list_id IN (SELECT id FROM lists WHERE account_id = $1)",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces a list's attributes (the handler merges absent params first).
pub async fn update(
    pool: &PgPool,
    list_id: i64,
    title: &str,
    replies_policy: &str,
    exclusive: bool,
) -> Result<List, DbError> {
    let list = sqlx::query_as!(
        List,
        r#"
        UPDATE lists
        SET title = $2, replies_policy = $3, exclusive = $4, updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, title, replies_policy, exclusive
        "#,
        list_id,
        title,
        replies_policy,
        exclusive,
    )
    .fetch_one(pool)
    .await?;
    Ok(list)
}

/// Deletes an owned list; returns whether a row matched.
pub async fn delete(pool: &PgPool, account_id: i64, list_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM lists WHERE id = $1 AND account_id = $2",
        list_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Why adding a member failed — Mastodon's `ListAccount` validations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMemberError {
    /// The owner neither follows the account nor has a pending request
    /// toward it (and it isn't the owner themself).
    NotFollowed,
    /// The account is already on the list.
    AlreadyMember,
}

/// Adds `account_ids` to a list atomically: any failure rejects the whole
/// batch with nothing inserted, like Mastodon's transactional
/// `AddAccountsToListService`. Callers pass a deduplicated, capped set
/// (`bounded_unique_ids`); resolution is set-based — one follow-edge lookup,
/// one existing-membership lookup, and one insert — rather than a query per id.
pub async fn add_members(
    pool: &PgPool,
    list_id: i64,
    owner_id: i64,
    account_ids: &[i64],
) -> Result<Result<(), AddMemberError>, DbError> {
    // The owner's follow-edge id for every non-self target, in one query — a
    // member must be followed (accepted or pending) or be the owner themself.
    let non_self: Vec<i64> = account_ids
        .iter()
        .copied()
        .filter(|&id| id != owner_id)
        .collect();
    let follow_ids = crate::follow::find_out_ids_batch(pool, owner_id, &non_self).await?;
    // Which requested ids are already members, in one query.
    let existing: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM list_accounts WHERE list_id = $1 AND account_id = ANY($2)",
        list_id,
        account_ids,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    // Validate in input order — the first offending id decides the error, and
    // follow-membership is checked before existing-membership, exactly as the
    // former per-row loop did. Build the insert columns as we go.
    let mut row_ids = Vec::with_capacity(account_ids.len());
    let mut row_accounts = Vec::with_capacity(account_ids.len());
    let mut row_follow_ids: Vec<Option<i64>> = Vec::with_capacity(account_ids.len());
    for &account_id in account_ids {
        let follow_id = if account_id == owner_id {
            None
        } else {
            match follow_ids.get(&account_id) {
                Some(&follow_id) => Some(follow_id),
                None => return Ok(Err(AddMemberError::NotFollowed)),
            }
        };
        if existing.contains(&account_id) {
            return Ok(Err(AddMemberError::AlreadyMember));
        }
        row_ids.push(id::next());
        row_accounts.push(account_id);
        row_follow_ids.push(follow_id);
    }

    // One set-based insert. `ON CONFLICT DO NOTHING` guards a concurrent
    // duplicate; a within-batch duplicate cannot occur since callers dedup.
    sqlx::query!(
        r#"
        INSERT INTO list_accounts (id, list_id, account_id, follow_id)
        SELECT ins.id, $2, ins.account_id, ins.follow_id
        FROM unnest($1::bigint[], $3::bigint[], $4::bigint[])
             AS ins(id, account_id, follow_id)
        ON CONFLICT (list_id, account_id) DO NOTHING
        "#,
        &row_ids,
        list_id,
        &row_accounts,
        &row_follow_ids as &[Option<i64>],
    )
    .execute(pool)
    .await?;
    Ok(Ok(()))
}

/// Removes `account_ids` from a list; absent memberships are ignored.
pub async fn remove_members(
    pool: &PgPool,
    list_id: i64,
    account_ids: &[i64],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM list_accounts WHERE list_id = $1 AND account_id = ANY($2)",
        list_id,
        account_ids,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A page of member account ids, newest account first (Mastodon paginates
/// the accounts relation by account id). `limit` of `None` lists everyone —
/// Mastodon's `limit=0`.
pub async fn members_page(
    pool: &PgPool,
    list_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT account_id AS "account_id!"
        FROM list_accounts
        WHERE list_id = $1
          AND ($2::bigint IS NULL OR account_id < $2)
          AND ($3::bigint IS NULL OR account_id > $3)
        ORDER BY account_id DESC
        LIMIT $4
        "#,
        list_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// The lists `owner_id` owns that contain `member_id` — the
/// `GET /api/v1/accounts/{id}/lists` listing.
pub async fn containing(
    pool: &PgPool,
    owner_id: i64,
    member_id: i64,
) -> Result<Vec<List>, DbError> {
    let lists = sqlx::query_as!(
        List,
        r#"
        SELECT l.id, l.account_id, l.title, l.replies_policy, l.exclusive
        FROM lists l
        JOIN list_accounts la ON la.list_id = l.id
        WHERE l.account_id = $1 AND la.account_id = $2
        ORDER BY l.id
        "#,
        owner_id,
        member_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(lists)
}

/// The list timeline: statuses (and boosts) by *active* members — the
/// owner's accepted follows, or the owner themself — visibility- and
/// block/mute-gated like the home timeline, with the list's
/// `replies_policy` applied: self-replies and replies to the owner always
/// show; otherwise `followed` admits replies to accounts the owner follows,
/// `list` replies to fellow list members, `none` no other replies
/// (Mastodon's `filter_from_list?` + the home reply rule). Keyset-paginated
/// (the cursor is a status id under either ordering).
pub async fn timeline(
    pool: &PgPool,
    list: &List,
    order: TimelineOrder,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    match order {
        TimelineOrder::Published => {
            let anchor = crate::status::sort_anchor(pool, max_id).await?;
            timeline_published(pool, list, anchor, limit).await
        }
        TimelineOrder::Received => timeline_received(pool, list, max_id, limit).await,
    }
}

// The two orderings are separate compile-checked queries whose WHERE bodies
// must stay in sync; only the keyset predicate and ORDER BY differ (the
// `published` variant merges each member's `idx_statuses_account_sort_at`
// slice, the `received` one their `idx_statuses_account` slice).
//
// Both are driven per member, like the home timeline's arm A: nested-loop the
// member set into each member's per-account index slice capped at `limit`, so
// the candidate pool is bounded by #members x limit instead of walking the
// global feed order until enough list rows surface (at 400 members x deep
// histories the old shape ran account_hidden() and the reply/M32 subplans on
// ~15k rows per page — the 1.1s list-timeline pathology).
//
// Predicates split by altitude, same discipline as home:
//
//   * member-level — `NOT account_hidden(owner, member)` inlined onto the driving member scan
//     (blocks both ways, mutes, both domain-block directions), one hashed subplan each built once
//     per query plus a per-member domain probe; account_hidden() stays the source of truth for
//     those semantics (`streams_receiving` below keeps the callable copy). The member's follow row
//     (pending gate, M32 show_reblogs/languages) is the driving LEFT JOIN, carried into the
//     lateral.
//   * row-level — visibility, the boosted author's account_hidden(), the replies_policy parent gate
//     and the keyset bound stay inside the per-member CROSS JOIN LATERAL.
//
// Taking the top `limit` from the per-member slices is exact: a row in the
// combined top `limit` has fewer than `limit` rows above it in its own
// member's slice, so it survives that slice's cap. Members are unique per
// list, so no cross-slice dedup is needed.

#[allow(
    clippy::too_many_lines,
    reason = "one keyset-bounded driven query; the length is the SQL literal, which can't be split"
)]
async fn timeline_received(
    pool: &PgPool,
    list: &List,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!", application_id,
               title, object_type, external_url
        FROM (
            SELECT sl.id, sl.uri, sl.account_id, sl.content, sl.created_at, sl.updated_at,
                   sl.visibility, sl.in_reply_to_id, sl.reblog_of_id, sl.edited_at,
                   sl.spoiler_text, sl.sensitive, sl.language, sl.url, sl.quote_approval_policy,
                   sl.application_id, sl.title, sl.object_type, sl.external_url
            FROM (
                SELECT la.account_id AS aid, mf.id AS follow_id, mf.show_reblogs,
                       mf.with_replies, mf.languages
                FROM list_accounts la
                LEFT JOIN follows mf ON mf.id = la.follow_id
                WHERE la.list_id = $1
                  AND (la.follow_id IS NULL OR NOT mf.pending)
                  AND EXISTS (SELECT 1 FROM accounts member
                              WHERE member.id = la.account_id
                                AND member.suspended_at IS NULL)
                  -- Member-level: inlined NOT account_hidden($2, member).
                  -- MUST stay in sync with the other ordering's copy and,
                  -- semantically, with account_hidden().
                  AND (la.account_id = $2 OR (
                       la.account_id NOT IN (SELECT b.target_account_id FROM blocks b
                                             WHERE b.account_id = $2)
                   AND la.account_id NOT IN (SELECT b.account_id FROM blocks b
                                             WHERE b.target_account_id = $2)
                   AND la.account_id NOT IN (SELECT m.target_account_id FROM mutes m
                                             WHERE m.account_id = $2
                                               AND (m.expires_at IS NULL OR m.expires_at > now()))
                   AND la.account_id NOT IN (SELECT adb.account_id
                                             FROM account_domain_blocks adb
                                             JOIN accounts v ON v.id = $2 AND adb.domain = v.domain)
                   AND NOT EXISTS (SELECT 1 FROM accounts a
                                   WHERE a.id = la.account_id AND a.domain IS NOT NULL
                                     AND NOT a.portable
                                     AND a.domain IN (SELECT adb.domain
                                                      FROM account_domain_blocks adb
                                                      WHERE adb.account_id = $2))))
            ) members
            CROSS JOIN LATERAL (
                SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                       s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                       s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy,
                       s.application_id, s.title, s.object_type, s.external_url
                FROM statuses s
                WHERE s.account_id = members.aid
                  AND s.deleted_at IS NULL -- STUBFILTER (reply-parent probe below keeps stubs)
                  AND s.ingest_provenance = 'delivery'
                  -- Row-level: MUST stay in sync with the other ordering's
                  -- copy and with `streams_receiving` below.
                  AND s.visibility <> 'direct'
                  AND (s.reblog_of_id IS NULL OR NOT account_hidden(
                         $2, (SELECT t.account_id FROM statuses t WHERE t.id = s.reblog_of_id)))
                  AND (s.in_reply_to_id IS NULL OR EXISTS (
                         SELECT 1 FROM statuses p
                         WHERE p.id = s.in_reply_to_id
                           AND (p.account_id = s.account_id
                                OR p.account_id = $2
                                OR ($3 = 'followed' AND EXISTS (
                                      SELECT 1 FROM follows rf
                                      WHERE rf.account_id = $2
                                        AND rf.target_account_id = p.account_id
                                        AND NOT rf.pending))
                                OR ($3 = 'list' AND EXISTS (
                                      SELECT 1 FROM list_accounts ra
                                      WHERE ra.list_id = $1
                                        AND ra.account_id = p.account_id)))))
                  -- Per-follow settings (M32), same as the home timeline: the
                  -- owner's follow of the member gates boosts and languages.
                  AND (members.follow_id IS NULL OR ((s.reblog_of_id IS NULL OR members.show_reblogs)
                       AND (s.language IS NULL OR members.languages IS NULL
                            OR s.language = ANY(members.languages))))
                  -- Per-follow `with_replies`, ANDed with the list's own
                  -- replies_policy gate above: Mastodon runs
                  -- `filter_from_list?` and then `filter_from_home`, so a list
                  -- is never less filtered than home. Same three exemptions as
                  -- home, and the judge-a-boost-by-its-target rule rides along for
                  -- the same reason it does there (a community authors nothing;
                  -- everything it contributes is a wrapper whose own
                  -- in_reply_to_id is NULL). A member with no follow row has no
                  -- flag to read and keeps every reply. MUST stay in sync with
                  -- the other ordering's copy, with `streams_receiving` below,
                  -- and semantically with home arm A.
                  AND (members.follow_id IS NULL OR members.with_replies
                       OR ((s.in_reply_to_id IS NULL
                            OR s.in_reply_to_account_id = s.account_id
                            OR s.in_reply_to_account_id = $2
                            OR EXISTS (SELECT 1 FROM follows rf
                                       WHERE rf.account_id = $2
                                         AND rf.target_account_id = s.in_reply_to_account_id
                                         AND NOT rf.pending))
                           AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                                  SELECT 1 FROM statuses bt
                                  WHERE bt.id = s.reblog_of_id
                                    AND bt.in_reply_to_id IS NOT NULL))))
                  AND ($4::bigint IS NULL OR s.id < $4)
                ORDER BY s.id DESC
                LIMIT $5
            ) sl
        ) merged
        ORDER BY id DESC
        LIMIT $5
        "#,
        list.id,
        list.account_id,
        list.replies_policy,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

#[allow(
    clippy::too_many_lines,
    reason = "one keyset-bounded driven query; the length is the SQL literal, which can't be split"
)]
async fn timeline_published(
    pool: &PgPool,
    list: &List,
    anchor: Option<(OffsetDateTime, i64)>,
    limit: i64,
) -> Result<Vec<Status>, DbError> {
    let (anchor_sort_at, anchor_id) = anchor.unzip();
    let statuses = sqlx::query_as!(
        Status,
        r#"
        SELECT id AS "id!", uri, account_id AS "account_id!", content AS "content!",
               created_at AS "created_at!", updated_at AS "updated_at!",
               visibility AS "visibility!", in_reply_to_id, reblog_of_id, edited_at,
               spoiler_text AS "spoiler_text!", sensitive AS "sensitive!", language, url,
               quote_approval_policy AS "quote_approval_policy!", application_id,
               title, object_type, external_url
        FROM (
            SELECT sl.id, sl.uri, sl.account_id, sl.content, sl.created_at, sl.updated_at,
                   sl.visibility, sl.in_reply_to_id, sl.reblog_of_id, sl.edited_at,
                   sl.spoiler_text, sl.sensitive, sl.language, sl.url, sl.quote_approval_policy,
                   sl.application_id, sl.title, sl.object_type, sl.external_url, sl.sort_at
            FROM (
                SELECT la.account_id AS aid, mf.id AS follow_id, mf.show_reblogs,
                       mf.with_replies, mf.languages
                FROM list_accounts la
                LEFT JOIN follows mf ON mf.id = la.follow_id
                WHERE la.list_id = $1
                  AND (la.follow_id IS NULL OR NOT mf.pending)
                  AND EXISTS (SELECT 1 FROM accounts member
                              WHERE member.id = la.account_id
                                AND member.suspended_at IS NULL)
                  -- Member-level: inlined NOT account_hidden($2, member).
                  -- MUST stay in sync with the other ordering's copy and,
                  -- semantically, with account_hidden().
                  AND (la.account_id = $2 OR (
                       la.account_id NOT IN (SELECT b.target_account_id FROM blocks b
                                             WHERE b.account_id = $2)
                   AND la.account_id NOT IN (SELECT b.account_id FROM blocks b
                                             WHERE b.target_account_id = $2)
                   AND la.account_id NOT IN (SELECT m.target_account_id FROM mutes m
                                             WHERE m.account_id = $2
                                               AND (m.expires_at IS NULL OR m.expires_at > now()))
                   AND la.account_id NOT IN (SELECT adb.account_id
                                             FROM account_domain_blocks adb
                                             JOIN accounts v ON v.id = $2 AND adb.domain = v.domain)
                   AND NOT EXISTS (SELECT 1 FROM accounts a
                                   WHERE a.id = la.account_id AND a.domain IS NOT NULL
                                     AND NOT a.portable
                                     AND a.domain IN (SELECT adb.domain
                                                      FROM account_domain_blocks adb
                                                      WHERE adb.account_id = $2))))
            ) members
            CROSS JOIN LATERAL (
                SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
                       s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
                       s.spoiler_text, s.sensitive, s.language, s.url, s.quote_approval_policy,
                       s.application_id, s.title, s.object_type, s.external_url, s.sort_at
                FROM statuses s
                WHERE s.account_id = members.aid
                  AND s.deleted_at IS NULL -- STUBFILTER (reply-parent probe below keeps stubs)
                  AND s.ingest_provenance = 'delivery'
                  -- Row-level: MUST stay in sync with the other ordering's
                  -- copy and with `streams_receiving` below.
                  AND s.visibility <> 'direct'
                  AND (s.reblog_of_id IS NULL OR NOT account_hidden(
                         $2, (SELECT t.account_id FROM statuses t WHERE t.id = s.reblog_of_id)))
                  AND (s.in_reply_to_id IS NULL OR EXISTS (
                         SELECT 1 FROM statuses p
                         WHERE p.id = s.in_reply_to_id
                           AND (p.account_id = s.account_id
                                OR p.account_id = $2
                                OR ($3 = 'followed' AND EXISTS (
                                      SELECT 1 FROM follows rf
                                      WHERE rf.account_id = $2
                                        AND rf.target_account_id = p.account_id
                                        AND NOT rf.pending))
                                OR ($3 = 'list' AND EXISTS (
                                      SELECT 1 FROM list_accounts ra
                                      WHERE ra.list_id = $1
                                        AND ra.account_id = p.account_id)))))
                  -- Per-follow settings (M32), same as the home timeline: the
                  -- owner's follow of the member gates boosts and languages.
                  AND (members.follow_id IS NULL OR ((s.reblog_of_id IS NULL OR members.show_reblogs)
                       AND (s.language IS NULL OR members.languages IS NULL
                            OR s.language = ANY(members.languages))))
                  -- Per-follow `with_replies`, ANDed with the list's own
                  -- replies_policy gate above: Mastodon runs
                  -- `filter_from_list?` and then `filter_from_home`, so a list
                  -- is never less filtered than home. Same three exemptions as
                  -- home, and the judge-a-boost-by-its-target rule rides along for
                  -- the same reason it does there (a community authors nothing;
                  -- everything it contributes is a wrapper whose own
                  -- in_reply_to_id is NULL). A member with no follow row has no
                  -- flag to read and keeps every reply. MUST stay in sync with
                  -- the other ordering's copy, with `streams_receiving` below,
                  -- and semantically with home arm A.
                  AND (members.follow_id IS NULL OR members.with_replies
                       OR ((s.in_reply_to_id IS NULL
                            OR s.in_reply_to_account_id = s.account_id
                            OR s.in_reply_to_account_id = $2
                            OR EXISTS (SELECT 1 FROM follows rf
                                       WHERE rf.account_id = $2
                                         AND rf.target_account_id = s.in_reply_to_account_id
                                         AND NOT rf.pending))
                           AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                                  SELECT 1 FROM statuses bt
                                  WHERE bt.id = s.reblog_of_id
                                    AND bt.in_reply_to_id IS NOT NULL))))
                  AND ($4::timestamptz IS NULL OR (s.sort_at, s.id) < ($4, $5::bigint))
                ORDER BY s.sort_at DESC, s.id DESC
                LIMIT $6
            ) sl
        ) merged
        ORDER BY sort_at DESC, id DESC
        LIMIT $6
        "#,
        list.id,
        list.account_id,
        list.replies_policy,
        anchor_sort_at,
        anchor_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// A list that should receive a status on its stream, with the owner the
/// payload is rendered for.
#[derive(Debug, Clone, Copy)]
pub struct ReceivingList {
    pub list_id: i64,
    pub owner_id: i64,
}

/// Which of `candidates` (lists with live streams) carry `status_id` —
/// the [`timeline`] conditions applied to one status.
pub async fn streams_receiving(
    pool: &PgPool,
    candidates: &[i64],
    status_id: i64,
) -> Result<Vec<ReceivingList>, DbError> {
    let lists = sqlx::query_as!(
        ReceivingList,
        r#"
        SELECT l.id AS "list_id!", l.account_id AS "owner_id!"
        FROM lists l
        JOIN statuses s ON s.id = $2
        JOIN list_accounts la ON la.list_id = l.id AND la.account_id = s.account_id
        LEFT JOIN follows mf ON mf.id = la.follow_id
        WHERE l.id = ANY($1)
          AND (la.follow_id IS NULL OR NOT mf.pending)
          AND s.deleted_at IS NULL -- STUBFILTER (a stub is never a newly-delivered status; kept for the marker census)
          AND s.ingest_provenance = 'delivery'
          AND s.visibility <> 'direct'
          AND NOT account_hidden(l.account_id, s.account_id)
          AND (s.reblog_of_id IS NULL OR NOT account_hidden(
                 l.account_id,
                 (SELECT t.account_id FROM statuses t WHERE t.id = s.reblog_of_id)))
          AND (s.in_reply_to_id IS NULL OR EXISTS (
                 SELECT 1 FROM statuses p
                 WHERE p.id = s.in_reply_to_id
                   AND (p.account_id = s.account_id
                        OR p.account_id = l.account_id
                        OR (l.replies_policy = 'followed' AND EXISTS (
                              SELECT 1 FROM follows rf
                              WHERE rf.account_id = l.account_id
                                AND rf.target_account_id = p.account_id
                                AND NOT rf.pending))
                        OR (l.replies_policy = 'list' AND EXISTS (
                              SELECT 1 FROM list_accounts ra
                              WHERE ra.list_id = l.id
                                AND ra.account_id = p.account_id)))))
          AND (mf.id IS NULL OR ((s.reblog_of_id IS NULL OR mf.show_reblogs)
               AND (s.language IS NULL OR mf.languages IS NULL
                    OR s.language = ANY(mf.languages))))
          -- Per-follow `with_replies`; the live half of the
          -- timeline queries' copy above.
          AND (mf.id IS NULL OR mf.with_replies
               OR ((s.in_reply_to_id IS NULL
                    OR s.in_reply_to_account_id = s.account_id
                    OR s.in_reply_to_account_id = l.account_id
                    OR EXISTS (SELECT 1 FROM follows rf
                               WHERE rf.account_id = l.account_id
                                 AND rf.target_account_id = s.in_reply_to_account_id
                                 AND NOT rf.pending))
                   AND (s.reblog_of_id IS NULL OR NOT EXISTS (
                          SELECT 1 FROM statuses bt
                          WHERE bt.id = s.reblog_of_id
                            AND bt.in_reply_to_id IS NOT NULL))))
        "#,
        candidates,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(lists)
}

/// Which of `candidates` have `author_id` as an active member — where a
/// deleted status of theirs may have appeared.
pub async fn streams_with_member(
    pool: &PgPool,
    candidates: &[i64],
    author_id: i64,
) -> Result<Vec<i64>, DbError> {
    let lists = sqlx::query_scalar!(
        r#"
        SELECT l.id AS "id!"
        FROM lists l
        JOIN list_accounts la ON la.list_id = l.id AND la.account_id = $2
        LEFT JOIN follows mf ON mf.id = la.follow_id
        WHERE l.id = ANY($1) AND (la.follow_id IS NULL OR NOT mf.pending)
        "#,
        candidates,
        author_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(lists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};
    use crate::{follow, mute};

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

    /// Runs the list timeline under both orderings and asserts they select
    /// the same rows — the WHERE bodies are duplicated per ordering and must
    /// not drift. Returns the `published` (default) result.
    async fn timeline_both(
        pool: &PgPool,
        list: &List,
        max_id: Option<i64>,
        limit: i64,
    ) -> Vec<Status> {
        let published = timeline(pool, list, TimelineOrder::Published, max_id, limit)
            .await
            .unwrap();
        let received = timeline(pool, list, TimelineOrder::Received, max_id, limit)
            .await
            .unwrap();
        let mut a: Vec<i64> = published.iter().map(|s| s.id).collect();
        let mut b: Vec<i64> = received.iter().map(|s| s.id).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "both orderings must select the same rows");
        published
    }

    async fn post(pool: &PgPool, author: i64, text: &str, reply_to: Option<i64>) -> i64 {
        status::create_local(pool, NewLocalStatus::new(author, text, "public", reply_to))
            .await
            .unwrap()
            .id
    }

    /// A late-arriving old post sinks to its publish date under the default
    /// ordering but leads under ingest order.
    #[sqlx::test]
    async fn timeline_orderings_disagree_on_backfilled_posts(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        follow::create(&pool, alice, bob, None).await.unwrap();
        let list = create(&pool, alice, "Friends", "list", false)
            .await
            .unwrap();
        add_members(&pool, list.id, alice, &[bob])
            .await
            .unwrap()
            .unwrap();

        let fresh = post(&pool, bob, "<p>now</p>", None).await;
        // Ingested after `fresh` but published long before it.
        let old = status::upsert_remote(
            &pool,
            status::NewRemoteStatus {
                title: None,
                object_type: None,
                external_url: None,
                uri: "https://remote.example/users/bob/statuses/old",
                account_id: bob,
                content: "<p>old</p>",
                created_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                visibility: "public",
                in_reply_to_id: None,
                in_reply_to_uri: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                url: None,
                quote_approval_policy: 0,
            },
        )
        .await
        .unwrap()
        .id;

        let published = timeline(&pool, &list, TimelineOrder::Published, None, 20)
            .await
            .unwrap();
        assert_eq!(
            published.iter().map(|s| s.id).collect::<Vec<_>>(),
            [fresh, old]
        );
        let received = timeline(&pool, &list, TimelineOrder::Received, None, 20)
            .await
            .unwrap();
        assert_eq!(
            received.iter().map(|s| s.id).collect::<Vec<_>>(),
            [old, fresh]
        );
    }

    #[sqlx::test]
    async fn crud_and_membership(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = local_account(&pool, "carol").await;

        let list = create(&pool, alice, "Friends", "list", false)
            .await
            .unwrap();
        assert_eq!(owned_by(&pool, alice).await.unwrap().len(), 1);
        assert!(find_owned(&pool, bob, list.id).await.unwrap().is_none());

        let renamed = update(&pool, list.id, "Close friends", "none", true)
            .await
            .unwrap();
        assert_eq!(renamed.title, "Close friends");
        assert_eq!(renamed.replies_policy, "none");
        assert!(renamed.exclusive);

        // Members must be followed (or the owner). Failures roll the whole
        // batch back.
        assert_eq!(
            add_members(&pool, list.id, alice, &[bob]).await.unwrap(),
            Err(AddMemberError::NotFollowed)
        );
        follow::create(&pool, alice, bob, None).await.unwrap();
        assert_eq!(
            add_members(&pool, list.id, alice, &[bob, carol])
                .await
                .unwrap(),
            Err(AddMemberError::NotFollowed)
        );
        assert!(
            members_page(&pool, list.id, None, None, None)
                .await
                .unwrap()
                .is_empty(),
            "failed batch must not leave partial members"
        );
        add_members(&pool, list.id, alice, &[bob, alice])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            add_members(&pool, list.id, alice, &[bob]).await.unwrap(),
            Err(AddMemberError::AlreadyMember)
        );
        assert_eq!(containing(&pool, alice, bob).await.unwrap().len(), 1);

        // Unfollowing cascades the membership away (Mastodon's FK).
        follow::delete(&pool, alice, bob).await.unwrap();
        let members = members_page(&pool, list.id, None, None, None)
            .await
            .unwrap();
        assert_eq!(members, [alice], "only the self-membership survives");

        remove_members(&pool, list.id, &[alice]).await.unwrap();
        assert!(delete(&pool, alice, list.id).await.unwrap());
        assert!(!delete(&pool, alice, list.id).await.unwrap());
    }

    #[sqlx::test]
    async fn timeline_membership_and_replies_policy(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let carol = local_account(&pool, "carol").await;
        let dave = local_account(&pool, "dave").await;
        follow::create(&pool, alice, bob, None).await.unwrap();
        follow::create(&pool, alice, carol, None).await.unwrap();

        let mut list = create(&pool, alice, "Friends", "list", false)
            .await
            .unwrap();
        add_members(&pool, list.id, alice, &[bob])
            .await
            .unwrap()
            .unwrap();

        let bob_post = post(&pool, bob, "<p>hi</p>", None).await;
        let carol_post = post(&pool, carol, "<p>not listed</p>", None).await;
        let self_reply = post(&pool, bob, "<p>more</p>", Some(bob_post)).await;
        let to_owner = {
            let alice_post = post(&pool, alice, "<p>own</p>", None).await;
            post(&pool, bob, "<p>@alice yes</p>", Some(alice_post)).await
        };
        let to_carol = post(&pool, bob, "<p>@carol hm</p>", Some(carol_post)).await;
        let dave_post = post(&pool, dave, "<p>unrelated</p>", None).await;
        let to_dave = post(&pool, bob, "<p>@dave hm</p>", Some(dave_post)).await;

        let ids = |statuses: Vec<Status>| statuses.iter().map(|s| s.id).collect::<Vec<_>>();

        // policy "list": replies to fellow members (none here), the owner
        // and self-replies only.
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(page.contains(&bob_post));
        assert!(page.contains(&self_reply));
        assert!(page.contains(&to_owner));
        assert!(!page.contains(&carol_post), "carol is not a member");
        assert!(!page.contains(&to_carol), "carol is not on the list");
        assert!(!page.contains(&to_dave));

        // policy "list" admits the reply once carol joins the list.
        add_members(&pool, list.id, alice, &[carol])
            .await
            .unwrap()
            .unwrap();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(page.contains(&to_carol));
        remove_members(&pool, list.id, &[carol]).await.unwrap();

        // policy "followed": replies to anyone the owner follows.
        list = update(&pool, list.id, "Friends", "followed", false)
            .await
            .unwrap();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(page.contains(&to_carol), "alice follows carol");
        assert!(!page.contains(&to_dave), "alice does not follow dave");

        // policy "none": only self-replies and replies to the owner.
        list = update(&pool, list.id, "Friends", "none", false)
            .await
            .unwrap();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(!page.contains(&to_carol));
        assert!(page.contains(&self_reply));
        assert!(page.contains(&to_owner));

        // Muting a member hides them from the list timeline too.
        mute::upsert(&pool, alice, bob, false, None).await.unwrap();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(!page.contains(&bob_post));
    }

    #[sqlx::test]
    async fn pending_members_inactive_until_accepted(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        follow::create_request(&pool, alice, bob, None)
            .await
            .unwrap();

        let list = create(&pool, alice, "Friends", "list", false)
            .await
            .unwrap();
        // A pending request may be listed (Mastodon's follow_request_id
        // memberships) but contributes nothing until accepted.
        add_members(&pool, list.id, alice, &[bob])
            .await
            .unwrap()
            .unwrap();
        let bob_post = post(&pool, bob, "<p>hi</p>", None).await;
        assert!(timeline_both(&pool, &list, None, 40).await.is_empty());
        assert!(
            streams_receiving(&pool, &[list.id], bob_post)
                .await
                .unwrap()
                .is_empty()
        );

        follow::mark_accepted(&pool, alice, bob).await.unwrap();
        assert_eq!(timeline_both(&pool, &list, None, 40).await.len(), 1);
        let receiving = streams_receiving(&pool, &[list.id], bob_post)
            .await
            .unwrap();
        assert_eq!(receiving.len(), 1);
        assert_eq!(receiving[0].owner_id, alice);
        assert_eq!(
            streams_with_member(&pool, &[list.id], bob).await.unwrap(),
            [list.id]
        );
    }

    /// The per-follow flag ANDs with the list's own `replies_policy` — a
    /// list is never *less* filtered than home (Mastodon runs
    /// `filter_from_list?` and then `filter_from_home`).
    ///
    /// On plain replies the two rules coincide here, and that is worth stating
    /// rather than discovering: list membership requires a follow, so
    /// `replies_policy` "list" and "followed" both already restrict replies to
    /// accounts the owner follows — exactly the third exemption. What the
    /// flag adds to a list: `replies_policy` tests `s.in_reply_to_id`,
    /// which is NULL on every Announce wrapper, so an announced comment passes
    /// every policy and only the flag takes it away.
    #[sqlx::test]
    async fn with_replies_ands_with_the_replies_policy(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let dave = local_account(&pool, "dave").await;
        follow::create(&pool, alice, bob, None).await.unwrap();
        follow::create(&pool, alice, dave, None).await.unwrap();

        let list = create(&pool, alice, "Friends", "list", false)
            .await
            .unwrap();
        add_members(&pool, list.id, alice, &[bob, dave])
            .await
            .unwrap()
            .unwrap();

        let dave_post = post(&pool, dave, "<p>dave</p>", None).await;
        let dave_comment = post(&pool, dave, "<p>more</p>", Some(dave_post)).await;
        let bob_post = post(&pool, bob, "<p>hi</p>", None).await;
        let to_dave = post(&pool, bob, "<p>@dave hm</p>", Some(dave_post)).await;
        let self_reply = post(&pool, bob, "<p>more</p>", Some(bob_post)).await;
        let boosted_post = status::create_local_reblog(&pool, bob, dave_post)
            .await
            .unwrap()
            .id;
        let boosted_comment = status::create_local_reblog(&pool, bob, dave_comment)
            .await
            .unwrap()
            .id;

        let ids = |statuses: Vec<Status>| statuses.iter().map(|s| s.id).collect::<Vec<_>>();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(page.contains(&to_dave));
        assert!(page.contains(&boosted_comment), "flag on keeps everything");

        follow::update_settings(&pool, alice, bob, None, Some(false), None, None)
            .await
            .unwrap();
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(
            !page.contains(&boosted_comment),
            "an announced comment goes"
        );
        assert!(page.contains(&boosted_post), "an announced post stays");
        assert!(
            page.contains(&to_dave),
            "a reply to a followed member stays"
        );
        assert!(page.contains(&self_reply), "self-threads are exempt");
        assert!(page.contains(&bob_post), "originals are unaffected");

        // A reply to someone outside the follow set is dropped by both rules;
        // asserting it here keeps the flag's own arm honest if the policy ever
        // loosens.
        let stranger = local_account(&pool, "stranger").await;
        let stranger_post = post(&pool, stranger, "<p>who</p>", None).await;
        let to_stranger = post(&pool, bob, "<p>@stranger</p>", Some(stranger_post)).await;
        let page = ids(timeline_both(&pool, &list, None, 40).await);
        assert!(!page.contains(&to_stranger));

        // `streams_receiving` is the live half of the same query and must
        // answer identically, row for row.
        for (status_id, expected) in [
            (to_dave, true),
            (self_reply, true),
            (bob_post, true),
            (to_stranger, false),
            (boosted_comment, false),
            (boosted_post, true),
        ] {
            let receiving = streams_receiving(&pool, &[list.id], status_id)
                .await
                .unwrap();
            assert_eq!(
                !receiving.is_empty(),
                expected,
                "stream and timeline disagree on status {status_id}"
            );
        }
    }
}
