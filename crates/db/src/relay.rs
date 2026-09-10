//! `ActivityPub` relays — Mastodon's `Relay`. A relay is subscribed by the
//! *instance actor* sending `Follow(as:Public)` to its inbox; `state` tracks
//! the handshake (`idle` → `pending` → `accepted`/`rejected`) keyed by the
//! Follow's activity id, which the relay echoes back in its response.

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Relay {
    pub id: i64,
    pub inbox_url: String,
    /// The relay actor's own URI — the only identity that cannot collide with
    /// every other actor on its host (a general-purpose server's relay actor
    /// advertises the shared inbox). NULL on rows predating migration 0028.
    pub actor_uri: Option<String>,
    pub state: String,
    pub follow_activity_id: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Every configured relay, oldest first.
pub async fn list(pool: &PgPool) -> Result<Vec<Relay>, DbError> {
    let relays = sqlx::query_as!(
        Relay,
        r#"
        SELECT id, inbox_url, actor_uri, state, follow_activity_id, created_at, updated_at
        FROM relays
        ORDER BY id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(relays)
}

pub async fn find<'e, E: sqlx::PgExecutor<'e>>(pool: E, id: i64) -> Result<Option<Relay>, DbError> {
    let relay = sqlx::query_as!(
        Relay,
        r#"
        SELECT id, inbox_url, actor_uri, state, follow_activity_id, created_at, updated_at
        FROM relays
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(relay)
}

/// Registers a relay in `idle` state; `None` if the inbox URL is taken.
pub async fn create(
    pool: &PgPool,
    inbox_url: &str,
    actor_uri: Option<&str>,
) -> Result<Option<Relay>, DbError> {
    let relay = sqlx::query_as!(
        Relay,
        r#"
        INSERT INTO relays (id, inbox_url, actor_uri)
        VALUES ($1, $2, $3)
        ON CONFLICT (inbox_url) DO NOTHING
        RETURNING id, inbox_url, actor_uri, state, follow_activity_id, created_at, updated_at
        "#,
        id::next(),
        inbox_url.trim(),
        actor_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(relay)
}

pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(pool: E, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM relays WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The subscription request went out: `pending`, remembering the Follow's
/// activity id, as Mastodon does when enabling a relay.
pub async fn mark_pending<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    id: i64,
    follow_activity_id: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE relays
        SET state = 'pending', follow_activity_id = $2, updated_at = now()
        WHERE id = $1
        "#,
        id,
        follow_activity_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Unsubscribed: back to `idle`, dropping the handshake id, as Mastodon
/// does when disabling a relay.
pub async fn mark_idle<'e, E: sqlx::PgExecutor<'e>>(pool: E, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE relays
        SET state = 'idle', follow_activity_id = NULL, updated_at = now()
        WHERE id = $1
        "#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Applies a relay's `Accept`/`Reject` whose object is our Follow with this
/// activity id. Returns whether a relay matched (Mastodon's
/// `Relay.find_by(follow_activity_id:)` branch in Accept/Reject handling).
pub async fn resolve_follow_response(
    pool: &PgPool,
    follow_activity_id: &str,
    accepted: bool,
) -> Result<bool, DbError> {
    let state = if accepted { "accepted" } else { "rejected" };
    let result = sqlx::query!(
        r#"
        UPDATE relays
        SET state = $2, updated_at = now()
        WHERE follow_activity_id = $1
        "#,
        follow_activity_id,
        state,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Inboxes of accepted relays — added to public fan-out (Mastodon's
/// `Relay.enabled`, aliased from `accepted`).
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn enabled_inboxes<'e, E: PgExecutor<'e>>(executor: E) -> Result<Vec<String>, DbError> {
    let inboxes =
        sqlx::query_scalar!("SELECT inbox_url FROM relays WHERE state = 'accepted' ORDER BY id")
            .fetch_all(executor)
            .await?;
    Ok(inboxes)
}

/// The accepted relay owning `inbox_url`, if any — how an inbound `Announce`
/// is recognised as relayed (Mastodon's `Relay.find_by(inbox_url:)&.enabled?`),
/// returning the id so the caller can attribute the delivery.
/// The accepted relay with this actor URI — the collision-free identity check.
pub async fn enabled_id_by_actor(pool: &PgPool, actor_uri: &str) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM relays WHERE actor_uri = $1 AND state = 'accepted'",
        actor_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

pub async fn enabled_id_by_inbox(pool: &PgPool, inbox_url: &str) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM relays WHERE inbox_url = $1 AND state = 'accepted'",
        inbox_url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// The accepted relay matching a signed sender. Exact actor identity wins;
/// inbox fallback is only for legacy rows that predate persisted actor URIs.
/// One query keeps ordinary Announce traffic from paying two relay probes.
pub async fn enabled_id_for_sender(
    pool: &PgPool,
    actor_uri: Option<&str>,
    inbox_url: &str,
) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(
            (SELECT id FROM relays
             WHERE actor_uri = $1 AND state = 'accepted'),
            (SELECT id FROM relays
             WHERE actor_uri IS NULL AND inbox_url = $2 AND state = 'accepted')
        ) AS "id?"
        "#,
        actor_uri,
        inbox_url,
    )
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Whether `inbox_url` belongs to an accepted relay.
pub async fn is_enabled_inbox(pool: &PgPool, inbox_url: &str) -> Result<bool, DbError> {
    Ok(enabled_id_by_inbox(pool, inbox_url).await?.is_some())
}

/// Counts one activity received through a relay, in today's (UTC) bucket —
/// the `daily_interactions` upsert-increment pattern, keyed per relay.
pub async fn record_activity(pool: &PgPool, relay_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO relay_daily_activities (relay_id, day, count)
        VALUES ($1, (now() AT TIME ZONE 'UTC')::date, 1)
        ON CONFLICT (relay_id, day)
        DO UPDATE SET count = relay_daily_activities.count + 1
        "#,
        relay_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A relay's inbound-volume rollup: everything ever, and the trailing 7 days
/// (today inclusive).
#[derive(Debug, Clone, Copy, Default)]
pub struct RelayActivity {
    pub total: i64,
    pub last_week: i64,
}

/// The activity rollup for every relay that has ever received one, as
/// `(relay_id, activity)` pairs.
pub async fn activity_totals(pool: &PgPool) -> Result<Vec<(i64, RelayActivity)>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT relay_id,
               COALESCE(SUM(count), 0)::bigint AS "total!",
               COALESCE(SUM(count) FILTER (
                   WHERE day >= (now() AT TIME ZONE 'UTC')::date - 6), 0)::bigint
                   AS "last_week!"
        FROM relay_daily_activities
        GROUP BY relay_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.relay_id,
                RelayActivity {
                    total: row.total,
                    last_week: row.last_week,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn relay_handshake_lifecycle(pool: PgPool) {
        let relay = create(&pool, " https://relay.example/inbox ", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(relay.inbox_url, "https://relay.example/inbox");
        assert_eq!(relay.state, "idle");
        // Duplicate inbox URL refused.
        assert!(
            create(&pool, "https://relay.example/inbox", None)
                .await
                .unwrap()
                .is_none()
        );

        let follow_id = "https://local.example/payloads/1";
        assert!(mark_pending(&pool, relay.id, follow_id).await.unwrap());
        assert!(enabled_inboxes(&pool).await.unwrap().is_empty());

        assert!(
            resolve_follow_response(&pool, follow_id, true)
                .await
                .unwrap()
        );
        assert_eq!(
            enabled_inboxes(&pool).await.unwrap(),
            vec!["https://relay.example/inbox".to_owned()]
        );
        assert!(
            is_enabled_inbox(&pool, "https://relay.example/inbox")
                .await
                .unwrap()
        );
        // An unknown handshake id matches nothing.
        assert!(
            !resolve_follow_response(&pool, "https://x.example/nope", false)
                .await
                .unwrap()
        );

        assert!(mark_idle(&pool, relay.id).await.unwrap());
        let relay = find(&pool, relay.id).await.unwrap().unwrap();
        assert_eq!(relay.state, "idle");
        assert!(relay.follow_activity_id.is_none());
        assert!(!is_enabled_inbox(&pool, &relay.inbox_url).await.unwrap());

        assert!(delete(&pool, relay.id).await.unwrap());
        assert!(list(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn relay_activity_rollup(pool: PgPool) {
        let relay = create(&pool, "https://relay.example/inbox", None)
            .await
            .unwrap()
            .unwrap();
        let follow_id = "https://local.example/payloads/1";
        mark_pending(&pool, relay.id, follow_id).await.unwrap();
        resolve_follow_response(&pool, follow_id, true)
            .await
            .unwrap();
        assert_eq!(
            enabled_id_by_inbox(&pool, &relay.inbox_url).await.unwrap(),
            Some(relay.id)
        );

        assert!(activity_totals(&pool).await.unwrap().is_empty());
        record_activity(&pool, relay.id).await.unwrap();
        record_activity(&pool, relay.id).await.unwrap();
        let totals = activity_totals(&pool).await.unwrap();
        assert_eq!(totals.len(), 1);
        let (id, activity) = totals[0];
        assert_eq!(id, relay.id);
        assert_eq!(activity.total, 2);
        assert_eq!(activity.last_week, 2);

        // Deleting the relay cascades its history away.
        delete(&pool, relay.id).await.unwrap();
        assert!(activity_totals(&pool).await.unwrap().is_empty());
    }
}
