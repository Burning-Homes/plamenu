//! Set-based carriers for the account-migration replay — the
//! `mute::carry_over` mould, scoped to `Move` processing.
//!
//! The per-edge machine (`follow_account` / `block_account`) is deliberately
//! untouched; these queries fold the exact skip rules it checks into one
//! eligibility read per direction, and the writes land as set statements. The
//! per-edge outbound activities (each follower's `Follow`/`Undo` is a distinct
//! payload signed by that follower) ride `job::enqueue_batch_tx`.

use sqlx::{PgExecutor, PgPool};

use crate::{DbError, id};

/// One accepted local follower of the moving account, with the facts the
/// replay needs: the stored edge URI (for the `Undo(Follow)` toward the old
/// account) and whether the follower passes every `follow_account` gate
/// toward the new one.
#[derive(Debug)]
pub struct FollowerCarry {
    pub follower_id: i64,
    pub username: String,
    pub actor_uri: Option<String>,
    /// The F→source edge's activity URI; `None` federates no `Undo`.
    pub edge_uri: Option<String>,
    /// Passes the re-follow gates: not the target itself, no block in either
    /// direction, no user-level domain block against the target's domain, and
    /// no existing edge toward the target.
    pub eligible: bool,
}

/// The accepted local followers of `source`, each with its re-follow
/// eligibility toward `target`, in one query. Mirrors
/// `follow::local_follower_ids` (accepted only, ordered by follower id) plus
/// the gates `follow_account` checks per edge. `target_domain` is `None` for
/// a local target, which vacuously passes the domain-block gate — exactly as
/// `account_domain_block::exists` never fires for a domainless target.
pub async fn follower_carries(
    pool: &PgPool,
    source_id: i64,
    target_id: i64,
    target_domain: Option<&str>,
) -> Result<Vec<FollowerCarry>, DbError> {
    let rows = sqlx::query_as!(
        FollowerCarry,
        r#"
        SELECT f.account_id AS "follower_id!", a.username AS "username!",
               a.uri AS actor_uri,
               f.uri AS edge_uri,
               (f.account_id <> $2
                AND NOT EXISTS (SELECT 1 FROM blocks
                                WHERE account_id = f.account_id AND target_account_id = $2)
                AND NOT EXISTS (SELECT 1 FROM blocks
                                WHERE account_id = $2 AND target_account_id = f.account_id)
                AND NOT ($3::text IS NOT NULL AND EXISTS (
                        SELECT 1 FROM account_domain_blocks
                        WHERE account_id = f.account_id AND domain = $3))
                AND NOT EXISTS (SELECT 1 FROM follows
                                WHERE account_id = f.account_id AND target_account_id = $2)
               ) AS "eligible!"
        FROM follows f
        JOIN accounts a ON a.id = f.account_id
        WHERE f.target_account_id = $1 AND a.domain IS NULL AND NOT f.pending
        ORDER BY f.account_id
        "#,
        source_id,
        target_id,
        target_domain,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One local blocker of the moving account, with the target-side follow edge
/// (`block_account` severs it with a `Reject`) and whether the block carries
/// over at all (Mastodon's `skip_block_move?`: not the target, not already
/// blocking it, not following it).
#[derive(Debug)]
pub struct BlockerCarry {
    pub blocker_id: i64,
    pub username: String,
    /// The target→blocker follow edge's URI, if that edge exists.
    pub target_edge_uri: Option<String>,
    pub target_follows_blocker: bool,
    pub eligible: bool,
}

/// The local blockers of `source`, each with its carry eligibility toward
/// `target`, in one query. Mirrors `block::local_blocker_ids` (ordered by
/// blocker id) plus the skip rules the per-edge loop checked.
pub async fn blocker_carries(
    pool: &PgPool,
    source_id: i64,
    target_id: i64,
) -> Result<Vec<BlockerCarry>, DbError> {
    let rows = sqlx::query_as!(
        BlockerCarry,
        r#"
        SELECT b.account_id AS "blocker_id!", a.username AS "username!",
               tf.uri AS "target_edge_uri?",
               tf.account_id IS NOT NULL AS "target_follows_blocker!",
               (b.account_id <> $2
                AND NOT EXISTS (SELECT 1 FROM blocks
                                WHERE account_id = b.account_id AND target_account_id = $2)
                AND NOT EXISTS (SELECT 1 FROM follows
                                WHERE account_id = b.account_id AND target_account_id = $2)
               ) AS "eligible!"
        FROM blocks b
        JOIN accounts a ON a.id = b.account_id AND a.domain IS NULL
        LEFT JOIN follows tf ON tf.account_id = $2 AND tf.target_account_id = b.account_id
        WHERE b.target_account_id = $1
        ORDER BY b.account_id
        "#,
        source_id,
        target_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Set-inserts the re-follow edges toward `target_id`: one row per
/// `(follower, uri, pending)` triple. The idempotent `ON CONFLICT` mirrors
/// `follow::create`'s, so a race with a genuine follow refreshes rather than
/// errors.
pub async fn insert_follow_edges<'e, E: PgExecutor<'e>>(
    executor: E,
    target_id: i64,
    follower_ids: &[i64],
    uris: &[Option<String>],
    pending: bool,
) -> Result<(), DbError> {
    if follower_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = follower_ids.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        INSERT INTO follows (id, account_id, target_account_id, uri, pending)
        SELECT v.id, v.account_id, $4, v.uri, $5
        FROM unnest($1::bigint[], $2::bigint[], $3::text[]) AS v(id, account_id, uri)
        ON CONFLICT (account_id, target_account_id)
            DO UPDATE SET uri = EXCLUDED.uri, pending = EXCLUDED.pending
        "#,
        &ids,
        follower_ids,
        uris as &[Option<String>],
        target_id,
        pending,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Set-inserts the carried block rows, returning each blocker's block row id
/// (the `Block` activity's synthetic URI embeds it). The `ON CONFLICT`
/// mirrors `block::create`'s, so a racing duplicate keeps the original row
/// and its id.
pub async fn insert_blocks<'e, E: PgExecutor<'e>>(
    executor: E,
    blocker_ids: &[i64],
    target_id: i64,
) -> Result<std::collections::HashMap<i64, i64>, DbError> {
    if blocker_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let ids: Vec<i64> = blocker_ids.iter().map(|_| id::next()).collect();
    let rows = sqlx::query!(
        r#"
        INSERT INTO blocks (id, account_id, target_account_id)
        SELECT v.id, v.account_id, $3
        FROM unnest($1::bigint[], $2::bigint[]) AS v(id, account_id)
        ON CONFLICT (account_id, target_account_id)
            DO UPDATE SET uri = blocks.uri
        RETURNING account_id AS "blocker_id!", id AS "block_row_id!"
        "#,
        &ids,
        blocker_ids,
        target_id,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.blocker_id, row.block_row_id))
        .collect())
}
