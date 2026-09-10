//! Poll federation: parsing inbound `Question` objects, recording federated
//! votes, and distributing tally updates for local polls.

use plamenu_ap::activity::{id_of, one_or_many};
use plamenu_db::account::{self, Account};
use plamenu_db::poll::{self, Poll};
use plamenu_db::status::Status;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::error::ApiError;

/// Option-count cap on inbound polls (Mastodon truncates at 500; nobody
/// legitimate is near either limit).
const MAX_REMOTE_OPTIONS: usize = 100;

/// The poll fields of an inbound `Question` object.
#[derive(Debug)]
pub struct ParsedQuestion {
    pub options: Vec<String>,
    pub tallies: Vec<i64>,
    pub multiple: bool,
    pub expires_at: Option<OffsetDateTime>,
    pub voters_count: Option<i64>,
}

/// Parses a `Question`'s poll fields the way Mastodon does: options from
/// `anyOf`/`oneOf` item names, tallies from their `replies.totalItems`,
/// expiry from `closed`/`endTime`. `None` when the object carries no
/// usable poll.
#[must_use]
pub fn parse_question(object: &Value) -> Option<ParsedQuestion> {
    if !crate::ingest::object_type_in(object, &["Question"]) {
        return None;
    }
    let any_of = one_or_many(object.get("anyOf"));
    let (items, multiple) = if any_of.is_empty() {
        (one_or_many(object.get("oneOf")), false)
    } else {
        (any_of, true)
    };
    let mut options = Vec::new();
    let mut tallies = Vec::new();
    for item in items.iter().take(MAX_REMOTE_OPTIONS) {
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .or_else(|| item.get("content").and_then(Value::as_str))
        else {
            continue;
        };
        options.push(name.to_owned());
        tallies.push(
            item.get("replies")
                .and_then(|r| r.get("totalItems"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        );
    }
    if options.is_empty() {
        return None;
    }
    let parse_time = |value: &Value| {
        value
            .as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    };
    // `closed` may be a timestamp or a bare flag; either ends the poll.
    let expires_at = match object.get("closed") {
        Some(closed @ Value::String(_)) => parse_time(closed).or(Some(OffsetDateTime::now_utc())),
        Some(Value::Bool(true)) => Some(OffsetDateTime::now_utc()),
        _ => object.get("endTime").and_then(parse_time),
    };
    Some(ParsedQuestion {
        options,
        tallies,
        multiple,
        expires_at,
        voters_count: object.get("votersCount").and_then(Value::as_i64),
    })
}

/// Stores the poll of a freshly-ingested remote `Question` status.
pub async fn store_remote_poll(
    state: &AppState,
    author: &Account,
    stored: &Status,
    object: &Value,
) -> Result<(), ApiError> {
    let Some(parsed) = parse_question(object) else {
        return Ok(());
    };
    poll::create(
        &state.pool,
        poll::NewPoll {
            status_id: stored.id,
            account_id: author.id,
            options: &parsed.options,
            cached_tallies: &parsed.tallies,
            multiple: parsed.multiple,
            hide_totals: false,
            voters_count: parsed.voters_count,
            expires_at: parsed.expires_at,
        },
    )
    .await?;
    Ok(())
}

/// Applies the poll state of an inbound `Update(Question)`: refreshed
/// tallies (the origin's are authoritative), and a vote reset when the
/// options themselves changed (the poll was effectively replaced). Creates
/// the poll row when the original `Create` predated poll support.
pub async fn refresh_remote_poll(
    state: &AppState,
    existing: &Status,
    object: &Value,
) -> Result<(), ApiError> {
    let Some(parsed) = parse_question(object) else {
        return Ok(());
    };
    let Some(current) = poll::find_by_status(&state.pool, existing.id).await? else {
        poll::create(
            &state.pool,
            poll::NewPoll {
                status_id: existing.id,
                account_id: existing.account_id,
                options: &parsed.options,
                cached_tallies: &parsed.tallies,
                multiple: parsed.multiple,
                hide_totals: false,
                voters_count: parsed.voters_count,
                expires_at: parsed.expires_at,
            },
        )
        .await?;
        return Ok(());
    };
    if current.options != parsed.options || current.multiple != parsed.multiple {
        poll::reset_votes(&state.pool, current.id).await?;
    }
    poll::apply_remote_update(
        &state.pool,
        current.id,
        poll::RemotePollUpdate {
            options: &parsed.options,
            cached_tallies: &parsed.tallies,
            multiple: parsed.multiple,
            voters_count: parsed.voters_count,
            expires_at: parsed.expires_at,
        },
    )
    .await?;
    Ok(())
}

/// Records an inbound `Create(Note)` that is actually a poll vote: a reply
/// to a local `Question` whose `name` is one of the options (Mastodon's
/// shape). Returns whether the object was consumed as a vote — votes never
/// become statuses.
pub async fn handle_inbound_vote(
    state: &AppState,
    sender: &Account,
    object: &Value,
) -> Result<bool, ApiError> {
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(parent_uri) = object.get("inReplyTo").and_then(id_of) else {
        return Ok(false);
    };
    let Some(parent) = crate::ingest::resolve_status_ref(state, parent_uri).await? else {
        return Ok(false);
    };
    // Only votes on our own polls count; remote polls tally at their origin.
    if parent.uri.is_some() {
        return Ok(false);
    }
    let Some(poll) = poll::find_by_status(&state.pool, parent.id).await? else {
        return Ok(false);
    };
    let Some(choice) = poll.options.iter().position(|option| option == name) else {
        return Ok(false);
    };
    // It addresses a poll, so it is a vote — but late votes don't count.
    if poll.expired() || poll.account_id == sender.id {
        return Ok(true);
    }
    let choice = i32::try_from(choice).map_err(|e| ApiError::Internal(Box::new(e)))?;
    let inserted =
        poll::insert_vote(&state.pool, poll.id, sender.id, choice, id_of(object)).await?;
    if inserted.is_none() {
        return Ok(true); // redelivery
    }
    let refreshed = poll::refresh_local_tallies(&state.pool, poll.id).await?;
    tracing::info!(voter = %sender.username, poll = poll.id, choice, "federated vote recorded");
    if !refreshed.hide_totals {
        distribute_poll_update(state, &parent, &refreshed).await?;
    }
    Ok(true)
}

/// Fans out an `Update(Question)` with the current tallies to the author's
/// followers, so remote servers see votes as they arrive. (Mastodon batches
/// these on a 3-minute delay; we send immediately — poll volume is low.)
pub async fn distribute_poll_update(
    state: &AppState,
    status: &Status,
    poll: &Poll,
) -> Result<(), ApiError> {
    let Some(author) = account::find_by_id(&state.pool, status.account_id).await? else {
        return Ok(());
    };
    if !author.is_local() {
        return Ok(());
    }
    let note = crate::note::note_for_status(state, status, &author).await?;
    let updated = poll
        .updated_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let update =
        plamenu_ap::activity::update_note(&state.config.domain, &author.username, note, &updated);
    crate::actions::fan_out(state, &author, &update, &[]).await?;
    Ok(())
}

/// Loads a poll and enforces its status' visibility for `viewer` — an
/// invisible poll is a 404, like Mastodon's `authorize @poll.status, :show?`.
pub async fn visible_poll(
    state: &AppState,
    poll_id: i64,
    viewer: Option<i64>,
) -> Result<(Poll, Status), ApiError> {
    let item = plamenu_db::poll::find_by_id(&state.pool, poll_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stored = plamenu_db::status::find_by_id(&state.pool, item.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::entities::can_view(&state.pool, &stored, viewer).await? {
        return Err(ApiError::NotFound);
    }
    Ok((item, stored))
}

/// Validated option indices for a vote, deduplicated and order-preserving.
pub fn validated_choices(item: &Poll, raw: &[Value]) -> Result<Vec<i32>, ApiError> {
    let invalid = || ApiError::Unprocessable("The chosen vote option does not exist".into());
    let mut choices: Vec<i32> = Vec::with_capacity(raw.len());
    for value in raw {
        let index = match value {
            Value::String(s) => s.parse::<i64>().map_err(|_| invalid())?,
            Value::Number(n) => n.as_i64().ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
        let count = i64::try_from(item.options.len()).unwrap_or(0);
        if !(0..count).contains(&index) {
            return Err(invalid());
        }
        let index = i32::try_from(index).map_err(|_| invalid())?;
        if !choices.contains(&index) {
            choices.push(index);
        }
    }
    Ok(choices)
}

/// Records `voter`'s choices on a poll and, for remote polls, federates each
/// vote to the author; returns the resulting poll. The single source of truth
/// for both the API (`/api/v1/polls/{id}/votes`) and the web UI vote form.
pub async fn cast_vote(
    state: &AppState,
    voter: &Account,
    poll_id: i64,
    raw: &[Value],
) -> Result<Poll, ApiError> {
    let (item, stored) = visible_poll(state, poll_id, Some(voter.id)).await?;
    if raw.is_empty() {
        return Err(ApiError::BadRequest("Param choices is missing".into()));
    }
    // Mastodon's VoteValidator, in its order.
    if item.expired() {
        return Err(ApiError::Unprocessable("The poll has already ended".into()));
    }
    let choices = validated_choices(&item, raw)?;
    if item.account_id == voter.id {
        return Err(ApiError::Unprocessable(
            "You cannot vote in your own polls".into(),
        ));
    }
    let already_voted = !poll::votes_by(&state.pool, item.id, voter.id)
        .await?
        .is_empty();
    if already_voted || (!item.multiple && choices.len() > 1) {
        return Err(ApiError::Unprocessable(
            "You have already voted on this poll".into(),
        ));
    }

    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut vote_ids = Vec::with_capacity(choices.len());
    for choice in &choices {
        if let Some(vote_id) =
            poll::insert_vote(&state.pool, item.id, voter.id, *choice, None).await?
        {
            vote_ids.push((vote_id, *choice));
        }
    }

    if author.is_local() {
        let refreshed = poll::refresh_local_tallies(&state.pool, item.id).await?;
        if !refreshed.hide_totals {
            distribute_poll_update(state, &stored, &refreshed).await?;
        }
        Ok(refreshed)
    } else {
        // The origin tallies the vote; our copy updates when its
        // Update(Question) arrives.
        let poll_uri =
            crate::entities::status_uri_for_account(&state.config.domain, &stored, &author);
        let author_uri = author.uri.clone().ok_or(ApiError::NotFound)?;
        for (vote_id, choice) in &vote_ids {
            let option_name = &item.options[usize::try_from(*choice).unwrap_or_default()];
            let activity = plamenu_ap::activity::create_vote(
                &state.config.domain,
                &voter.username,
                *vote_id,
                option_name,
                &poll_uri,
                &author_uri,
            );
            plamenu_db::job::enqueue(&state.pool, voter.id, &author.inbox_url, &activity).await?;
        }
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_mastodon_question_shapes() {
        let object = json!({
            "id": "https://remote.example/users/bob/statuses/1",
            "type": "Question",
            "oneOf": [
                {"type": "Note", "name": "yes",
                 "replies": {"type": "Collection", "totalItems": 3}},
                {"type": "Note", "name": "no",
                 "replies": {"type": "Collection", "totalItems": 4}},
            ],
            "endTime": "2026-06-12T00:00:00Z",
            "votersCount": 7,
        });
        let parsed = parse_question(&object).unwrap();
        assert_eq!(parsed.options, ["yes", "no"]);
        assert_eq!(parsed.tallies, [3, 4]);
        assert!(!parsed.multiple);
        assert_eq!(parsed.voters_count, Some(7));
        assert_eq!(
            parsed.expires_at.unwrap().unix_timestamp(),
            OffsetDateTime::parse("2026-06-12T00:00:00Z", &Rfc3339)
                .unwrap()
                .unix_timestamp()
        );

        // anyOf marks multiple choice; missing tallies default to zero.
        let object = json!({
            "type": "Question",
            "anyOf": [
                {"type": "Note", "name": "a"},
                {"type": "Note", "name": "b"},
            ],
            "closed": "2026-06-01T00:00:00Z",
        });
        let parsed = parse_question(&object).unwrap();
        assert!(parsed.multiple);
        assert_eq!(parsed.tallies, [0, 0]);
        assert!(parsed.expires_at.is_some());

        // Plain notes and option-less questions parse to nothing.
        assert!(parse_question(&json!({"type": "Note"})).is_none());
        assert!(parse_question(&json!({"type": "Question", "oneOf": []})).is_none());
    }
}
