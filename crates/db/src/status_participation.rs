//! RSVPs to event statuses — the `Join` / `Accept(Join)` / `Reject(Join)` /
//! `Leave` family (E2/E3). One row per (status, account), dying with either
//! side.
//!
//! Unlike a favourite, an RSVP is a negotiation: proposed, then accepted or
//! refused by the organizer. So the row carries a [`State`] rather than merely
//! existing, and a refusal is *kept* — deleting it would offer the button again
//! as if nothing had happened, and would let a redelivered `Join` launder a
//! refusal back into `pending`.

use std::collections::HashMap;
use std::fmt;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

/// Where an RSVP stands.
///
/// `Pending` is a resting state, not a transient one: on a `restricted` event it
/// lasts until a moderator acts, and a `Join` that hit the origin's capacity
/// stays there **forever** — Mobilizon sends no rejection activity when an event
/// is full. Nothing may treat a long-pending row as a failure to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The organizer invited us, unprompted. Not attendance — standing
    /// permission to join an event that would otherwise refuse us.
    Invited,
    Pending,
    Accepted,
    Rejected,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invited => "invited",
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    /// Parses a stored state. The CHECK constraint keeps the column to the four
    /// known values, so anything else is a row this build didn't write — read
    /// conservatively as `Pending` rather than panicking a timeline render.
    #[must_use]
    pub fn from_str_lossy(value: &str) -> Self {
        match value {
            "invited" => Self::Invited,
            "accepted" => Self::Accepted,
            "rejected" => Self::Rejected,
            _ => Self::Pending,
        }
    }

    /// Whether this state counts as attending.
    #[must_use]
    pub fn is_attending(self) -> bool {
        self == Self::Accepted
    }

    /// Whether the organizer has answered. An `Invited` row has no answer *from
    /// the attendee*; a `Pending` one has none from the organizer.
    #[must_use]
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Accepted | Self::Rejected)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One RSVP.
#[derive(Debug, Clone)]
pub struct Participation {
    pub id: i64,
    pub status_id: i64,
    pub account_id: i64,
    pub state: State,
    /// The `Join` activity id — ours when outbound, theirs when inbound. The
    /// handle an `Accept`/`Reject` echoes back.
    pub uri: Option<String>,
    pub message: Option<String>,
    pub created_at: OffsetDateTime,
}

struct Row {
    id: i64,
    status_id: i64,
    account_id: i64,
    state: String,
    uri: Option<String>,
    message: Option<String>,
    created_at: OffsetDateTime,
}

impl From<Row> for Participation {
    fn from(row: Row) -> Self {
        Self {
            id: row.id,
            status_id: row.status_id,
            account_id: row.account_id,
            state: State::from_str_lossy(&row.state),
            uri: row.uri,
            message: row.message,
            created_at: row.created_at,
        }
    }
}

/// Records (or re-records) an RSVP, returning the row as stored.
///
/// A repeat `Join` for a row that is already `accepted` or `rejected` keeps that
/// verdict — only the uri and message are refreshed. Re-opening a settled
/// negotiation is the organizer's call, expressed by an `Accept`/`Reject`, never
/// a side effect of a redelivered `Join`. An `invited` row *does* advance, since
/// the invitee acting on an invitation is exactly the transition it exists for.
pub async fn upsert(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
    state: State,
    uri: Option<&str>,
    message: Option<&str>,
) -> Result<Participation, DbError> {
    upsert_with_id(pool, id::next(), status_id, account_id, state, uri, message).await
}

/// [`upsert`] with the row id supplied by the caller.
///
/// Ids are client-side snowflakes, so an outbound RSVP can mint its id, derive the
/// `Join` activity uri from it, and insert once — rather than inserting to learn
/// the id and then writing again to stamp the uri.
pub async fn upsert_with_id<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    id: i64,
    status_id: i64,
    account_id: i64,
    state: State,
    uri: Option<&str>,
    message: Option<&str>,
) -> Result<Participation, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO status_participations (id, status_id, account_id, state, uri, message)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (account_id, status_id) DO UPDATE
        SET uri = COALESCE(EXCLUDED.uri, status_participations.uri),
            message = COALESCE(EXCLUDED.message, status_participations.message),
            -- A settled verdict survives a redelivered Join; an invitation is
            -- consumed by one.
            state = CASE WHEN status_participations.state IN ('pending', 'invited')
                         THEN EXCLUDED.state ELSE status_participations.state END,
            updated_at = now()
        RETURNING id, status_id, account_id, state, uri, message, created_at
        "#,
        id,
        status_id,
        account_id,
        state.as_str(),
        uri,
        message,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

/// Moves an RSVP to a settled state — an `Accept(Join)` or `Reject(Join)`
/// landing. Returns the updated row, or `None` when there is nothing to settle
/// (an `Accept` for an RSVP we never made, or already deleted).
pub async fn settle<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
    state: State,
) -> Result<Option<Participation>, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE status_participations SET state = $3, updated_at = now()
        WHERE status_id = $1 AND account_id = $2
        RETURNING id, status_id, account_id, state, uri, message, created_at
        "#,
        status_id,
        account_id,
        state.as_str(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Settles the RSVP a `Join` uri identifies. The uri is the only handle an
/// origin echoes in its `Accept`/`Reject`, so this is the primary resolution
/// path for a remote verdict; the (status, account) form above is for a verdict
/// we reach by other means.
pub async fn settle_by_uri(
    pool: &PgPool,
    uri: &str,
    state: State,
) -> Result<Option<Participation>, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE status_participations SET state = $2, updated_at = now()
        WHERE uri = $1
        RETURNING id, status_id, account_id, state, uri, message, created_at
        "#,
        uri,
        state.as_str(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Drops an RSVP outright — a `Leave` (ours or theirs), which withdraws the
/// request rather than settling it. Returns the removed row.
pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Participation>, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        DELETE FROM status_participations WHERE status_id = $1 AND account_id = $2
        RETURNING id, status_id, account_id, state, uri, message, created_at
        "#,
        status_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// One account's RSVP to one status.
pub async fn find(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
) -> Result<Option<Participation>, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id, status_id, account_id, state, uri, message, created_at
           FROM status_participations WHERE status_id = $1 AND account_id = $2"#,
        status_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// The RSVP a `Join` uri identifies, without changing it.
pub async fn find_by_uri(pool: &PgPool, uri: &str) -> Result<Option<Participation>, DbError> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id, status_id, account_id, state, uri, message, created_at
           FROM status_participations WHERE uri = $1"#,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// The viewer's own RSVP state for a status batch — for the render maps.
pub async fn states_of(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<HashMap<i64, Participation>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, status_id, account_id, state, uri, message, created_at
           FROM status_participations
           WHERE account_id = $1 AND status_id = ANY($2)"#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.status_id, row.into()))
        .collect())
}

/// [`states_of`] across a set of viewers in one query, keyed by `(viewer,
/// status id)` — each recipient's own RSVP state for the render maps.
pub async fn states_for_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<HashMap<(i64, i64), Participation>, DbError> {
    if status_ids.is_empty() || viewer_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, status_id, account_id, state, uri, message, created_at
           FROM status_participations
           WHERE account_id = ANY($1) AND status_id = ANY($2)"#,
        viewer_ids,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ((row.account_id, row.status_id), row.into()))
        .collect())
}

/// Every RSVP to one status, oldest first — the organizer's attendee list.
pub async fn for_status(pool: &PgPool, status_id: i64) -> Result<Vec<Participation>, DbError> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, status_id, account_id, state, uri, message, created_at
           FROM status_participations WHERE status_id = $1
           ORDER BY created_at, id"#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// How many accepted attendees a **local** event has.
///
/// Only meaningful for an event we host: on a remote one we see just the
/// participation activities addressed to us, so this would systematically
/// undercount, and the origin's `participant_count` is authoritative instead.
/// The two are never mixed.
/// Generic over the executor — the authoring path counts inside its own
/// transaction.
pub async fn accepted_count<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM status_participations
           WHERE status_id = $1 AND state = 'accepted'"#,
        status_id,
    )
    .fetch_one(executor)
    .await?;
    Ok(count)
}

/// The delivery inboxes of **remote** accounts with a live RSVP to `status_id`.
///
/// Shaped exactly like `follow::follower_inboxes`, and for the same reason: the
/// shared inbox wins where a host advertises one, and `DISTINCT` collapses a
/// whole host to one delivery. Building this list from personal `inbox_url`s
/// instead would turn 200 attendees on one server into 200 signed POSTs.
///
/// `accepted` and `pending` only — a refused attendee is not coming, and an
/// invitee who never acted has nothing to re-plan.
pub async fn live_remote_inboxes<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<Vec<String>, DbError> {
    let inboxes = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT
            CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url
                 ELSE a.inbox_url
            END AS "inbox!"
        FROM status_participations p
        JOIN accounts a ON a.id = p.account_id
        WHERE p.status_id = $1
          AND p.state IN ('accepted', 'pending')
          AND a.domain IS NOT NULL
          AND (a.shared_inbox_url <> '' OR a.inbox_url <> '')
        "#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(inboxes)
}

/// The **local** account ids with a live RSVP to `status_id` — who to notify when
/// the event moves or is cancelled. One query instead of a lookup per row.
pub async fn live_local_account_ids(pool: &PgPool, status_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"SELECT p.account_id AS "id!"
           FROM status_participations p
           JOIN accounts a ON a.id = p.account_id
           WHERE p.status_id = $1
             AND p.state IN ('accepted', 'pending')
             AND a.domain IS NULL"#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Which of `status_ids` are authored by a **local** account.
///
/// The attendance count of a local event comes from our own rows and that of a
/// remote one from the origin's `participantCount` — never a mix, since we only
/// receive the participation activities addressed to us. Rendering a timeline
/// needs that split in one query rather than an account lookup per status.
pub async fn locally_authored_of(pool: &PgPool, status_ids: &[i64]) -> Result<Vec<i64>, DbError> {
    if status_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_scalar!(
        r#"SELECT s.id AS "id!" FROM statuses s -- STUBKEEP: identity probe over caller-chosen ids
           JOIN accounts a ON a.id = s.account_id
           WHERE s.id = ANY($1) AND a.domain IS NULL"#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Accepted-attendee counts for a status batch, keyed by status id.
pub async fn accepted_counts<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, i64>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"SELECT status_id AS "status_id!", count(*) AS "count!"
           FROM status_participations
           WHERE status_id = ANY($1) AND state = 'accepted'
           GROUP BY status_id"#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.status_id, row.count))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_reads_unknown_conservatively() {
        for state in [
            State::Invited,
            State::Pending,
            State::Accepted,
            State::Rejected,
        ] {
            assert_eq!(State::from_str_lossy(state.as_str()), state);
        }
        // A row this build didn't write must not panic a timeline render.
        assert_eq!(State::from_str_lossy("something-else"), State::Pending);
    }

    #[test]
    fn only_accepted_attends_and_only_a_verdict_settles() {
        assert!(State::Accepted.is_attending());
        for other in [State::Invited, State::Pending, State::Rejected] {
            assert!(!other.is_attending(), "{other} is not attendance");
        }
        assert!(State::Accepted.is_settled());
        assert!(State::Rejected.is_settled());
        // An invitation is unanswered *by the attendee*; pending is unanswered
        // by the organizer. Neither is a verdict.
        assert!(!State::Invited.is_settled());
        assert!(!State::Pending.is_settled());
    }
}
