//! Follow relationships between accounts.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};

use crate::{DbError, id};

/// Records `account_id` following `target_account_id`. Idempotent: a repeated
/// Follow refreshes the activity URI and keeps the original row. Executor-generic
/// so a follow row can commit inside the same transaction as its outbox
/// `Follow`/`Accept` job.
pub async fn create<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
    uri: Option<&str>,
) -> Result<i64, DbError> {
    insert(executor, account_id, target_account_id, uri, false).await
}

/// Records an inbound follow request toward a locked account, pending until
/// the target authorizes it. Idempotent like [`create`]: a repeat refreshes
/// the activity URI and keeps the row pending.
pub async fn create_request<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
    uri: Option<&str>,
) -> Result<i64, DbError> {
    insert(executor, account_id, target_account_id, uri, true).await
}

/// Records a follow we initiate, pending until the remote side `Accept`s.
pub async fn create_outgoing<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
    uri: &str,
) -> Result<i64, DbError> {
    insert(executor, account_id, target_account_id, Some(uri), true).await
}

async fn insert<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
    uri: Option<&str>,
    pending: bool,
) -> Result<i64, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO follows (id, account_id, target_account_id, uri, pending)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (account_id, target_account_id)
            DO UPDATE SET uri = EXCLUDED.uri, pending = EXCLUDED.pending
        RETURNING id
        "#,
        id::next(),
        account_id,
        target_account_id,
        uri,
        pending,
    )
    .fetch_one(executor)
    .await?;
    Ok(row_id)
}

/// Marks an outgoing follow accepted; returns whether a row matched.
pub async fn mark_accepted<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE follows SET pending = FALSE WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A follow edge as stored: its activity URI, pending state, and the
/// per-relationship settings (M32, plus `with_replies`). `languages: None`
/// means no language filter.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // independent per-relationship toggles
pub struct FollowEdge {
    pub uri: Option<String>,
    pub pending: bool,
    pub show_reblogs: bool,
    pub with_replies: bool,
    pub notify: bool,
    pub languages: Option<Vec<String>>,
}

/// The follow edge from `account_id` to `target_account_id`, if any.
pub async fn find<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    target_account_id: i64,
) -> Result<Option<FollowEdge>, DbError> {
    let edge = sqlx::query_as!(
        FollowEdge,
        r#"
        SELECT uri, pending, show_reblogs, with_replies, notify, languages
        FROM follows
        WHERE account_id = $1 AND target_account_id = $2
        "#,
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(edge)
}

/// The outgoing follow edges from `account_id` to each of `target_ids`
/// (accepted or pending, with per-follow settings), keyed by target, in one
/// query — the batched form of [`find`] for rendering a page of relationships.
pub async fn find_out_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<HashMap<i64, FollowEdge>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT target_account_id AS "id!", uri, pending, show_reblogs, with_replies,
               notify, languages
        FROM follows
        WHERE account_id = $1 AND target_account_id = ANY($2)
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
                FollowEdge {
                    uri: row.uri,
                    pending: row.pending,
                    show_reblogs: row.show_reblogs,
                    with_replies: row.with_replies,
                    notify: row.notify,
                    languages: row.languages,
                },
            )
        })
        .collect())
}

/// The `follows.id` of each outgoing edge (accepted or pending) from
/// `account_id` to a target in `target_ids`, keyed by target, in one query —
/// the batched form of the per-target lookup `list::add_members` needs to stamp
/// `list_accounts.follow_id` without a round trip per member.
pub async fn find_out_ids_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<HashMap<i64, i64>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT target_account_id AS "target!", id AS "follow_id!"
           FROM follows
           WHERE account_id = $1 AND target_account_id = ANY($2)"#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.target, row.follow_id))
        .collect())
}

/// The incoming follow state (`pending`) from each of `source_ids` toward
/// `target_account_id`, keyed by source, in one query — the batched form of
/// [`pending_state`].
pub async fn pending_state_in_batch<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    target_account_id: i64,
    source_ids: &[i64],
) -> Result<HashMap<i64, bool>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id AS "id!", pending
           FROM follows
           WHERE target_account_id = $1 AND account_id = ANY($2)"#,
        target_account_id,
        source_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.id, row.pending)).collect())
}

/// Existing incoming edges (accepted or pending) between a batch of authors
/// and potential recipients, as `(author, recipient)` pairs. A locally
/// silenced author's AP addressing is restricted to precisely this set.
pub async fn existing_in_edges<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    author_ids: &[i64],
    recipient_ids: &[i64],
) -> Result<std::collections::HashSet<(i64, i64)>, DbError> {
    if author_ids.is_empty() || recipient_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT target_account_id AS "author!", account_id AS "recipient!"
        FROM follows
        WHERE target_account_id = ANY($1) AND account_id = ANY($2)
        "#,
        author_ids,
        recipient_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.author, row.recipient))
        .collect())
}

/// Of `target_ids`, the subset that `account_id` (accepted-)follows — the
/// outgoing accepted edges from one account to a batch of targets, resolved
/// in a single query so a page of accounts can be checked without a
/// round trip per row.
pub async fn accepted_out_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT target_account_id AS "id!"
           FROM follows
           WHERE account_id = $1 AND target_account_id = ANY($2) AND NOT pending"#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Of `follower_ids`, the subset that (accepted-)follow `target_account_id` —
/// the incoming accepted edges from a batch of accounts to one target, in a
/// single query.
pub async fn accepted_in_batch(
    pool: &PgPool,
    target_account_id: i64,
    follower_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT account_id AS "id!"
           FROM follows
           WHERE target_account_id = $1 AND account_id = ANY($2) AND NOT pending"#,
        target_account_id,
        follower_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// [`accepted_out_batch`] across a set of viewers in one query — `(viewer,
/// target)` pairs where the viewer accepted-follows the target.
pub async fn accepted_out_edges(
    pool: &PgPool,
    viewer_ids: &[i64],
    target_ids: &[i64],
) -> Result<std::collections::HashSet<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, target_account_id
           FROM follows
           WHERE account_id = ANY($1) AND target_account_id = ANY($2) AND NOT pending"#,
        viewer_ids,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.target_account_id))
        .collect())
}

/// [`accepted_in_batch`] across a set of viewers in one query — `(viewer,
/// follower)` pairs where the follower accepted-follows the viewer.
pub async fn accepted_in_edges(
    pool: &PgPool,
    viewer_ids: &[i64],
    follower_ids: &[i64],
) -> Result<std::collections::HashSet<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT target_account_id, account_id
           FROM follows
           WHERE target_account_id = ANY($1) AND account_id = ANY($2) AND NOT pending"#,
        viewer_ids,
        follower_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.target_account_id, row.account_id))
        .collect())
}

/// Updates a follow's per-relationship settings. `None` fields keep the
/// stored value (Mastodon's `follow!`: an absent option never resets), while
/// `Some(&[])` for `languages` clears the filter back to "all languages".
/// Returns whether an edge matched.
pub async fn update_settings(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
    show_reblogs: Option<bool>,
    with_replies: Option<bool>,
    notify: Option<bool>,
    languages: Option<&[String]>,
) -> Result<bool, DbError> {
    // An empty language list behaves exactly like NULL everywhere it is
    // read, so it is normalized to NULL at the door.
    let replace_languages = languages.is_some();
    let new_languages = languages.filter(|langs| !langs.is_empty());
    let result = sqlx::query!(
        r#"
        UPDATE follows SET
            show_reblogs = COALESCE($3, show_reblogs),
            with_replies = COALESCE($4, with_replies),
            notify = COALESCE($5, notify),
            languages = CASE WHEN $6 THEN $7::text[] ELSE languages END
        WHERE account_id = $1 AND target_account_id = $2
        "#,
        account_id,
        target_account_id,
        show_reblogs,
        with_replies,
        notify,
        replace_languages,
        new_languages,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Local accounts to notify about a fresh post by `author_id` (the `status`
/// notification kind): accepted followers with `notify` set, minus viewers
/// whose per-follow language filter excludes the post's language and viewers
/// hiding the author (mute/block, like the home timeline).
pub async fn notify_follower_ids(
    pool: &PgPool,
    author_id: i64,
    language: Option<&str>,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.account_id AS "id!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND f.notify AND NOT f.pending
          AND a.domain IS NULL
          AND ($2::text IS NULL OR f.languages IS NULL OR $2 = ANY(f.languages))
          AND NOT account_hidden(f.account_id, $1)
        ORDER BY f.account_id
        "#,
        author_id,
        language,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Pending state of a follow relationship, if one exists.
pub async fn pending_state(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<Option<bool>, DbError> {
    let pending = sqlx::query_scalar!(
        "SELECT pending FROM follows WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(pending)
}

/// Distinct inboxes of `target_account_id`'s remote followers, preferring
/// shared inboxes — the fan-out list for a new post.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn follower_inboxes<'e, E: PgExecutor<'e>>(
    executor: E,
    target_account_id: i64,
) -> Result<Vec<String>, DbError> {
    let inboxes = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT
            CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url
                 ELSE a.inbox_url
            END AS "inbox!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND a.domain IS NOT NULL
          AND a.suspended_at IS NULL AND NOT f.pending
        "#,
        target_account_id,
    )
    .fetch_all(executor)
    .await?;
    Ok(inboxes)
}

/// Ids of the *local* accounts that follow `target_account_id` (accepted
/// follows only). The account-migration handler re-points each of these
/// follows at the migration target.
pub async fn local_follower_ids(
    pool: &PgPool,
    target_account_id: i64,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.account_id AS "id!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND a.domain IS NULL AND NOT f.pending
        ORDER BY f.account_id
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Usernames of local accounts that follow `target_account_id`, accepted
/// follows only. The server layer turns these into local actor IRIs for
/// Mastodon's local-followers synchronization digest.
#[derive(Debug, sqlx::FromRow)]
pub struct LocalFollowerIdentity {
    pub username: String,
    pub uri: Option<String>,
}

pub async fn local_follower_identities(
    pool: &PgPool,
    target_account_id: i64,
) -> Result<Vec<LocalFollowerIdentity>, DbError> {
    let identities = sqlx::query_as!(
        LocalFollowerIdentity,
        r#"
        SELECT a.username, a.uri
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND a.domain IS NULL AND NOT f.pending
        ORDER BY a.id
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(identities)
}

/// Remote follower actor IRIs whose actor URI is either exactly `uri_prefix`
/// or below it (`{prefix}/...`). Mastodon scopes followers synchronization by
/// URI prefix, not by the stored acct domain.
pub async fn follower_uris_matching_prefix(
    pool: &PgPool,
    target_account_id: i64,
    uri_prefix: &str,
) -> Result<Vec<String>, DbError> {
    let uris = sqlx::query_scalar!(
        r#"
        SELECT a.uri AS "uri!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1
          AND a.uri IS NOT NULL
          AND (a.uri = $2 OR a.uri LIKE ($2 || '/%'))
          AND NOT f.pending
        ORDER BY a.uri
        "#,
        target_account_id,
        uri_prefix,
    )
    .fetch_all(pool)
    .await?;
    Ok(uris)
}

/// One entry of a followers/following listing: the follow row id (the
/// pagination key clients page by) and the account on the other end.
#[derive(Debug)]
pub struct FollowListEntry {
    pub follow_id: i64,
    pub account_id: i64,
}

/// Accepted followers of `target_account_id`, newest follow first, keyset-
/// paginated by follow row id like Mastodon's `paginate_by_max_id`.
///
/// A follower who set `hide_collections` is omitted from other accounts'
/// lists, but stays visible to themselves (`viewer` equals that follower)
/// and to the list owner viewing their own followers (`viewer` equals
/// `target_account_id`). Pass `viewer = Some(target_account_id)` to disable
/// member hiding entirely (owner-context / server-to-server callers).
pub async fn followers_of(
    pool: &PgPool,
    target_account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<FollowListEntry>, DbError> {
    let entries = sqlx::query_as!(
        FollowListEntry,
        r#"
        SELECT f.id AS "follow_id!", f.account_id AS "account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND NOT f.pending
          AND a.suspended_at IS NULL
          AND (NOT a.hide_collections OR a.id = $5 OR $1 = $5)
          AND ($2::bigint IS NULL OR f.id < $2)
          AND ($3::bigint IS NULL OR f.id > $3)
        ORDER BY f.id DESC
        LIMIT $4
        "#,
        target_account_id,
        max_id,
        since_id,
        limit,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// Pending follow requests toward `target_account_id` (Mastodon's
/// `follow_requests` listing), newest first, keyset-paginated by follow row
/// id.
pub async fn requests_of(
    pool: &PgPool,
    target_account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<FollowListEntry>, DbError> {
    let entries = sqlx::query_as!(
        FollowListEntry,
        r#"
        SELECT f.id AS "follow_id!", f.account_id AS "account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND f.pending
          AND a.suspended_at IS NULL
          AND ($2::bigint IS NULL OR f.id < $2)
          AND ($3::bigint IS NULL OR f.id > $3)
        ORDER BY f.id DESC
        LIMIT $4
        "#,
        target_account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// Number of pending follow requests toward `target_account_id`, counted up
/// to `cap` — Mastodon's credential serializer caps this at 40.
pub async fn count_requests(
    pool: &PgPool,
    target_account_id: i64,
    cap: i64,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM (
            SELECT 1 AS one FROM follows
            WHERE target_account_id = $1 AND pending
            LIMIT $2
        ) AS capped
        "#,
        target_account_id,
        cap,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Pending follow requests `account_id` has *sent* — outgoing follows still
/// awaiting the target's `Accept`, newest first, keyset-paginated by follow
/// row id. The mirror of [`requests_of`] on the initiating side: where
/// `requests_of` filters `target_account_id = you`, this filters
/// `account_id = you`. A row lands here whether the target approves followers
/// manually and hasn't decided yet, or the delivery simply went unanswered —
/// the `pending` flag doesn't distinguish the two.
pub async fn sent_requests_of(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<FollowListEntry>, DbError> {
    let entries = sqlx::query_as!(
        FollowListEntry,
        r#"
        SELECT f.id AS "follow_id!", f.target_account_id AS "account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND f.pending
          AND a.suspended_at IS NULL
          AND ($2::bigint IS NULL OR f.id < $2)
          AND ($3::bigint IS NULL OR f.id > $3)
        ORDER BY f.id DESC
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

/// Number of pending follow requests `account_id` has sent, counted up to
/// `cap` — the outgoing mirror of [`count_requests`], for the tab badge.
pub async fn count_sent_requests(pool: &PgPool, account_id: i64, cap: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM (
            SELECT 1 AS one FROM follows
            WHERE account_id = $1 AND pending
            LIMIT $2
        ) AS capped
        "#,
        account_id,
        cap,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Accounts `account_id` follows (accepted only), newest follow first,
/// keyset-paginated by follow row id.
///
/// A followed account that set `hide_collections` is omitted from other
/// accounts' lists, but stays visible to itself (`viewer` equals that
/// account) and to the list owner viewing their own following (`viewer`
/// equals `account_id`). Pass `viewer = Some(account_id)` to disable member
/// hiding entirely (owner-context / server-to-server callers).
pub async fn following_of(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<FollowListEntry>, DbError> {
    let entries = sqlx::query_as!(
        FollowListEntry,
        r#"
        SELECT f.id AS "follow_id!", f.target_account_id AS "account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND NOT f.pending
          AND a.suspended_at IS NULL
          AND (NOT a.hide_collections OR a.id = $5 OR $1 = $5)
          AND ($2::bigint IS NULL OR f.id < $2)
          AND ($3::bigint IS NULL OR f.id > $3)
        ORDER BY f.id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// Follower account ids of `target_account_id` for an offset/limit page —
/// the `ActivityPub` collection pages, which are page-numbered, not keyset.
///
/// Members that set `hide_collections` are omitted for everyone but
/// themselves (`viewer` equals the member) and the list owner (`viewer`
/// equals `target_account_id`). Pass `viewer = Some(target_account_id)` to
/// disable member hiding (owner-context / server-to-server callers).
pub async fn followers_page(
    pool: &PgPool,
    target_account_id: i64,
    offset: i64,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.account_id AS "account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND NOT f.pending
          AND a.suspended_at IS NULL
          AND (NOT a.hide_collections OR a.id = $4 OR $1 = $4)
        ORDER BY f.id DESC
        OFFSET $2 LIMIT $3
        "#,
        target_account_id,
        offset,
        limit,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Followed account ids of `account_id` for an offset/limit page.
///
/// Members that set `hide_collections` are omitted for everyone but
/// themselves (`viewer` equals the member) and the list owner (`viewer`
/// equals `account_id`). Pass `viewer = Some(account_id)` to disable member
/// hiding (owner-context / server-to-server callers).
pub async fn following_page(
    pool: &PgPool,
    account_id: i64,
    offset: i64,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.target_account_id AS "target_account_id!"
        FROM follows f
        JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND NOT f.pending
          AND a.suspended_at IS NULL
          AND (NOT a.hide_collections OR a.id = $4 OR $1 = $4)
        ORDER BY f.id DESC
        OFFSET $2 LIMIT $3
        "#,
        account_id,
        offset,
        limit,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Mutually-followed account ids of `account_id` — accounts this account
/// follows that also follow it back — for an offset/limit page (the
/// relationships manager's "mutual" filter).
pub async fn mutual_page(
    pool: &PgPool,
    account_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.target_account_id FROM follows f
        WHERE f.account_id = $1 AND NOT f.pending
          AND EXISTS (
              SELECT 1 FROM follows g
              WHERE g.account_id = f.target_account_id
                AND g.target_account_id = $1 AND NOT g.pending
          )
        ORDER BY f.id DESC
        OFFSET $2 LIMIT $3
        "#,
        account_id,
        offset,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Removes a follow relationship; returns whether one existed. Executor-generic
/// so an unfollow/block row change commits with its outbox `Undo`/`Reject` job.
pub async fn delete<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_account_id,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn exists(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM follows WHERE account_id = $1 AND target_account_id = $2"#,
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Number of accounts `account_id` follows (accepted only).
pub async fn count_following(pool: &PgPool, account_id: i64) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM follows WHERE account_id = $1 AND NOT pending"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Number of accounts following `target_account_id` (accepted only, so the
/// count always matches what the followers listings show).
pub async fn count_followers(pool: &PgPool, target_account_id: i64) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM follows WHERE target_account_id = $1 AND NOT pending"#,
        target_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// [`count_following`] for every id in `account_ids` in one query; ids with
/// no accepted following are absent from the map (treat as 0).
pub async fn count_following_batch(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, u64>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id AS "id!", count(*) AS "count!"
           FROM follows WHERE account_id = ANY($1) AND NOT pending GROUP BY 1"#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, u64::try_from(row.count).unwrap_or(0)))
        .collect())
}

/// [`count_followers`] for every id in `target_account_ids` in one query; ids
/// with no accepted followers are absent from the map (treat as 0).
pub async fn count_followers_batch(
    pool: &PgPool,
    target_account_ids: &[i64],
) -> Result<HashMap<i64, u64>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT target_account_id AS "id!", count(*) AS "count!"
           FROM follows WHERE target_account_id = ANY($1) AND NOT pending GROUP BY 1"#,
        target_account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, u64::try_from(row.count).unwrap_or(0)))
        .collect())
}

/// One `(target, familiar follower)` pair of [`familiar_followers`].
#[derive(Debug)]
pub struct FamiliarFollow {
    pub target_id: i64,
    pub follower_id: i64,
}

/// For each id in `target_ids`, the accounts `viewer_id` follows (accepted)
/// who also (accepted-)follow that target — Mastodon's
/// `FamiliarFollowersPresenter`. The viewer is never their own familiar
/// follower. Ordered by target then follower id; the caller regroups.
pub async fn familiar_followers(
    pool: &PgPool,
    viewer_id: i64,
    target_ids: &[i64],
) -> Result<Vec<FamiliarFollow>, DbError> {
    let rows = sqlx::query_as!(
        FamiliarFollow,
        r#"
        SELECT f.target_account_id AS "target_id!", f.account_id AS "follower_id!"
        FROM follows f
        JOIN accounts familiar ON familiar.id = f.account_id
        WHERE f.target_account_id = ANY($2) AND NOT f.pending
          AND familiar.suspended_at IS NULL
          AND f.account_id <> $1
          AND f.account_id IN (
              SELECT target_account_id FROM follows
              WHERE account_id = $1 AND NOT pending
          )
        ORDER BY f.target_account_id, f.account_id
        "#,
        viewer_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One follow edge between an account and the accounts of a blocked domain,
/// carrying what the severance worker needs to retract it on the wire.
/// `other_*` is the remote side, whichever
/// direction the edge runs.
#[derive(Debug)]
pub struct DomainEdge {
    pub other_account_id: i64,
    /// The follow activity's own URI (ours for outgoing, theirs for incoming);
    /// `None` means nothing to retract by reference.
    pub edge_uri: Option<String>,
    pub other_uri: Option<String>,
    pub other_inbox_url: String,
    pub pending: bool,
}

/// A remote account's follow toward one of our local actors. These edges are
/// irreversibly rejected when the remote account is locally suspended: its
/// origin has not disabled it, so leaving the edge in place would keep giving
/// it follower-only posts.
#[derive(Debug)]
pub struct SuspendedFollowerEdge {
    pub target_account_id: i64,
    pub target_username: String,
    pub edge_uri: Option<String>,
    pub pending: bool,
}

pub async fn local_edges_from<'e, E: PgExecutor<'e>>(
    executor: E,
    follower_account_id: i64,
) -> Result<Vec<SuspendedFollowerEdge>, DbError> {
    let rows = sqlx::query_as!(
        SuspendedFollowerEdge,
        r#"
        SELECT f.target_account_id AS "target_account_id!",
               target.username AS "target_username!", f.uri AS edge_uri,
               f.pending
        FROM follows f
        JOIN accounts target ON target.id = f.target_account_id
        WHERE f.account_id = $1 AND target.domain IS NULL
        ORDER BY f.id
        "#,
        follower_account_id,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows)
}

/// The outgoing follow edges from `account_id` toward accounts on `domain`,
/// with the remote-side routing facts, in one query.
pub async fn edges_to_domain(
    pool: &PgPool,
    account_id: i64,
    domain: &str,
) -> Result<Vec<DomainEdge>, DbError> {
    let rows = sqlx::query_as!(
        DomainEdge,
        r#"
        SELECT f.target_account_id AS "other_account_id!", f.uri AS edge_uri,
               target.uri AS other_uri, target.inbox_url AS "other_inbox_url!",
               f.pending
        FROM follows f
        JOIN accounts target ON target.id = f.target_account_id
        WHERE f.account_id = $1 AND target.domain = $2
        ORDER BY f.id
        "#,
        account_id,
        domain,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The incoming follow edges from accounts on `domain` toward `account_id`,
/// with the remote-side routing facts, in one query.
pub async fn edges_from_domain(
    pool: &PgPool,
    account_id: i64,
    domain: &str,
) -> Result<Vec<DomainEdge>, DbError> {
    let rows = sqlx::query_as!(
        DomainEdge,
        r#"
        SELECT f.account_id AS "other_account_id!", f.uri AS edge_uri,
               follower.uri AS other_uri, follower.inbox_url AS "other_inbox_url!",
               f.pending
        FROM follows f
        JOIN accounts follower ON follower.id = f.account_id
        WHERE f.target_account_id = $1 AND follower.domain = $2
        ORDER BY f.id
        "#,
        account_id,
        domain,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Set-deletes the outgoing edges from `account_id` to each of `target_ids`.
/// Executor-generic so the severance commits with its outbox retractions.
pub async fn delete_out_many<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_ids: &[i64],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM follows WHERE account_id = $1 AND target_account_id = ANY($2)",
        account_id,
        target_ids,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Set-deletes the incoming edges from each of `source_ids` to `account_id`.
pub async fn delete_in_many<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    source_ids: &[i64],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM follows WHERE target_account_id = $1 AND account_id = ANY($2)",
        account_id,
        source_ids,
    )
    .execute(executor)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount, RemoteAccountData};

    async fn local_and_remote(pool: &PgPool) -> (i64, i64) {
        let local = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let remote = account::upsert_remote(
            pool,
            RemoteAccountData {
                username: "bob",
                domain: "remote.example",
                uri: "https://remote.example/users/bob",
                display_name: "",
                note: "",
                inbox_url: "https://remote.example/users/bob/inbox",
                shared_inbox_url: "",
                public_key_pem: "pub",
                public_key_id: "https://remote.example/users/bob#main-key",
                avatar_remote_url: None,
                header_remote_url: None,
                avatar_description: "",
                header_description: "",
                created_at: None,
                fields: Vec::new(),
                featured_collection_url: None,
                locked: false,
                also_known_as: &[],
                moved_to_uri: None,
                url: None,
                discoverable: false,
                feature_approval_policy: 0,
                is_bot: false,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                actor_type: None,
            },
        )
        .await
        .unwrap();
        (local.id, remote.id)
    }

    async fn make_local(pool: &PgPool, username: &str) -> i64 {
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
    async fn local_follower_ids_excludes_remote_and_pending(pool: PgPool) {
        let (target, remote_follower) = local_and_remote(&pool).await;
        let local_follower = make_local(&pool, "carol").await;
        let pending_local = make_local(&pool, "dave").await;

        create(&pool, local_follower, target, None).await.unwrap();
        create(&pool, remote_follower, target, None).await.unwrap();
        create_request(&pool, pending_local, target, None)
            .await
            .unwrap();

        let ids = local_follower_ids(&pool, target).await.unwrap();
        assert_eq!(ids, vec![local_follower], "local + accepted only");
    }

    #[sqlx::test]
    async fn suspension_edges_include_remote_follows_to_local_accounts(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        let other_local = make_local(&pool, "carol").await;
        create(
            &pool,
            remote,
            local,
            Some("https://remote.example/follows/1"),
        )
        .await
        .unwrap();
        create_request(
            &pool,
            remote,
            other_local,
            Some("https://remote.example/follows/2"),
        )
        .await
        .unwrap();

        let edges = local_edges_from(&pool, remote).await.unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().any(|edge| edge.target_account_id == local));
        assert!(
            edges
                .iter()
                .any(|edge| edge.target_account_id == other_local && edge.pending)
        );
    }

    #[sqlx::test]
    async fn settings_default_keep_and_clear(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        create(&pool, local, remote, None).await.unwrap();

        let edge = find(&pool, local, remote).await.unwrap().unwrap();
        assert!(edge.show_reblogs, "boosts shown by default");
        assert!(!edge.notify, "notify off by default");
        assert_eq!(edge.languages, None, "no language filter by default");

        let langs = vec!["en".to_owned(), "de".to_owned()];
        assert!(
            update_settings(
                &pool,
                local,
                remote,
                Some(false),
                None,
                Some(true),
                Some(&langs)
            )
            .await
            .unwrap()
        );
        let edge = find(&pool, local, remote).await.unwrap().unwrap();
        assert!(!edge.show_reblogs);
        assert!(edge.notify);
        assert_eq!(edge.languages.as_deref(), Some(langs.as_slice()));

        // Absent options keep the stored values.
        assert!(
            update_settings(&pool, local, remote, None, None, None, None)
                .await
                .unwrap()
        );
        let edge = find(&pool, local, remote).await.unwrap().unwrap();
        assert!(!edge.show_reblogs);
        assert!(edge.notify);
        assert_eq!(edge.languages.as_deref(), Some(langs.as_slice()));

        // An empty language list clears the filter (normalized to NULL).
        assert!(
            update_settings(
                &pool,
                local,
                remote,
                Some(true),
                None,
                Some(false),
                Some(&[])
            )
            .await
            .unwrap()
        );
        let edge = find(&pool, local, remote).await.unwrap().unwrap();
        assert!(edge.show_reblogs);
        assert!(!edge.notify);
        assert_eq!(edge.languages, None);

        // No edge in the other direction.
        assert!(
            !update_settings(&pool, remote, local, Some(false), None, None, None)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn notify_follower_ids_filters(pool: PgPool) {
        let (author, remote_follower) = local_and_remote(&pool).await;
        let plain = make_local(&pool, "carol").await;
        let notifying = make_local(&pool, "dave").await;
        let german_only = make_local(&pool, "erin").await;
        let muter = make_local(&pool, "frank").await;
        let pending = make_local(&pool, "grace").await;

        create(&pool, plain, author, None).await.unwrap();
        for follower in [notifying, german_only, muter, remote_follower] {
            create(&pool, follower, author, None).await.unwrap();
            update_settings(&pool, follower, author, None, None, Some(true), None)
                .await
                .unwrap();
        }
        update_settings(
            &pool,
            german_only,
            author,
            None,
            None,
            None,
            Some(&["de".to_owned()]),
        )
        .await
        .unwrap();
        crate::mute::upsert(&pool, muter, author, true, None)
            .await
            .unwrap();
        create_request(&pool, pending, author, None).await.unwrap();
        update_settings(&pool, pending, author, None, None, Some(true), None)
            .await
            .unwrap();

        // No language: everyone notifying, local, accepted, not muting.
        assert_eq!(
            notify_follower_ids(&pool, author, None).await.unwrap(),
            vec![notifying, german_only]
        );
        // A language outside a follower's filter drops that follower.
        assert_eq!(
            notify_follower_ids(&pool, author, Some("en"))
                .await
                .unwrap(),
            vec![notifying]
        );
        assert_eq!(
            notify_follower_ids(&pool, author, Some("de"))
                .await
                .unwrap(),
            vec![notifying, german_only]
        );
    }

    #[sqlx::test]
    async fn follow_lifecycle(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        assert!(!exists(&pool, remote, local).await.unwrap());

        let first = create(
            &pool,
            remote,
            local,
            Some("https://remote.example/follows/1"),
        )
        .await
        .unwrap();
        assert!(exists(&pool, remote, local).await.unwrap());
        assert_eq!(count_followers(&pool, local).await.unwrap(), 1);

        // Re-follow is idempotent and keeps the row.
        let second = create(
            &pool,
            remote,
            local,
            Some("https://remote.example/follows/2"),
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(count_followers(&pool, local).await.unwrap(), 1);

        assert!(delete(&pool, remote, local).await.unwrap());
        assert!(!exists(&pool, remote, local).await.unwrap());
        assert!(!delete(&pool, remote, local).await.unwrap());
    }

    #[sqlx::test]
    async fn batch_counts_match_single_lookups(pool: PgPool) {
        let alice = make_local(&pool, "alice").await;
        let bob = make_local(&pool, "bob").await;
        let carol = make_local(&pool, "carol").await;
        // alice follows bob and carol; bob follows alice. carol follows
        // nobody, so she's the one absent-from-a-map case: no row in the
        // `following` batch result, treated by callers as 0.
        create(&pool, alice, bob, None).await.unwrap();
        create(&pool, alice, carol, None).await.unwrap();
        create(&pool, bob, alice, None).await.unwrap();

        let ids = [alice, bob, carol];
        let followers = count_followers_batch(&pool, &ids).await.unwrap();
        let following = count_following_batch(&pool, &ids).await.unwrap();
        for id in ids {
            assert_eq!(
                followers.get(&id).copied().unwrap_or(0),
                count_followers(&pool, id).await.unwrap(),
                "followers mismatch for {id}"
            );
            assert_eq!(
                following.get(&id).copied().unwrap_or(0),
                count_following(&pool, id).await.unwrap(),
                "following mismatch for {id}"
            );
        }
        assert!(!following.contains_key(&carol));

        assert!(count_followers_batch(&pool, &[]).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn deleting_remote_account_cascades_follows(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        create(&pool, remote, local, None).await.unwrap();
        account::delete_by_uri(&pool, "https://remote.example/users/bob")
            .await
            .unwrap();
        assert_eq!(count_followers(&pool, local).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn outgoing_follow_is_pending_until_accepted(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        create_outgoing(
            &pool,
            local,
            remote,
            "https://plamenu.test/users/alice#follows/1",
        )
        .await
        .unwrap();
        assert_eq!(
            pending_state(&pool, local, remote).await.unwrap(),
            Some(true)
        );

        assert!(mark_accepted(&pool, local, remote).await.unwrap());
        assert_eq!(
            pending_state(&pool, local, remote).await.unwrap(),
            Some(false)
        );
        assert!(
            !mark_accepted(&pool, remote, local).await.unwrap(),
            "no such follow"
        );
        assert_eq!(pending_state(&pool, remote, local).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn listings_page_newest_first_and_skip_pending(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        let carol = account::create_local(
            &pool,
            NewLocalAccount {
                username: "carol",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let first = create(&pool, remote, local, None).await.unwrap();
        let second = create(&pool, carol.id, local, None).await.unwrap();
        // A pending outgoing follow must not appear anywhere.
        create_outgoing(&pool, local, remote, "uri").await.unwrap();

        let followers = followers_of(&pool, local, None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(
            followers
                .iter()
                .map(|e| (e.follow_id, e.account_id))
                .collect::<Vec<_>>(),
            [(second, carol.id), (first, remote)],
            "newest follow first"
        );
        // Keyset bounds page by follow row id.
        let older = followers_of(&pool, local, Some(second), None, 10, None)
            .await
            .unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].account_id, remote);
        let newer = followers_of(&pool, local, None, Some(first), 10, None)
            .await
            .unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].account_id, carol.id);

        assert!(
            following_of(&pool, local, None, None, 10, None)
                .await
                .unwrap()
                .is_empty(),
            "pending outgoing follow is not 'following' yet"
        );
        assert_eq!(count_followers(&pool, remote).await.unwrap(), 0);
        mark_accepted(&pool, local, remote).await.unwrap();
        let following = following_of(&pool, local, None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(following.len(), 1);
        assert_eq!(following[0].account_id, remote);

        assert_eq!(
            followers_page(&pool, local, 0, 1, None).await.unwrap(),
            [carol.id]
        );
        assert_eq!(
            followers_page(&pool, local, 1, 1, None).await.unwrap(),
            [remote]
        );
        assert!(
            followers_page(&pool, local, 2, 1, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            following_page(&pool, local, 0, 10, None).await.unwrap(),
            [remote]
        );

        account::suspend(&pool, remote, "remote").await.unwrap();
        assert!(
            followers_of(&pool, local, None, None, 10, Some(local))
                .await
                .unwrap()
                .iter()
                .all(|entry| entry.account_id != remote),
            "suspended followers are absent even in owner context"
        );
        assert!(
            following_of(&pool, local, None, None, 10, Some(local))
                .await
                .unwrap()
                .is_empty(),
            "suspended followed accounts are absent even in owner context"
        );
    }

    /// A member who set `hide_collections` drops out of *other* accounts'
    /// follower/following lists, but stays visible to themselves and to the
    /// list's owner. Covers both the keyset (`*_of`) and offset (`*_page`)
    /// helpers, in both directions.
    #[sqlx::test]
    async fn hidden_member_filtered_from_others_lists_with_exceptions(pool: PgPool) {
        let bob = make_local(&pool, "bob").await;
        let alice = make_local(&pool, "alice").await;
        let carol = make_local(&pool, "carol").await;
        // alice hides who she follows and who follows her.
        sqlx::query!(
            "UPDATE accounts SET hide_collections = TRUE WHERE id = $1",
            alice
        )
        .execute(&pool)
        .await
        .unwrap();

        // alice then carol follow bob → both are followers of bob; bob follows
        // alice then carol → both are accounts bob follows. Newest-first, so a
        // full list orders carol before alice.
        create(&pool, alice, bob, None).await.unwrap();
        create(&pool, carol, bob, None).await.unwrap();
        create(&pool, bob, alice, None).await.unwrap();
        create(&pool, bob, carol, None).await.unwrap();

        let members =
            |entries: &[FollowListEntry]| entries.iter().map(|e| e.account_id).collect::<Vec<_>>();

        // followers_of — the Mastodon API path.
        for (viewer, expected, note) in [
            (None, vec![carol], "anonymous never sees a hidden follower"),
            (
                Some(carol),
                vec![carol],
                "a third party never sees a hidden follower",
            ),
            (
                Some(alice),
                vec![carol, alice],
                "a hidden member still sees themselves",
            ),
            (
                Some(bob),
                vec![carol, alice],
                "the list owner always sees every follower",
            ),
        ] {
            let got = followers_of(&pool, bob, None, None, 10, viewer)
                .await
                .unwrap();
            assert_eq!(members(&got), expected, "followers_of: {note}");
        }

        // following_of — same rule on the target side of the follow.
        for (viewer, expected, note) in [
            (None, vec![carol], "anonymous never sees a hidden followee"),
            (
                Some(alice),
                vec![carol, alice],
                "a hidden member still sees themselves in following",
            ),
            (
                Some(bob),
                vec![carol, alice],
                "the list owner always sees who they follow",
            ),
        ] {
            let got = following_of(&pool, bob, None, None, 10, viewer)
                .await
                .unwrap();
            assert_eq!(members(&got), expected, "following_of: {note}");
        }

        // The offset-paged variants (web UI + AP collection) apply the same rule.
        assert_eq!(
            followers_page(&pool, bob, 0, 10, None).await.unwrap(),
            [carol],
            "followers_page hides the hidden member for a stranger",
        );
        assert_eq!(
            followers_page(&pool, bob, 0, 10, Some(bob)).await.unwrap(),
            [carol, alice],
            "followers_page shows everyone to the owner",
        );
        assert_eq!(
            following_page(&pool, bob, 0, 10, Some(carol))
                .await
                .unwrap(),
            [carol],
            "following_page hides the hidden member for a third party",
        );
    }

    #[sqlx::test]
    async fn follow_requests_stay_out_of_follower_listings(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        let carol = account::create_local(
            &pool,
            NewLocalAccount {
                username: "carol",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        create_request(
            &pool,
            remote,
            local,
            Some("https://remote.example/follows/1"),
        )
        .await
        .unwrap();
        create_request(&pool, carol.id, local, None).await.unwrap();

        // Requests are invisible everywhere followers appear.
        assert_eq!(count_followers(&pool, local).await.unwrap(), 0);
        assert!(
            followers_of(&pool, local, None, None, 10, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(follower_inboxes(&pool, local).await.unwrap().is_empty());

        // …but list and count as requests, newest first, capped.
        let requests = requests_of(&pool, local, None, None, 10).await.unwrap();
        assert_eq!(
            requests.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [carol.id, remote]
        );
        assert_eq!(count_requests(&pool, local, 40).await.unwrap(), 2);
        assert_eq!(count_requests(&pool, local, 1).await.unwrap(), 1);

        // A repeat request refreshes the URI and stays pending.
        create_request(
            &pool,
            remote,
            local,
            Some("https://remote.example/follows/2"),
        )
        .await
        .unwrap();
        let edge = find(&pool, remote, local).await.unwrap().unwrap();
        assert!(edge.pending);
        assert_eq!(
            edge.uri.as_deref(),
            Some("https://remote.example/follows/2")
        );

        // Accepting turns the request into a follower.
        assert!(mark_accepted(&pool, remote, local).await.unwrap());
        assert_eq!(count_requests(&pool, local, 40).await.unwrap(), 1);
        assert_eq!(count_followers(&pool, local).await.unwrap(), 1);
        let followers = followers_of(&pool, local, None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(followers[0].account_id, remote);
    }

    #[sqlx::test]
    async fn sent_requests_list_only_own_outgoing_pending(pool: PgPool) {
        // alice sends outgoing follows to bob (remote) and carol (local); dave
        // sends a request *to* alice (incoming, must not appear in alice's
        // sent list); alice already-accepted-follows erin (must not appear).
        let (alice, bob) = local_and_remote(&pool).await;
        let carol = make_local(&pool, "carol").await;
        let dave = make_local(&pool, "dave").await;
        let erin = make_local(&pool, "erin").await;

        let to_bob = create_outgoing(&pool, alice, bob, "uri-bob").await.unwrap();
        let to_carol = create_outgoing(&pool, alice, carol, "uri-carol")
            .await
            .unwrap();
        create_request(&pool, dave, alice, None).await.unwrap();
        create(&pool, alice, erin, None).await.unwrap();

        // Newest sent request first; only alice's own outgoing pending edges.
        let sent = sent_requests_of(&pool, alice, None, None, 10)
            .await
            .unwrap();
        assert_eq!(
            sent.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [carol, bob],
            "own outgoing pending only, newest first"
        );
        assert_eq!(count_sent_requests(&pool, alice, 40).await.unwrap(), 2);
        assert_eq!(count_sent_requests(&pool, alice, 1).await.unwrap(), 1);

        // The incoming request to alice is hers to *receive*, not to send.
        let received = requests_of(&pool, alice, None, None, 10).await.unwrap();
        assert_eq!(
            received.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [dave]
        );

        // Keyset bounds page by follow row id, like the other listings.
        let older = sent_requests_of(&pool, alice, Some(to_carol), None, 10)
            .await
            .unwrap();
        assert_eq!(
            older.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [bob]
        );
        let newer = sent_requests_of(&pool, alice, None, Some(to_bob), 10)
            .await
            .unwrap();
        assert_eq!(
            newer.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [carol]
        );

        // Accepting a sent request clears it from the list.
        assert!(mark_accepted(&pool, alice, bob).await.unwrap());
        let sent = sent_requests_of(&pool, alice, None, None, 10)
            .await
            .unwrap();
        assert_eq!(
            sent.iter().map(|e| e.account_id).collect::<Vec<_>>(),
            [carol]
        );
        assert_eq!(count_sent_requests(&pool, alice, 40).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn follower_inboxes_prefer_shared_and_skip_pending_and_local(pool: PgPool) {
        let (local, remote) = local_and_remote(&pool).await;
        // remote (with personal inbox only) follows local.
        create(&pool, remote, local, None).await.unwrap();
        // A second remote with a shared inbox follows local too.
        let carol = account::upsert_remote(
            &pool,
            RemoteAccountData {
                username: "carol",
                domain: "other.example",
                uri: "https://other.example/users/carol",
                display_name: "",
                note: "",
                inbox_url: "https://other.example/users/carol/inbox",
                shared_inbox_url: "https://other.example/inbox",
                public_key_pem: "pub",
                public_key_id: "https://other.example/users/carol#main-key",
                avatar_remote_url: None,
                header_remote_url: None,
                avatar_description: "",
                header_description: "",
                created_at: None,
                fields: Vec::new(),
                featured_collection_url: None,
                locked: false,
                also_known_as: &[],
                moved_to_uri: None,
                url: None,
                discoverable: false,
                feature_approval_policy: 0,
                is_bot: false,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                actor_type: None,
            },
        )
        .await
        .unwrap();
        create(&pool, carol.id, local, None).await.unwrap();
        // A pending outgoing follow in the other direction must not appear.
        create_outgoing(&pool, local, remote, "uri").await.unwrap();

        let mut inboxes = follower_inboxes(&pool, local).await.unwrap();
        inboxes.sort();
        assert_eq!(
            inboxes,
            [
                "https://other.example/inbox",
                "https://remote.example/users/bob/inbox",
            ]
        );
        assert!(follower_inboxes(&pool, remote).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn mutual_page_lists_only_reciprocated_follows(pool: PgPool) {
        let alice = make_local(&pool, "alice").await;
        let bob = make_local(&pool, "bob").await;
        let carol = make_local(&pool, "carol").await;
        let dave = make_local(&pool, "dave").await;

        // alice ⇄ bob (mutual), alice → carol (one-way), dave → alice (one-way).
        create(&pool, alice, bob, None).await.unwrap();
        create(&pool, bob, alice, None).await.unwrap();
        create(&pool, alice, carol, None).await.unwrap();
        create(&pool, dave, alice, None).await.unwrap();
        // A pending reciprocal follow does not make bob→alice count if it were
        // pending; here also add a pending one-way that must be excluded.
        create_request(&pool, alice, dave, None).await.unwrap();

        let mutual = mutual_page(&pool, alice, 0, 10).await.unwrap();
        // Only bob is followed-back and accepted both directions.
        assert_eq!(mutual, vec![bob]);
    }

    /// The `with_replies` default depends on who was followed: a
    /// person starts on, a community or a bot starts off, and an actor whose
    /// `actor_type` we never recognised reads as a person rather than as
    /// "unknown, so filter" — that state is a real share of the follow set.
    #[sqlx::test]
    async fn with_replies_default_depends_on_the_target(pool: PgPool) {
        let alice = make_local(&pool, "alice").await;
        let person = make_local(&pool, "person").await;
        let community = make_local(&pool, "community").await;
        let bot = make_local(&pool, "bot").await;
        let unknown = make_local(&pool, "unknown").await;
        let bot_community = make_local(&pool, "botcommunity").await;

        let set_kind = |id: i64, actor_type: Option<&'static str>, is_bot: bool| {
            let pool = pool.clone();
            async move {
                sqlx::query!(
                    "UPDATE accounts SET actor_type = $2, is_bot = $3 WHERE id = $1",
                    id,
                    actor_type,
                    is_bot,
                )
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        set_kind(person, Some("Person"), false).await;
        set_kind(community, Some("Group"), false).await;
        set_kind(bot, Some("Service"), true).await;
        set_kind(unknown, None, false).await;
        set_kind(bot_community, Some("Group"), true).await;

        for target in [person, community, bot, unknown, bot_community] {
            create(&pool, alice, target, None).await.unwrap();
        }
        let flag = |target: i64| {
            let pool = pool.clone();
            async move {
                find(&pool, alice, target)
                    .await
                    .unwrap()
                    .unwrap()
                    .with_replies
            }
        };
        assert!(flag(person).await, "a person's replies show by default");
        assert!(!flag(community).await, "a community's do not");
        assert!(!flag(bot).await, "a bot's do not");
        assert!(
            flag(unknown).await,
            "an unrecognised actor_type reads as a person"
        );
        assert!(!flag(bot_community).await, "the rule is an OR, not a match");

        // A repeated Follow must not reset a value the user has since changed:
        // `insert` is an upsert whose DO UPDATE touches only uri/pending.
        assert!(
            update_settings(&pool, alice, person, None, Some(false), None, None)
                .await
                .unwrap()
        );
        create(&pool, alice, person, Some("https://example.test/follows/1"))
            .await
            .unwrap();
        assert!(!flag(person).await, "a re-Follow keeps the chosen value");

        // An account that turns into a bot after the follow keeps the flag it
        // had — the default is applied once, at follow time.
        set_kind(unknown, None, true).await;
        assert!(flag(unknown).await);
    }

    /// Migration 0032 backfills the same rule onto follows that predate it,
    /// so a community followed before the deploy behaves like one followed
    /// after. The statement below is copied from
    /// `0032_follows_with_replies.sql` and must stay identical to it — the
    /// trigger and the backfill are two implementations of one rule and only
    /// the trigger is exercised by new follows.
    #[sqlx::test]
    async fn migration_backfill_flips_pre_existing_group_and_bot_follows(pool: PgPool) {
        let alice = make_local(&pool, "alice").await;
        let person = make_local(&pool, "person").await;
        let community = make_local(&pool, "community").await;
        let bot = make_local(&pool, "bot").await;
        sqlx::query!(
            "UPDATE accounts SET actor_type = 'Group' WHERE id = $1",
            community
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!("UPDATE accounts SET is_bot = TRUE WHERE id = $1", bot)
            .execute(&pool)
            .await
            .unwrap();
        for target in [person, community, bot] {
            create(&pool, alice, target, None).await.unwrap();
        }
        // Pre-migration state: every follow carried the column default.
        sqlx::query!("UPDATE follows SET with_replies = TRUE")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query!(
            r#"
            UPDATE follows f
            SET with_replies = false
            FROM accounts a
            WHERE a.id = f.target_account_id
              AND (a.actor_type = 'Group' OR a.is_bot)
            "#
        )
        .execute(&pool)
        .await
        .unwrap();

        let flag = |target: i64| {
            let pool = pool.clone();
            async move {
                find(&pool, alice, target)
                    .await
                    .unwrap()
                    .unwrap()
                    .with_replies
            }
        };
        assert!(flag(person).await);
        assert!(!flag(community).await);
        assert!(!flag(bot).await);
    }
}
