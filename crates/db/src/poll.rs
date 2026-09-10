//! Polls and votes (Mastodon's model: one optional poll per status).
//!
//! Tallies: local polls recompute `cached_tallies`/`voters_count` from
//! `poll_votes` after every vote ([`refresh_local_tallies`]); remote polls
//! store whatever the origin server last reported (`Update(Question)`).

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Poll {
    pub id: i64,
    pub status_id: i64,
    pub account_id: i64,
    pub options: Vec<String>,
    pub cached_tallies: Vec<i64>,
    pub multiple: bool,
    pub hide_totals: bool,
    /// Distinct-voter count; `None` when the origin does not report one.
    pub voters_count: Option<i64>,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Poll {
    /// Whether the poll has ended.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|at| at <= OffsetDateTime::now_utc())
    }

    /// Total vote count across options.
    #[must_use]
    pub fn votes_count(&self) -> i64 {
        self.cached_tallies.iter().sum()
    }
}

const COLS: &str = "id, status_id, account_id, options, cached_tallies, multiple, \
                    hide_totals, voters_count, expires_at, created_at, updated_at";
const _: &str = COLS; // documentation: every query selects exactly these

#[derive(Debug)]
pub struct NewPoll<'a> {
    pub status_id: i64,
    pub account_id: i64,
    pub options: &'a [String],
    /// Initial tallies; all zeros for a fresh local poll.
    pub cached_tallies: &'a [i64],
    pub multiple: bool,
    pub hide_totals: bool,
    pub voters_count: Option<i64>,
    pub expires_at: Option<OffsetDateTime>,
}

/// Inserts a poll. Idempotent on the status: re-delivery of the same
/// `Create(Question)` keeps the original row untouched.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn create<'e, E: PgExecutor<'e>>(executor: E, new: NewPoll<'_>) -> Result<Poll, DbError> {
    let row = sqlx::query_as!(
        Poll,
        r#"
        INSERT INTO polls (id, status_id, account_id, options, cached_tallies,
                           multiple, hide_totals, voters_count, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (status_id) DO UPDATE SET status_id = EXCLUDED.status_id
        RETURNING id, status_id, account_id, options, cached_tallies, multiple,
                  hide_totals, voters_count, expires_at, created_at, updated_at
        "#,
        id::next(),
        new.status_id,
        new.account_id,
        new.options,
        new.cached_tallies,
        new.multiple,
        new.hide_totals,
        new.voters_count,
        new.expires_at,
    )
    .fetch_one(executor)
    .await?;
    Ok(row)
}

pub async fn find_by_id(pool: &PgPool, poll_id: i64) -> Result<Option<Poll>, DbError> {
    let row = sqlx::query_as!(
        Poll,
        r#"
        SELECT id, status_id, account_id, options, cached_tallies, multiple,
               hide_totals, voters_count, expires_at, created_at, updated_at
        FROM polls WHERE id = $1
        "#,
        poll_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_by_status(pool: &PgPool, status_id: i64) -> Result<Option<Poll>, DbError> {
    let row = sqlx::query_as!(
        Poll,
        r#"
        SELECT id, status_id, account_id, options, cached_tallies, multiple,
               hide_totals, voters_count, expires_at, created_at, updated_at
        FROM polls WHERE status_id = $1
        "#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Polls for a batch of statuses, keyed by status id.
pub async fn for_statuses<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, Poll>, DbError> {
    let rows = sqlx::query_as!(
        Poll,
        r#"
        SELECT id, status_id, account_id, options, cached_tallies, multiple,
               hide_totals, voters_count, expires_at, created_at, updated_at
        FROM polls WHERE status_id = ANY($1)
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|p| (p.status_id, p)).collect())
}

/// The refreshable fields of a remote poll (`Update(Question)` / re-fetch).
#[derive(Debug)]
pub struct RemotePollUpdate<'a> {
    pub options: &'a [String],
    pub cached_tallies: &'a [i64],
    pub multiple: bool,
    pub voters_count: Option<i64>,
    pub expires_at: Option<OffsetDateTime>,
}

/// Applies the origin server's current poll state. When the options or
/// multiple-choice mode changed the poll was effectively replaced, so stored
/// votes are reset (Mastodon's `significantly_changes?` rule).
pub async fn apply_remote_update(
    pool: &PgPool,
    poll_id: i64,
    update: RemotePollUpdate<'_>,
) -> Result<Poll, DbError> {
    let row = sqlx::query_as!(
        Poll,
        r#"
        UPDATE polls SET options = $2, cached_tallies = $3, multiple = $4,
                         voters_count = $5, expires_at = $6, updated_at = now(),
                         -- A poll whose expiry moved back into the future
                         -- becomes due again for expiry processing.
                         expiry_processed_at = CASE
                             WHEN $6::timestamptz > now() THEN NULL
                             ELSE expiry_processed_at
                         END
        WHERE id = $1
        RETURNING id, status_id, account_id, options, cached_tallies, multiple,
                  hide_totals, voters_count, expires_at, created_at, updated_at
        "#,
        poll_id,
        update.options,
        update.cached_tallies,
        update.multiple,
        update.voters_count,
        update.expires_at,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Removes all recorded votes of a poll (the poll was replaced).
pub async fn reset_votes(pool: &PgPool, poll_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", poll_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records one vote. Idempotent per (poll, account, choice) and per remote
/// vote URI; returns the new row's id, or `None` when it already existed.
pub async fn insert_vote(
    pool: &PgPool,
    poll_id: i64,
    account_id: i64,
    choice: i32,
    uri: Option<&str>,
) -> Result<Option<i64>, DbError> {
    let vote_id = sqlx::query_scalar!(
        r#"
        INSERT INTO poll_votes (id, poll_id, account_id, choice, uri)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
        id::next(),
        poll_id,
        account_id,
        choice,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(vote_id)
}

/// The choices `account_id` voted for on one poll, ascending.
pub async fn votes_by(pool: &PgPool, poll_id: i64, account_id: i64) -> Result<Vec<i32>, DbError> {
    let choices = sqlx::query_scalar!(
        "SELECT choice FROM poll_votes WHERE poll_id = $1 AND account_id = $2 ORDER BY choice",
        poll_id,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(choices)
}

/// A viewer's choices across a batch of polls, keyed by poll id.
pub async fn votes_by_for_polls(
    pool: &PgPool,
    account_id: i64,
    poll_ids: &[i64],
) -> Result<HashMap<i64, Vec<i32>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT poll_id, choice FROM poll_votes
        WHERE account_id = $1 AND poll_id = ANY($2)
        ORDER BY poll_id, choice
        "#,
        account_id,
        poll_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<i32>> = HashMap::new();
    for row in rows {
        map.entry(row.poll_id).or_default().push(row.choice);
    }
    Ok(map)
}

/// [`votes_by_for_polls`] across a set of viewers in one query, keyed by
/// `(viewer, poll id)` — each viewer's choices in choice order, exactly as the
/// singular form returns them.
pub async fn votes_by_for_polls_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    poll_ids: &[i64],
) -> Result<HashMap<(i64, i64), Vec<i32>>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT account_id, poll_id, choice FROM poll_votes
        WHERE account_id = ANY($1) AND poll_id = ANY($2)
        ORDER BY account_id, poll_id, choice
        "#,
        viewer_ids,
        poll_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<(i64, i64), Vec<i32>> = HashMap::new();
    for row in rows {
        map.entry((row.account_id, row.poll_id))
            .or_default()
            .push(row.choice);
    }
    Ok(map)
}

/// Claims polls whose expiry has passed and was not processed yet, stamping
/// them processed — each poll is returned exactly once, so the expiry
/// sweeper can notify and fan out without double-sending.
///
/// The stamp is the durable, idempotent part: a crashed sweeper leaves the poll
/// `expiry_processed_at`-stamped (closed) and never re-opens it. Only the
/// **side effects** (the `poll` notifications and the final `Update(Question)`
/// fan-out) are at-most-once, and deliberately so: re-running them
/// after a crash would double-notify voters and re-federate a close. Plamenu
/// (like Mastodon's `PollExpirationNotifyWorker`) treats those as best-effort
/// rather than risk duplicates. This is a product decision, not the general
/// durability guarantee.
pub async fn claim_due_expirations(pool: &PgPool, limit: i64) -> Result<Vec<Poll>, DbError> {
    let rows = sqlx::query_as!(
        Poll,
        r#"
        UPDATE polls SET expiry_processed_at = now()
        WHERE id IN (
            SELECT id FROM polls
            WHERE expires_at IS NOT NULL AND expires_at <= now()
              AND expiry_processed_at IS NULL
            ORDER BY expires_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, status_id, account_id, options, cached_tallies, multiple,
                  hide_totals, voters_count, expires_at, created_at, updated_at
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Local accounts that voted on a poll, ascending — the audience of the
/// `poll` notification at expiry (the author is notified separately).
pub async fn local_voters_of(pool: &PgPool, poll_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT v.account_id
        FROM poll_votes v
        JOIN accounts a ON a.id = v.account_id
        WHERE v.poll_id = $1 AND a.domain IS NULL
        ORDER BY v.account_id
        "#,
        poll_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Recomputes a local poll's tallies and voter count from its vote rows.
pub async fn refresh_local_tallies(pool: &PgPool, poll_id: i64) -> Result<Poll, DbError> {
    let row = sqlx::query_as!(
        Poll,
        r#"
        UPDATE polls p SET
            cached_tallies = (
                SELECT COALESCE(array_agg(COALESCE(v.cnt, 0) ORDER BY gs.i), '{}')
                FROM generate_subscripts(p.options, 1) AS gs(i)
                LEFT JOIN (SELECT choice, count(*) AS cnt FROM poll_votes
                           WHERE poll_id = p.id GROUP BY choice) v
                       ON v.choice = gs.i - 1
            ),
            voters_count = (SELECT count(DISTINCT account_id) FROM poll_votes
                            WHERE poll_id = p.id),
            updated_at = now()
        WHERE p.id = $1
        RETURNING id, status_id, account_id,
                  options AS "options!", cached_tallies AS "cached_tallies!",
                  multiple, hide_totals, voters_count, expires_at,
                  created_at, updated_at
        "#,
        poll_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

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

    async fn poll_status(pool: &PgPool, account_id: i64) -> i64 {
        status::create_local(
            pool,
            NewLocalStatus::new(account_id, "<p>pick one</p>", "public", None),
        )
        .await
        .unwrap()
        .id
    }

    fn new_poll(status_id: i64, account_id: i64, options: &[String]) -> NewPoll<'_> {
        NewPoll {
            status_id,
            account_id,
            options,
            cached_tallies: &[0, 0],
            multiple: false,
            hide_totals: false,
            voters_count: Some(0),
            expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
        }
    }

    #[sqlx::test]
    async fn poll_create_is_idempotent_per_status(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let sid = poll_status(&pool, alice).await;
        let options = vec!["yes".to_owned(), "no".to_owned()];

        let created = create(&pool, new_poll(sid, alice, &options)).await.unwrap();
        assert_eq!(created.options, options);
        assert!(!created.expired());

        let again = create(&pool, new_poll(sid, alice, &options)).await.unwrap();
        assert_eq!(again.id, created.id);

        let map = for_statuses(&pool, &[sid]).await.unwrap();
        assert_eq!(map[&sid].id, created.id);
    }

    #[sqlx::test]
    async fn votes_and_local_tallies(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let sid = poll_status(&pool, alice).await;
        let options = vec!["yes".to_owned(), "no".to_owned()];
        let row = create(&pool, new_poll(sid, alice, &options)).await.unwrap();

        assert!(
            insert_vote(&pool, row.id, carol, 0, None)
                .await
                .unwrap()
                .is_some()
        );
        // Same (account, choice) again: no-op.
        assert!(
            insert_vote(&pool, row.id, carol, 0, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            insert_vote(&pool, row.id, dave, 1, None)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            insert_vote(&pool, row.id, dave, 0, None)
                .await
                .unwrap()
                .is_some()
        );

        let refreshed = refresh_local_tallies(&pool, row.id).await.unwrap();
        assert_eq!(refreshed.cached_tallies, [2, 1]);
        assert_eq!(refreshed.voters_count, Some(2));
        assert_eq!(refreshed.votes_count(), 3);

        assert_eq!(votes_by(&pool, row.id, dave).await.unwrap(), [0, 1]);
        let own = votes_by_for_polls(&pool, carol, &[row.id]).await.unwrap();
        assert_eq!(own[&row.id], [0]);

        // Remote-vote URIs are a redelivery guard.
        let bob = local(&pool, "bob").await;
        assert!(
            insert_vote(&pool, row.id, bob, 1, Some("https://r.example/v/1"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            insert_vote(&pool, row.id, bob, 0, Some("https://r.example/v/1"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn remote_update_resets_votes_when_options_change(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let sid = poll_status(&pool, alice).await;
        let options = vec!["yes".to_owned(), "no".to_owned()];
        let row = create(&pool, new_poll(sid, alice, &options)).await.unwrap();
        insert_vote(&pool, row.id, carol, 0, None).await.unwrap();

        let updated = apply_remote_update(
            &pool,
            row.id,
            RemotePollUpdate {
                options: &options,
                cached_tallies: &[5, 7],
                multiple: false,
                voters_count: Some(12),
                expires_at: row.expires_at,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.cached_tallies, [5, 7]);
        assert_eq!(updated.voters_count, Some(12));
        // Tally refresh alone keeps recorded votes.
        assert_eq!(votes_by(&pool, row.id, carol).await.unwrap(), [0]);

        reset_votes(&pool, row.id).await.unwrap();
        assert!(votes_by(&pool, row.id, carol).await.unwrap().is_empty());
    }
}
