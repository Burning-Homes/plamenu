//! Typed Event sidecar for statuses ingested from `Event` objects
//! (Mobilizon, Gancio, …). One optional row per status; rows die with
//! their status via `ON DELETE CASCADE`.

use std::collections::HashMap;

use sqlx::PgExecutor;
use time::OffsetDateTime;

use crate::DbError;

/// The event fields of an `Event` status. All optional: foreign dialects may
/// send as little as a bare `{name}` location.
///
/// `PartialEq` is derived on purpose: the edit path decides whether an event
/// change federates by comparing the stored row with the rebuilt one, and a
/// hand-written field list there silently stops federating any column added
/// later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEvent {
    pub status_id: i64,
    pub start_time: Option<OffsetDateTime>,
    /// Kept as sent — Mobilizon auto-fills `23:59:59` when the organizer set
    /// no end (a sentinel, not data); rendering decides what to show.
    pub end_time: Option<OffsetDateTime>,
    pub location_name: Option<String>,
    /// IANA timezone name as sent (display hint only).
    pub timezone: Option<String>,
    /// `CONFIRMED` | `TENTATIVE` | `CANCELLED` (ical vocabulary, as sent).
    pub event_status: Option<String>,
    /// `free` | `restricted` | `invite` | `external`, lowercased. Decides
    /// whether an RSVP is possible at all, and whether it resolves at once or
    /// waits on a moderator.
    pub join_mode: Option<String>,
    /// The origin's attendee count — authoritative for a remote event, where we
    /// only ever see the participation activities addressed to us.
    pub participant_count: Option<i32>,
    pub max_attendees: Option<i32>,
    pub remaining_attendees: Option<i32>,
    /// Where to RSVP when `join_mode` is `external`.
    pub external_participation_url: Option<String>,
    pub anonymous_participation: Option<bool>,
    pub is_online: Option<bool>,
    pub comments_enabled: Option<bool>,
    /// Origin's own category vocabulary, as sent — a display hint, never a gate.
    pub category: Option<String>,
    /// The `Place`'s own id, and its `PostalAddress` broken out.
    pub location_url: Option<String>,
    pub location_street: Option<String>,
    pub location_locality: Option<String>,
    pub location_region: Option<String>,
    pub location_country: Option<String>,
    pub location_postal_code: Option<String>,
}

impl StatusEvent {
    /// A sidecar with nothing but the owning status — the base every parser
    /// fills in, so a new column can never be silently forgotten by a caller
    /// that only cares about two fields.
    #[must_use]
    pub fn empty(status_id: i64) -> Self {
        Self {
            status_id,
            start_time: None,
            end_time: None,
            location_name: None,
            timezone: None,
            event_status: None,
            join_mode: None,
            participant_count: None,
            max_attendees: None,
            remaining_attendees: None,
            external_participation_url: None,
            anonymous_participation: None,
            is_online: None,
            comments_enabled: None,
            category: None,
            location_url: None,
            location_street: None,
            location_locality: None,
            location_region: None,
            location_country: None,
            location_postal_code: None,
        }
    }

    /// Whether the origin says this event is cancelled — the one `event_status`
    /// value that changes what the UI must say rather than how it reads.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.event_status.as_deref() == Some("CANCELLED")
    }

    /// Whether a change from `self` to `next` is one an attendee has to re-plan
    /// around: the event moved, or it was called off.
    ///
    /// The single owner of that rule. It is consulted from two unrelated places —
    /// the inbound `Update(Event)` ingest and the local edit path — and when it
    /// lived in both, adding a case (a moved venue, say) would have been done in
    /// one and missed in the other.
    #[must_use]
    pub fn disrupts(&self, next: &Self) -> bool {
        self.start_time != next.start_time || (!self.is_cancelled() && next.is_cancelled())
    }

    /// Whether the origin's capacity is exhausted. `remaining_attendees` is
    /// authoritative when sent; otherwise the count/capacity pair is compared.
    /// Unknown capacity is never "full".
    #[must_use]
    pub fn is_full(&self) -> bool {
        if let Some(remaining) = self.remaining_attendees {
            return remaining <= 0;
        }
        match (self.participant_count, self.max_attendees) {
            (Some(count), Some(max)) => count >= max,
            _ => false,
        }
    }
}

/// Writes (or rewrites — an `Update(Event)` may move the date) the sidecar
/// row of an event status.
/// Generic over the executor so a locally-authored event's sidecar can be written
/// inside the same transaction as its status row — a status typed `Event` with no
/// sidecar would serialize as an event with no date.
pub async fn upsert<'e, E: PgExecutor<'e>>(
    executor: E,
    event: &StatusEvent,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO status_events (status_id, start_time, end_time, location_name,
                                    timezone, event_status, join_mode,
                                    participant_count, max_attendees,
                                    remaining_attendees, external_participation_url,
                                    anonymous_participation, is_online,
                                    comments_enabled, category, location_url,
                                    location_street, location_locality,
                                    location_region, location_country,
                                    location_postal_code)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                 $16, $17, $18, $19, $20, $21)
         ON CONFLICT (status_id) DO UPDATE
         SET start_time = EXCLUDED.start_time, end_time = EXCLUDED.end_time,
             location_name = EXCLUDED.location_name, timezone = EXCLUDED.timezone,
             event_status = EXCLUDED.event_status, join_mode = EXCLUDED.join_mode,
             participant_count = EXCLUDED.participant_count,
             max_attendees = EXCLUDED.max_attendees,
             remaining_attendees = EXCLUDED.remaining_attendees,
             external_participation_url = EXCLUDED.external_participation_url,
             anonymous_participation = EXCLUDED.anonymous_participation,
             is_online = EXCLUDED.is_online,
             comments_enabled = EXCLUDED.comments_enabled,
             category = EXCLUDED.category, location_url = EXCLUDED.location_url,
             location_street = EXCLUDED.location_street,
             location_locality = EXCLUDED.location_locality,
             location_region = EXCLUDED.location_region,
             location_country = EXCLUDED.location_country,
             location_postal_code = EXCLUDED.location_postal_code",
        event.status_id,
        event.start_time,
        event.end_time,
        event.location_name,
        event.timezone,
        event.event_status,
        event.join_mode,
        event.participant_count,
        event.max_attendees,
        event.remaining_attendees,
        event.external_participation_url,
        event.anonymous_participation,
        event.is_online,
        event.comments_enabled,
        event.category,
        event.location_url,
        event.location_street,
        event.location_locality,
        event.location_region,
        event.location_country,
        event.location_postal_code,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// The sidecar row of one status, when it has one.
/// Generic over the executor — the authoring path reads it back inside its own
/// transaction.
pub async fn find<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<Option<StatusEvent>, DbError> {
    Ok(sqlx::query_as!(
        StatusEvent,
        "SELECT status_id, start_time, end_time, location_name, timezone, event_status,
                join_mode, participant_count, max_attendees, remaining_attendees,
                external_participation_url, anonymous_participation, is_online,
                comments_enabled, category, location_url, location_street,
                location_locality, location_region, location_country,
                location_postal_code
         FROM status_events WHERE status_id = $1",
        status_id,
    )
    .fetch_optional(executor)
    .await?)
}

/// The sidecar rows of a status batch, keyed by status id — for the
/// render-map builders.
pub async fn for_statuses<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<HashMap<i64, StatusEvent>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        StatusEvent,
        "SELECT status_id, start_time, end_time, location_name, timezone, event_status,
                join_mode, participant_count, max_attendees, remaining_attendees,
                external_participation_url, anonymous_participation, is_online,
                comments_enabled, category, location_url, location_street,
                location_locality, location_region, location_country,
                location_postal_code
         FROM status_events WHERE status_id = ANY($1)",
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.status_id, row)).collect())
}
