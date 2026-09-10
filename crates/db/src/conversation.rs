//! Direct-message conversations, mirroring Mastodon's model: a
//! `conversations` row groups a direct thread, `status_conversations` maps
//! each direct status into one, and every local participant gets an
//! `account_conversations` row carrying the statuses they may see, the other
//! participants, and their read state.

use sqlx::{PgExecutor, PgPool};

use crate::{DbError, id};

/// One local account's view of a conversation (Mastodon's
/// `AccountConversation`). `last_status_id` is the newest entry of
/// `status_ids` and drives pagination ordering.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccountConversation {
    pub id: i64,
    pub account_id: i64,
    pub conversation_id: i64,
    /// The other participants (author + mentioned, minus this account),
    /// sorted — part of the row's identity, like Mastodon.
    pub participant_account_ids: Vec<i64>,
    pub status_ids: Vec<i64>,
    pub last_status_id: Option<i64>,
    pub unread: bool,
}

/// A conversation's owner + identity. `uri` NULL means locally owned
/// (its context URL is minted from the id); otherwise it is the remote
/// `context` collection IRI the thread converges on.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Conversation {
    pub id: i64,
    pub owner_account_id: Option<i64>,
    pub root_status_id: Option<i64>,
    pub uri: Option<String>,
    pub history_uri: Option<String>,
}

/// The conversation a status belongs to, if recorded. Executor-generic so it
/// can run inside the status-creation transaction.
pub async fn of_status<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let conversation_id = sqlx::query_scalar!(
        "SELECT conversation_id FROM status_conversations WHERE status_id = $1",
        status_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(conversation_id)
}

/// Full conversation identity for serving/backfill.
pub async fn find(pool: &PgPool, id: i64) -> Result<Option<Conversation>, DbError> {
    let row = sqlx::query_as!(
        Conversation,
        "SELECT id, owner_account_id, root_status_id, uri, history_uri
         FROM conversations WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The conversation a remote `context`/`conversation` IRI resolves to, if we
/// have already threaded any post from it. Executor-generic so it can run inside
/// the status-creation transaction.
pub async fn find_by_uri<'e, E: PgExecutor<'e>>(
    executor: E,
    uri: &str,
) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!("SELECT id FROM conversations WHERE uri = $1", uri)
        .fetch_optional(executor)
        .await?;
    Ok(id)
}

/// A status' conversation identity, for advertising its `context` on the
/// wire. `uri` is the remote posts-collection IRI when the conversation is
/// remote-owned; `None` means locally owned and its context URL is minted from
/// `conversation_id`. `root_visibility` is the top-level post's visibility (so
/// callers only advertise a locally-owned collection whose root is
/// distributable) — `None` when the root is not yet known.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StatusContext {
    pub conversation_id: i64,
    pub uri: Option<String>,
    pub history_uri: Option<String>,
    pub root_visibility: Option<String>,
}

/// The conversation identity a status advertises. One row, joined so wire
/// serialization stays a single query per Note.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn context_of_status<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Option<StatusContext>, DbError> {
    let row = sqlx::query_as!(
        StatusContext,
        r#"
        SELECT c.id AS conversation_id, c.uri, c.history_uri,
               root.visibility AS "root_visibility?"
        FROM status_conversations sc
        JOIN conversations c ON c.id = sc.conversation_id
        LEFT JOIN statuses root ON root.id = c.root_status_id -- STUBKEEP: a stubbed root still carries the conversation's visibility
        WHERE sc.status_id = $1
        "#,
        status_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(row)
}

/// The conversation identities of a batch of statuses, keyed by status id —
/// the batched form of [`context_of_status`], for the collection builders that
/// serialize a whole page of `Note` documents.
pub async fn contexts_of_statuses<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<std::collections::HashMap<i64, StatusContext>, DbError> {
    if status_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT sc.status_id AS "status_id!", c.id AS conversation_id, c.uri, c.history_uri,
               root.visibility AS "root_visibility?"
        FROM status_conversations sc
        JOIN conversations c ON c.id = sc.conversation_id
        LEFT JOIN statuses root ON root.id = c.root_status_id -- STUBKEEP: a stubbed root still carries the conversation's visibility
        WHERE sc.status_id = ANY($1)
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.status_id,
                StatusContext {
                    conversation_id: row.conversation_id,
                    uri: row.uri,
                    history_uri: row.history_uri,
                    root_visibility: row.root_visibility,
                },
            )
        })
        .collect())
}

/// One post of a conversation's FEP-f228 collection: enough to mint its item
/// IRI (the remote `uri`, or the local AP id from `username` + `status_id`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContextPost {
    pub status_id: i64,
    pub uri: Option<String>,
    pub username: String,
    pub actor_uri: Option<String>,
}

/// A chronological page of a conversation's **distributable** posts (public or
/// unlisted only, boosts excluded), keyset by `status_id > after_id`. Private
/// and direct posts are never listed — the posts collection is public backfill,
/// matching Mastodon; private threads travel by the FEP-171b container.
pub async fn context_page(
    pool: &PgPool,
    conversation_id: i64,
    after_id: i64,
    limit: i64,
) -> Result<Vec<ContextPost>, DbError> {
    let rows = sqlx::query_as!(
        ContextPost,
        r#"
        SELECT s.id AS status_id, s.uri, a.username, a.uri AS actor_uri
        FROM statuses s
        JOIN status_conversations sc ON sc.status_id = s.id
        JOIN accounts a ON a.id = s.account_id
        WHERE sc.conversation_id = $1
          AND s.visibility IN ('public', 'unlisted')
          AND s.reblog_of_id IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.id > $2
        ORDER BY s.id ASC
        LIMIT $3
        "#,
        conversation_id,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The FEP-context IRIs a status carries, read from its AP object at ingest.
/// Both are `None` for a locally-authored status (it has no remote context).
#[derive(Debug, Default, Clone, Copy)]
pub struct ContextRefs<'a> {
    /// The `context` (or legacy `conversation`) collection-of-posts IRI.
    pub context_uri: Option<&'a str>,
    /// The `contextHistory` collection-of-activities IRI (FEP-171b).
    pub history_uri: Option<&'a str>,
}

/// Everything `ensure_for_status` needs to place a status in a conversation
/// and, for a root, stamp the conversation's owner + identity.
#[derive(Debug)]
pub struct EnsureConversation<'a> {
    pub status_id: i64,
    /// The status' author — the owner of a conversation this status roots.
    pub account_id: i64,
    /// The resolved reply-parent's local id, when we have it (drives
    /// inheritance).
    pub in_reply_to_id: Option<i64>,
    /// Whether the object is a reply at all (carried an `inReplyTo`), even if
    /// the parent could not be resolved. An orphaned reply must never be
    /// mistaken for a conversation root.
    pub is_reply: bool,
    pub refs: ContextRefs<'a>,
}

/// Maps a status to its conversation and returns the conversation id.
///
/// Precedence: an existing mapping wins (re-delivery); then a conversation we
/// already know by the object's `context` IRI (converges a thread whose reply
/// parent we never received — the FEP-f228 win); then the reply parent's
/// conversation; then a fresh conversation. Only a genuine top-level post
/// (`is_reply == false`) stamps `owner_account_id`/`root_status_id`; a remote
/// root records the `uri`/`history_uri` so later replies converge on it.
pub async fn ensure_for_status(
    pool: &PgPool,
    params: &EnsureConversation<'_>,
) -> Result<i64, DbError> {
    let mut conn = pool.acquire().await?;
    ensure_for_status_conn(&mut conn, params).await
}

/// [`ensure_for_status`] within a caller-provided transaction, so a fresh local
/// status' conversation mapping commits atomically with the status row. Runs
/// its several statements on one connection, reborrowed per query.
pub async fn ensure_for_status_conn(
    conn: &mut sqlx::PgConnection,
    params: &EnsureConversation<'_>,
) -> Result<i64, DbError> {
    if let Some(existing) = of_status(&mut *conn, params.status_id).await? {
        return Ok(existing);
    }
    // A known remote conversation IRI converges the thread ahead of the reply
    // chain, so a reply whose parent is missing still lands in the right place.
    let by_uri = match params.refs.context_uri {
        Some(uri) => find_by_uri(&mut *conn, uri).await?,
        None => None,
    };
    let inherited = match (by_uri, params.in_reply_to_id) {
        (Some(id), _) => Some(id),
        (None, Some(parent_id)) => of_status(&mut *conn, parent_id).await?,
        (None, None) => None,
    };
    let conversation_id = if let Some(existing) = inherited {
        existing
    } else if let Some(uri) = params.refs.context_uri {
        // Create-or-get keyed on the remote context IRI: a concurrent delivery
        // of the same brand-new conversation must not 23505 on the uri index.
        let fresh = id::next();
        sqlx::query!(
            "INSERT INTO conversations (id, uri, history_uri) VALUES ($1, $2, $3)
             ON CONFLICT (uri) WHERE uri IS NOT NULL DO NOTHING",
            fresh,
            uri,
            params.refs.history_uri,
        )
        .execute(&mut *conn)
        .await?;
        find_by_uri(&mut *conn, uri).await?.unwrap_or(fresh)
    } else {
        let fresh = id::next();
        sqlx::query!("INSERT INTO conversations (id) VALUES ($1)", fresh)
            .execute(&mut *conn)
            .await?;
        fresh
    };
    sqlx::query!(
        "INSERT INTO status_conversations (status_id, conversation_id)
         VALUES ($1, $2) ON CONFLICT (status_id) DO NOTHING",
        params.status_id,
        conversation_id,
    )
    .execute(&mut *conn)
    .await?;
    // Any member may teach us the container (`contextHistory`); only a genuine
    // root (never a reply) claims ownership + the root status — and only into
    // blanks, so re-delivery and orphaned replies change nothing.
    sqlx::query!(
        "UPDATE conversations
         SET history_uri      = COALESCE(history_uri, $2),
             owner_account_id = CASE WHEN $3 THEN owner_account_id
                                     ELSE COALESCE(owner_account_id, $4) END,
             root_status_id   = CASE WHEN $3 THEN root_status_id
                                     ELSE COALESCE(root_status_id, $5) END
         WHERE id = $1",
        conversation_id,
        params.refs.history_uri,
        params.is_reply,
        params.account_id,
        params.status_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(conversation_id)
}

/// Adding a status to one participant's conversation row.
#[derive(Debug)]
pub struct AddStatus<'a> {
    /// The local account whose row this is.
    pub account_id: i64,
    pub conversation_id: i64,
    /// All other participants (author + mentioned, minus `account_id`).
    pub participant_account_ids: &'a [i64],
    pub status_id: i64,
    /// The status author — receiving someone else's message marks unread.
    pub sender_id: i64,
}

/// Records a status in the matching `(account, conversation, participants)`
/// row, creating it if needed. Re-recording the same status is a no-op, so
/// re-delivered activities don't flip read state.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn add_status<'e, E: PgExecutor<'e>>(
    executor: E,
    add: AddStatus<'_>,
) -> Result<(), DbError> {
    let mut participants = add.participant_account_ids.to_vec();
    participants.sort_unstable();
    participants.dedup();
    let unread = add.sender_id != add.account_id;
    sqlx::query!(
        r#"
        INSERT INTO account_conversations
            (id, account_id, conversation_id, participant_account_ids,
             status_ids, last_status_id, unread)
        VALUES ($1, $2, $3, $4, ARRAY[$5::bigint], $5, $6)
        ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE SET
            status_ids = CASE
                WHEN account_conversations.status_ids @> ARRAY[$5::bigint]
                THEN account_conversations.status_ids
                ELSE account_conversations.status_ids || $5::bigint END,
            last_status_id = GREATEST(account_conversations.last_status_id, $5),
            unread = CASE
                WHEN account_conversations.status_ids @> ARRAY[$5::bigint]
                THEN account_conversations.unread
                ELSE $6 END
        "#,
        id::next(),
        add.account_id,
        add.conversation_id,
        &participants,
        add.status_id,
        unread,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Folds the placeholder conversations a batch of orphaned replies minted for
/// themselves into the conversation of the parent they have just been joined
/// to (`status::adopt_orphan_replies`).
///
/// A reply whose parent had not arrived could not inherit a conversation, so —
/// unless it named a `context` IRI we already knew — [`ensure_for_status`] opened
/// an anonymous one, and its own replies inherited *that*. Linking the reply
/// repairs the reply tree; this repairs the flat/context view, which groups by
/// conversation rather than by reply edge. One statement moves each reply and
/// every descendant that inherited its placeholder, because
/// `status_conversations` is keyed by status.
///
/// Conservative by design: only a *placeholder* is absorbed — no `uri` (nothing
/// converges on it by IRI), no `owner_account_id`/`root_status_id` (nothing claims
/// it as a thread of its own), and no DM inbox rows, whose per-participant status
/// arrays and read state are not ours to reshuffle. Everything else is left
/// standing: a peer that names two different contexts is telling us these are two
/// conversations, and the reply edge is repaired either way. A reply with no
/// mapping at all (it predates conversation bookkeeping, or its ingest failed
/// midway) is put where its parent lives.
///
/// Thread mutes ride along, ignoring collisions. A mute may end up covering more
/// than the muter picked when two threads turn out to be one, but it must never
/// be silently lifted.
///
/// The whole fold is a bounded number of statements however many replies were
/// waiting — the count is remote-controlled (a peer can deliver N orphans
/// before their parent), so a per-reply loop here was a remotely swellable
/// cost on the ingest connection.
pub async fn absorb_placeholders(
    conn: &mut sqlx::PgConnection,
    reply_status_ids: &[i64],
    parent_status_id: i64,
) -> Result<(), DbError> {
    if reply_status_ids.is_empty() {
        return Ok(());
    }
    let Some(target) = of_status(&mut *conn, parent_status_id).await? else {
        return Ok(());
    };
    let mapped = sqlx::query!(
        "SELECT status_id, conversation_id FROM status_conversations WHERE status_id = ANY($1)",
        reply_status_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    let unmapped: Vec<i64> = reply_status_ids
        .iter()
        .copied()
        .filter(|id| !mapped.iter().any(|row| row.status_id == *id))
        .collect();
    if !unmapped.is_empty() {
        sqlx::query!(
            "INSERT INTO status_conversations (status_id, conversation_id)
             SELECT unnest($1::bigint[]), $2
             ON CONFLICT (status_id) DO NOTHING",
            &unmapped,
            target,
        )
        .execute(&mut *conn)
        .await?;
    }
    // Distinct source conversations that are not already the target (already
    // converged by context IRI, FEP-f228).
    let mut sources: Vec<i64> = mapped
        .iter()
        .map(|row| row.conversation_id)
        .filter(|&conversation| conversation != target)
        .collect();
    sources.sort_unstable();
    sources.dedup();
    if sources.is_empty() {
        return Ok(());
    }
    let placeholders = sqlx::query_scalar!(
        r#"SELECT c.id
           FROM conversations c
           WHERE c.id = ANY($1)
             AND c.uri IS NULL
             AND c.owner_account_id IS NULL
             AND c.root_status_id IS NULL
             AND NOT EXISTS (SELECT 1 FROM account_conversations ac
                             WHERE ac.conversation_id = c.id)"#,
        &sources,
    )
    .fetch_all(&mut *conn)
    .await?;
    if placeholders.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE status_conversations SET conversation_id = $2 WHERE conversation_id = ANY($1)",
        &placeholders,
        target,
    )
    .execute(&mut *conn)
    .await?;
    // Carried before the source rows go, which would cascade them away.
    let muters = sqlx::query_scalar!(
        "SELECT DISTINCT account_id FROM conversation_mutes WHERE conversation_id = ANY($1)",
        &placeholders,
    )
    .fetch_all(&mut *conn)
    .await?;
    if !muters.is_empty() {
        let mute_ids: Vec<i64> = muters.iter().map(|_| id::next()).collect();
        sqlx::query!(
            "INSERT INTO conversation_mutes (id, account_id, conversation_id)
             SELECT v.id, v.account_id, $3
             FROM unnest($1::bigint[], $2::bigint[]) AS v(id, account_id)
             ON CONFLICT (account_id, conversation_id) DO NOTHING",
            &mute_ids,
            &muters,
            target,
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!(
        "DELETE FROM conversations WHERE id = ANY($1)",
        &placeholders,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Removes a deleted status from every participant row of its conversation;
/// rows left without statuses disappear (Mastodon's `remove_status`).
pub async fn remove_status(
    pool: &PgPool,
    conversation_id: i64,
    status_id: i64,
) -> Result<(), DbError> {
    let mut conn = pool.begin().await?;
    remove_status_conn(&mut conn, conversation_id, status_id).await?;
    conn.commit().await?;
    Ok(())
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn remove_status_conn(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE account_conversations
        SET status_ids = array_remove(status_ids, $2),
            last_status_id = (SELECT max(remaining)
                              FROM unnest(array_remove(status_ids, $2)) AS remaining)
        WHERE conversation_id = $1 AND status_ids @> ARRAY[$2::bigint]
        "#,
        conversation_id,
        status_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM account_conversations
         WHERE conversation_id = $1 AND cardinality(status_ids) = 0",
        conversation_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drops all of `account_id`'s conversation rows that `participant_id` takes
/// part in — blocking someone removes the shared DM threads, like Mastodon's
/// `AfterBlockService`.
/// Executor-generic so a block's conversation cleanup commits with the block
/// row and its outbox job.
pub async fn remove_with_participant<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    participant_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM account_conversations
         WHERE account_id = $1 AND participant_account_ids @> ARRAY[$2::bigint]",
        account_id,
        participant_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Batched [`remove_with_participant`] across owners: one statement drops the
/// participant's threads from every listed account — the `Move` replay's
/// carried-block cleanup over the whole blocker set.
pub async fn remove_with_participant_many<'e, E: PgExecutor<'e>>(
    executor: E,
    account_ids: &[i64],
    participant_id: i64,
) -> Result<(), DbError> {
    if account_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM account_conversations
         WHERE account_id = ANY($1) AND participant_account_ids @> ARRAY[$2::bigint]",
        account_ids,
        participant_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// An account's conversations, newest message first. `max_id`/`since_id`/
/// `min_id` compare `last_status_id`, exactly like Mastodon's pagination.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<AccountConversation>, DbError> {
    if let Some(min_id) = min_id {
        // The page of oldest-first results just above min_id, presented
        // newest-first like every other page.
        let mut rows = sqlx::query_as!(
            AccountConversation,
            r#"
            SELECT id, account_id, conversation_id, participant_account_ids,
                   status_ids, last_status_id, unread
            FROM account_conversations
            WHERE account_id = $1 AND last_status_id > $2
              AND ($3::bigint IS NULL OR last_status_id < $3)
            ORDER BY last_status_id ASC
            LIMIT $4
            "#,
            account_id,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        AccountConversation,
        r#"
        SELECT id, account_id, conversation_id, participant_account_ids,
               status_ids, last_status_id, unread
        FROM account_conversations
        WHERE account_id = $1
          AND ($2::bigint IS NULL OR last_status_id < $2)
          AND ($3::bigint IS NULL OR last_status_id > $3)
        ORDER BY last_status_id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One conversation row, scoped to its owner.
pub async fn find_for(
    pool: &PgPool,
    account_id: i64,
    row_id: i64,
) -> Result<Option<AccountConversation>, DbError> {
    let row = sqlx::query_as!(
        AccountConversation,
        r#"
        SELECT id, account_id, conversation_id, participant_account_ids,
               status_ids, last_status_id, unread
        FROM account_conversations
        WHERE id = $1 AND account_id = $2
        "#,
        row_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Sets a conversation row's read state, returning the updated row
/// (owner-scoped). `POST /conversations/{id}/read` passes `false`,
/// `/unread` passes `true`.
pub async fn set_unread(
    pool: &PgPool,
    account_id: i64,
    row_id: i64,
    unread: bool,
) -> Result<Option<AccountConversation>, DbError> {
    let row = sqlx::query_as!(
        AccountConversation,
        r#"
        UPDATE account_conversations SET unread = $3
        WHERE id = $1 AND account_id = $2
        RETURNING id, account_id, conversation_id, participant_account_ids,
                  status_ids, last_status_id, unread
        "#,
        row_id,
        account_id,
        unread,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Marks a conversation read, returning the updated row (owner-scoped).
/// Whether the account holds any unread conversation — the web client's nav
/// dot, probed on every page load (served by the partial
/// `account_conversations_unread_idx`). Muted threads are excluded: they keep
/// accumulating unread rows (`add_status` doesn't consult mutes), but a thread
/// the user silenced must not relight the dot or muting is self-defeating.
pub async fn has_unread(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let unread = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM account_conversations ac
            WHERE ac.account_id = $1
              AND ac.unread
              AND NOT EXISTS (
                  SELECT 1 FROM conversation_mutes cm
                  WHERE cm.account_id = $1
                    AND cm.conversation_id = ac.conversation_id)
        ) AS "unread!"
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(unread)
}

/// Marks every one of the account's rows for a conversation read — the thread
/// page's open-side-effect. Conversation-keyed because one account can hold
/// several rows per conversation (the participant set forked mid-thread);
/// the row-keyed [`set_unread`] serves the API verbs instead.
pub async fn mark_read_conversation(
    pool: &PgPool,
    account_id: i64,
    conversation_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE account_conversations SET unread = FALSE
         WHERE account_id = $1 AND conversation_id = $2 AND unread",
        account_id,
        conversation_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_read(
    pool: &PgPool,
    account_id: i64,
    row_id: i64,
) -> Result<Option<AccountConversation>, DbError> {
    set_unread(pool, account_id, row_id, false).await
}

/// Deletes one of `account_id`'s conversation rows (like Mastodon's
/// conversation-removal endpoint, which likewise drops only that account's
/// copy of the conversation); returns whether it existed.
pub async fn delete_account_conversation(
    pool: &PgPool,
    account_id: i64,
    row_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM account_conversations WHERE id = $1 AND account_id = $2",
        row_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Mutes a thread for `account_id` (Mastodon's `mute_conversation!`).
/// Idempotent — re-muting an already-muted conversation is a no-op.
pub async fn mute(pool: &PgPool, account_id: i64, conversation_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO conversation_mutes (id, account_id, conversation_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, conversation_id) DO NOTHING
        "#,
        id::next(),
        account_id,
        conversation_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Unmutes a thread; returns whether a mute existed.
pub async fn unmute(pool: &PgPool, account_id: i64, conversation_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM conversation_mutes WHERE account_id = $1 AND conversation_id = $2",
        account_id,
        conversation_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Batched [`status_muted`] across accounts: of `account_ids`, the ones muting
/// the thread `status_id` belongs to — the notification drop evaluated for a
/// whole recipient list in one statement.
pub async fn status_muted_of(
    pool: &PgPool,
    account_ids: &[i64],
    status_id: i64,
) -> Result<Vec<i64>, DbError> {
    if account_ids.is_empty() {
        return Ok(Vec::new());
    }
    let muted = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT cm.account_id AS "account_id!"
        FROM status_conversations sc
        JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
        WHERE sc.status_id = $1 AND cm.account_id = ANY($2)
        "#,
        status_id,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(muted)
}

/// Whether `account_id` mutes the thread `status_id` belongs to — drives the
/// status entity's `muted` flag and the notification drop. A status with no
/// recorded conversation (e.g. a reblog) is never muted.
pub async fn status_muted(pool: &PgPool, account_id: i64, status_id: i64) -> Result<bool, DbError> {
    let muted = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM status_conversations sc
            JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
            WHERE sc.status_id = $1 AND cm.account_id = $2
        ) AS "muted!"
        "#,
        status_id,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(muted)
}

/// The subset of `status_ids` whose conversation `account_id` mutes — the
/// batch form for rendering `muted` across a page of statuses.
pub async fn muted_status_ids(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT sc.status_id AS "status_id!"
        FROM status_conversations sc
        JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
        WHERE cm.account_id = $1 AND sc.status_id = ANY($2)
        "#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// [`muted_status_ids`] across a set of viewers in one query — `(viewer,
/// status)` pairs where the viewer mutes the status' conversation.
pub async fn muted_status_ids_for_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT cm.account_id, sc.status_id
        FROM status_conversations sc
        JOIN conversation_mutes cm ON cm.conversation_id = sc.conversation_id
        WHERE cm.account_id = ANY($1) AND sc.status_id = ANY($2)
        "#,
        viewer_ids,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.status_id))
        .collect())
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewRemoteStatus};

    async fn local_account(pool: &PgPool, name: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: name,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    fn remote(account_id: i64, uri: &str) -> NewRemoteStatus<'_> {
        NewRemoteStatus {
            uri,
            account_id,
            content: "hi",
            created_at: OffsetDateTime::UNIX_EPOCH,
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
            title: None,
            object_type: None,
            external_url: None,
        }
    }

    /// A local root owns a fresh conversation; its self-reply inherits it.
    #[sqlx::test]
    async fn local_root_owns_conversation(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>root</p>", "public", None),
        )
        .await
        .unwrap();
        let conv = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: root.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: false,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();

        let stored = find(&pool, conv).await.unwrap().unwrap();
        assert_eq!(stored.owner_account_id, Some(alice));
        assert_eq!(stored.root_status_id, Some(root.id));
        assert_eq!(stored.uri, None, "locally owned → no remote uri");

        let reply = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice, "<p>self</p>", "public", Some(root.id)),
        )
        .await
        .unwrap();
        let reply_conv = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: reply.id,
                account_id: alice,
                in_reply_to_id: Some(root.id),
                is_reply: true,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(reply_conv, conv, "reply inherits the root's conversation");
        // The reply never reassigns ownership.
        let stored = find(&pool, conv).await.unwrap().unwrap();
        assert_eq!(stored.root_status_id, Some(root.id));
    }

    /// A remote reply whose parent we never received still converges on the
    /// conversation named by its shared `context` IRI — the FEP-f228 win.
    #[sqlx::test]
    async fn remote_context_converges_without_parent(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let ctx = "https://remote.example/contexts/9";

        // First the reply arrives (parent unresolved, so in_reply_to_id=None),
        // carrying the context IRI.
        let reply =
            status::upsert_remote(&pool, remote(alice, "https://remote.example/posts/reply"))
                .await
                .unwrap();
        let a = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: reply.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: true, // orphaned reply — parent not yet received
                refs: ContextRefs {
                    context_uri: Some(ctx),
                    history_uri: None,
                },
            },
        )
        .await
        .unwrap();

        // Then the actual root arrives (also None reply, same context).
        let root = status::upsert_remote(&pool, remote(alice, "https://remote.example/posts/root"))
            .await
            .unwrap();
        let b = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: root.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: false, // the genuine top-level post
                refs: ContextRefs {
                    context_uri: Some(ctx),
                    history_uri: Some("https://remote.example/contexts/9/history"),
                },
            },
        )
        .await
        .unwrap();

        assert_eq!(a, b, "both posts share the context-keyed conversation");
        assert_eq!(find_by_uri(&pool, ctx).await.unwrap(), Some(a));
        let stored = find(&pool, a).await.unwrap().unwrap();
        assert_eq!(stored.uri.as_deref(), Some(ctx), "remote uri recorded");
        assert_eq!(
            stored.history_uri.as_deref(),
            Some("https://remote.example/contexts/9/history"),
            "contextHistory recorded once the root taught it",
        );
        assert_eq!(
            stored.root_status_id,
            Some(root.id),
            "the non-reply root claims root_status_id",
        );
    }

    /// A reply that minted its own placeholder conversation (no `context` IRI to
    /// converge on) is folded into its parent's when the parent finally arrives —
    /// carrying the subtree that inherited the placeholder, and its mute.
    #[sqlx::test]
    async fn placeholder_conversation_absorbed_on_adoption(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let root_uri = "https://remote.example/posts/root";

        // The reply arrives first: no parent, no context IRI, so it opens an
        // anonymous conversation of its own.
        let reply = status::upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(root_uri),
                ..remote(alice, "https://remote.example/posts/reply")
            },
        )
        .await
        .unwrap();
        let placeholder = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: reply.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: true,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        // Its own reply inherits the placeholder.
        let under = status::upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_id: Some(reply.id),
                ..remote(alice, "https://remote.example/posts/under")
            },
        )
        .await
        .unwrap();
        let inherited = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: under.id,
                account_id: alice,
                in_reply_to_id: Some(reply.id),
                is_reply: true,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(inherited, placeholder);
        mute(&pool, alice, placeholder).await.unwrap();

        // The root arrives: stored, placed in its own conversation, and only then
        // does the adoption run — the ingest order, which is what lets the fold
        // see a conversation to fold *into*.
        let root = status::upsert_remote(&pool, remote(alice, root_uri))
            .await
            .unwrap();
        let real = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: root.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: false,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let adopted = status::adopt_orphan_replies(&mut conn, root.id, root_uri)
            .await
            .unwrap();
        drop(conn);
        assert_eq!(adopted, [reply.id]);

        assert_eq!(of_status(&pool, reply.id).await.unwrap(), Some(real));
        assert_eq!(
            of_status(&pool, under.id).await.unwrap(),
            Some(real),
            "the subtree that inherited the placeholder comes along"
        );
        assert!(
            find(&pool, placeholder).await.unwrap().is_none(),
            "the emptied placeholder is gone"
        );
        assert!(
            status_muted(&pool, alice, reply.id).await.unwrap(),
            "a thread mute must never be silently lifted by the fold"
        );
    }

    /// Many orphans waiting on one parent fold in a single bounded batch:
    /// distinct placeholders all converge, their mutes all carry, and an
    /// orphan with no conversation mapping at all lands where the parent
    /// lives.
    #[sqlx::test]
    async fn many_placeholders_fold_in_a_bounded_batch(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let root_uri = "https://remote.example/posts/root";

        let mut orphans = Vec::new();
        let mut placeholders = Vec::new();
        for (n, muter) in [(1, alice), (2, bob)] {
            let orphan = status::upsert_remote(
                &pool,
                NewRemoteStatus {
                    in_reply_to_uri: Some(root_uri),
                    ..remote(alice, &format!("https://remote.example/posts/orphan{n}"))
                },
            )
            .await
            .unwrap();
            let placeholder = ensure_for_status(
                &pool,
                &EnsureConversation {
                    status_id: orphan.id,
                    account_id: alice,
                    in_reply_to_id: None,
                    is_reply: true,
                    refs: ContextRefs::default(),
                },
            )
            .await
            .unwrap();
            mute(&pool, muter, placeholder).await.unwrap();
            orphans.push(orphan.id);
            placeholders.push(placeholder);
        }
        // A third orphan that never got conversation bookkeeping.
        let bare = status::upsert_remote(
            &pool,
            NewRemoteStatus {
                in_reply_to_uri: Some(root_uri),
                ..remote(alice, "https://remote.example/posts/orphan3")
            },
        )
        .await
        .unwrap();
        orphans.push(bare.id);

        let root = status::upsert_remote(&pool, remote(alice, root_uri))
            .await
            .unwrap();
        let real = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: root.id,
                account_id: alice,
                in_reply_to_id: None,
                is_reply: false,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut adopted = status::adopt_orphan_replies(&mut conn, root.id, root_uri)
            .await
            .unwrap();
        drop(conn);
        adopted.sort_unstable();
        let mut expected = orphans.clone();
        expected.sort_unstable();
        assert_eq!(adopted, expected);
        for orphan in &orphans {
            assert_eq!(of_status(&pool, *orphan).await.unwrap(), Some(real));
        }
        for placeholder in &placeholders {
            assert!(
                find(&pool, *placeholder).await.unwrap().is_none(),
                "every emptied placeholder is gone"
            );
        }
        for muter in [alice, bob] {
            assert!(
                status_muted(&pool, muter, root.id).await.unwrap(),
                "every placeholder's mutes carry to the folded conversation"
            );
        }
    }

    /// The nav-dot probe and the conversation-keyed read sweep: unread rows
    /// light the dot unless the thread is muted; opening the thread clears
    /// every forked row of the conversation.
    #[sqlx::test]
    async fn unread_probe_and_conversation_read_sweep(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let dm = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob, "<p>psst</p>", "direct", None),
        )
        .await
        .unwrap();
        let conv = ensure_for_status(
            &pool,
            &EnsureConversation {
                status_id: dm.id,
                account_id: bob,
                in_reply_to_id: None,
                is_reply: false,
                refs: ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        assert!(!has_unread(&pool, alice).await.unwrap());
        add_status(
            &pool,
            AddStatus {
                account_id: alice,
                conversation_id: conv,
                participant_account_ids: &[bob],
                status_id: dm.id,
                sender_id: bob,
            },
        )
        .await
        .unwrap();
        assert!(has_unread(&pool, alice).await.unwrap());

        // Muting silences the dot without touching the rows.
        mute(&pool, alice, conv).await.unwrap();
        assert!(!has_unread(&pool, alice).await.unwrap());
        assert!(unmute(&pool, alice, conv).await.unwrap());
        assert!(has_unread(&pool, alice).await.unwrap());

        // A forked participant set holds a second row for the same
        // conversation; the conversation-keyed sweep clears both.
        let reply = status::create_local(
            &pool,
            status::NewLocalStatus::new(bob, "<p>more</p>", "direct", Some(dm.id)),
        )
        .await
        .unwrap();
        add_status(
            &pool,
            AddStatus {
                account_id: alice,
                conversation_id: conv,
                participant_account_ids: &[],
                status_id: reply.id,
                sender_id: bob,
            },
        )
        .await
        .unwrap();
        mark_read_conversation(&pool, alice, conv).await.unwrap();
        assert!(!has_unread(&pool, alice).await.unwrap());
        let rows = list(&pool, alice, None, None, None, 10).await.unwrap();
        assert_eq!(rows.len(), 2, "forked participant sets keep two rows");
        assert!(rows.iter().all(|r| !r.unread));
    }
}
