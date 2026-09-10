//! The cross-process event feed behind the streaming API.
//!
//! Writers announce row changes with `pg_notify` on [`CHANNEL`]; the server
//! process holds one `LISTEN`ing connection and fans the events out to its
//! websocket subscribers. Payloads are id references, never entities —
//! NOTIFY caps payloads at 8000 bytes and rendering is per-recipient anyway.
//! Going through Postgres (instead of an in-process bus) means events
//! published by other processes, like the CLI `post` command, still reach
//! the running server, and NOTIFY's transactional delivery never announces
//! rolled-back work.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgListener;

use crate::DbError;

/// The NOTIFY channel every event goes through.
pub const CHANNEL: &str = "plamenu_streaming";

/// A `LISTEN` connection to `pool`'s database that lives *outside* the
/// pool's limits. The listener holds its connection for the lifetime of the
/// process; taking it from the pool would permanently consume a slot (and
/// starve the tiny shared pools `#[sqlx::test]` hands out).
pub async fn listener(pool: &PgPool) -> Result<PgListener, DbError> {
    let options = (*pool.connect_options()).clone();
    let dedicated = sqlx::pool::PoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(options);
    let mut listener = PgListener::connect_with(&dedicated).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

/// One announced row change. `Notification` and `Conversation` are emitted
/// by database triggers (migration `0022_streaming.sql`) — their JSON shape
/// must stay in sync with the trigger functions there.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A status was created: a local post, a boost, or inbound federation.
    StatusNew { status_id: i64 },
    /// A status was edited.
    StatusEdit { status_id: i64 },
    /// A status is gone. Routing data is captured in the event because the
    /// row (and its cascaded tag/mention links) no longer exists by the
    /// time the listener processes it.
    StatusDelete {
        status_id: i64,
        account_id: i64,
        /// The author is local (`public:local` vs `public:remote` streams).
        local: bool,
        /// A local-only original; it only appears on authenticated local streams.
        #[serde(default)]
        local_only: bool,
        /// It was on the public timelines (a public original, not a boost).
        public: bool,
        /// Lowercased hashtag names, for the hashtag streams.
        tags: Vec<String>,
        /// Local participants of a deleted direct message.
        direct_recipients: Vec<i64>,
    },
    /// A notification row was inserted (trigger-emitted).
    Notification {
        notification_id: i64,
        account_id: i64,
    },
    /// A new status landed in a participant's conversation row
    /// (trigger-emitted).
    Conversation { row_id: i64, account_id: i64 },
    /// An announcement's reaction tally for one emoji changed (added or
    /// removed). Broadcast to every `user` stream, like Mastodon's
    /// announcement-reaction publish job.
    AnnouncementReaction { announcement_id: i64, name: String },
    /// A server announcement was published and should appear on every `user`
    /// stream.
    Announcement { announcement_id: i64 },
    /// A server announcement was deleted and should be removed from every
    /// `user` stream.
    AnnouncementDelete { announcement_id: i64 },
}

/// Announces an event to every listening process.
pub async fn publish(pool: &PgPool, event: &Event) -> Result<(), DbError> {
    let payload = serde_json::to_string(event).expect("streaming events serialize");
    sqlx::query_scalar!(
        r#"SELECT pg_notify($1, $2) AS "sent!: ()""#,
        CHANNEL,
        payload,
    )
    .fetch_one(pool)
    .await?;
    Ok(())
}

/// Which of `candidates` (accounts with live `user` streams) would see a
/// new status by `author_id` on their home timeline: the author and their
/// accepted followers, minus viewers the timeline hides it from — the same
/// `account_hidden`, exclusive-list and per-follow settings (M32, plus the
/// `with_replies`) gates as `status::home_timeline`, applied to the booster
/// and (for boosts) the boosted author. `language` is the status' language,
/// for the per-follow language filter (boost rows carry none).
///
/// `reply_parent_id` is the status' `in_reply_to_id` — `None` for an original
/// *and* for a reply whose parent has never arrived, which is what the
/// timeline query's `in_reply_to_id IS NULL` arm says too. The parent's author
/// is resolved here rather than by the caller so a routed reply costs one
/// probe inside this query instead of a round trip before it.
/// `boost_target_is_reply` carries the reply flag for boost rows; the caller has already
/// loaded the target to find its author.
pub async fn home_stream_recipients(
    pool: &PgPool,
    candidates: &[i64],
    author_id: i64,
    boosted_author_id: Option<i64>,
    language: Option<&str>,
    reply_parent_id: Option<i64>,
    boost_target_is_reply: bool,
) -> Result<Vec<i64>, DbError> {
    let recipients = sqlx::query_scalar!(
        r#"
        SELECT v.viewer AS "viewer!"
        FROM unnest($1::bigint[]) AS v(viewer)
        WHERE (v.viewer = $2 OR EXISTS (
                  SELECT 1 FROM follows f
                  WHERE f.account_id = v.viewer
                    AND f.target_account_id = $2 AND NOT f.pending))
          AND NOT account_hidden(v.viewer, $2)
          AND ($3::bigint IS NULL OR NOT account_hidden(v.viewer, $3))
          AND (v.viewer = $2 OR NOT EXISTS (
                  SELECT 1 FROM list_accounts la
                  JOIN lists l ON l.id = la.list_id
                  WHERE l.account_id = v.viewer AND l.exclusive
                    AND la.account_id = $2))
          AND (v.viewer = $2 OR NOT EXISTS (
                  SELECT 1 FROM follows sf
                  WHERE sf.account_id = v.viewer AND sf.target_account_id = $2
                    AND NOT sf.pending
                    AND (($3::bigint IS NOT NULL AND NOT sf.show_reblogs)
                         OR ($4::text IS NOT NULL AND sf.languages IS NOT NULL
                             AND $4 <> ALL (sf.languages))
                         -- Per-follow `with_replies`, in the same
                         -- follow probe rather than a second one. Kept in step
                         -- with home arm A's row filter: the boost arm first,
                         -- then the three exemptions.
                         OR (NOT sf.with_replies
                             AND ($6
                                  OR ($5::bigint IS NOT NULL AND NOT EXISTS (
                                        SELECT 1 FROM statuses p -- STUBKEEP: delivery-time probe on a live post
                                        WHERE p.id = $5
                                          AND (p.account_id = $2
                                               OR p.account_id = v.viewer
                                               OR EXISTS (
                                                   SELECT 1 FROM follows rf
                                                   WHERE rf.account_id = v.viewer
                                                     AND rf.target_account_id = p.account_id
                                                     AND NOT rf.pending)))))))))
          -- Reading-side language filter: mirrors home_timeline's
          -- `chosen_languages` gate. Own posts and boosts (no language) pass.
          AND (v.viewer = $2 OR $4::text IS NULL OR NOT EXISTS (
                  SELECT 1 FROM users u
                  WHERE u.account_id = v.viewer AND u.chosen_languages IS NOT NULL
                    AND $4 <> ALL (u.chosen_languages)))
        "#,
        candidates,
        author_id,
        boosted_author_id,
        language,
        reply_parent_id,
        boost_target_is_reply,
    )
    .fetch_all(pool)
    .await?;
    Ok(recipients)
}

/// Which of `candidates` would receive `status_id` on their home timeline
/// through a *followed hashtag* — the live half of `status::home_timeline`'s
/// tag injection: a public, non-reblog status carrying a tag the viewer
/// follows, passing the same reply, `account_hidden` and exclusive-list
/// gates. Overlaps with [`home_stream_recipients`] (a viewer following both
/// the author and a tag); callers dedupe.
pub async fn tag_follow_stream_recipients(
    pool: &PgPool,
    candidates: &[i64],
    status_id: i64,
) -> Result<Vec<i64>, DbError> {
    let recipients = sqlx::query_scalar!(
        r#"
        SELECT v.viewer AS "viewer!"
        FROM unnest($1::bigint[]) AS v(viewer)
        JOIN statuses s ON s.id = $2 -- STUBKEEP: delivery-time probe on a live post
        WHERE s.reblog_of_id IS NULL
          AND s.visibility = 'public'
          AND NOT account_hidden(v.viewer, s.account_id)
          AND (s.in_reply_to_id IS NULL
               OR EXISTS (
                   SELECT 1 FROM statuses p
                   WHERE p.id = s.in_reply_to_id
                     AND (p.account_id = v.viewer
                          OR p.account_id = s.account_id
                          OR EXISTS (
                              SELECT 1 FROM follows rf
                              WHERE rf.account_id = v.viewer
                                AND rf.target_account_id = p.account_id
                                AND NOT rf.pending))))
          AND (v.viewer = s.account_id OR NOT EXISTS (
                  SELECT 1 FROM list_accounts la
                  JOIN lists l ON l.id = la.list_id
                  WHERE l.account_id = v.viewer AND l.exclusive
                    AND la.account_id = s.account_id))
          AND EXISTS (
              SELECT 1 FROM status_tags st
              JOIN tag_follows tf ON tf.tag_id = st.tag_id
              WHERE st.status_id = s.id AND tf.account_id = v.viewer)
        "#,
        candidates,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(recipients)
}

/// Which of `candidates` follow any of `names` (already lowercased tag
/// names) — for delivering a tag-injected status's `delete` to the home
/// streams it reached. Sending a bare id leaks nothing, so this skips the
/// visibility re-check the create path applies.
pub async fn tag_follow_viewers_by_names(
    pool: &PgPool,
    candidates: &[i64],
    names: &[String],
) -> Result<Vec<i64>, DbError> {
    let viewers = sqlx::query_scalar!(
        r#"
        SELECT v.viewer AS "viewer!"
        FROM unnest($1::bigint[]) AS v(viewer)
        WHERE EXISTS (
            SELECT 1 FROM tag_follows tf
            JOIN tags t ON t.id = tf.tag_id
            WHERE tf.account_id = v.viewer AND lower(t.name) = ANY($2))
        "#,
        candidates,
        names,
    )
    .fetch_all(pool)
    .await?;
    Ok(viewers)
}

/// Which of `candidates` may see `author_id`'s posts on the shared (public
/// and hashtag) streams — the `account_hidden` filter those timelines apply.
pub async fn unhidden_viewers(
    pool: &PgPool,
    candidates: &[i64],
    author_id: i64,
) -> Result<Vec<i64>, DbError> {
    let viewers = sqlx::query_scalar!(
        r#"
        SELECT v.viewer AS "viewer!"
        FROM unnest($1::bigint[]) AS v(viewer)
        WHERE NOT account_hidden(v.viewer, $2)
        "#,
        candidates,
        author_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(viewers)
}

/// Whether `recipient`'s notification listing would hide notifications from
/// `sender` — the same `sender_filtered` gate the listing queries use.
pub async fn notification_filtered(
    pool: &PgPool,
    recipient: i64,
    sender: i64,
) -> Result<bool, DbError> {
    let filtered = sqlx::query_scalar!(
        r#"SELECT sender_filtered($1, $2) AS "filtered!""#,
        recipient,
        sender,
    )
    .fetch_one(pool)
    .await?;
    Ok(filtered)
}
