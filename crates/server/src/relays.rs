//! `ActivityPub` relay subscription and traffic — Mastodon's `Relay` model
//! behavior. Subscribing sends `Follow(as:Public)` from the *instance actor*
//! (Mastodon's representative) to the relay inbox; the relay answers
//! `Accept`/`Reject` echoing our Follow's id. Once accepted:
//!
//! - public local activity additionally fans out to the relay inbox
//!   ([`enabled_inboxes_for_public`], Mastodon's `StatusReachFinder#relay_inboxes`),
//! - an `Announce` *from* the relay is not a boost but a delivery hint — fetch and ingest the
//!   announced object ([`enabled_relay_for_sender`], Mastodon's `requested_through_relay?`).

use plamenu_ap::activity::PUBLIC;
use plamenu_ap::urls::InstanceActorUrls;
use plamenu_db::account::Account;
use plamenu_db::{account, id, job, relay, status};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiError;
use crate::remote::{host_of, refresh_remote_actor};

/// The relay-actor inbox paths tried when an admin gives a bare instance
/// address, in order.
///
/// `/relay` is Mobilizon's instance relay actor — the only channel that carries
/// its **person-organized** events, since a Mobilizon Person refuses to be
/// followed (`:person_no_follow`) and only group-attributed events reach a
/// follower otherwise. `/actor` and `/inbox` cover the Mastodon-family relay
/// software an admin is more likely to have met.
const RELAY_ACTOR_PATHS: &[&str] = &["/relay", "/actor", "/inbox"];

/// Resolves what an admin typed into a relay inbox URL.
///
/// A full inbox URL is taken as given — that is the Mastodon-compatible input and
/// must keep working. A bare instance address (`mobilizon.local`,
/// `https://mobilizon.local`) is *resolved*: each candidate in
/// [`RELAY_ACTOR_PATHS`] is fetched as an actor and the first one that answers
/// contributes its own `inbox`.
///
/// This exists because "follow this instance" is the actual admin intent, and
/// requiring the operator to know that Mobilizon's relay actor lives at `/relay`
/// — and that its inbox is a different URL again — turns a one-line decision into
/// a research task.
pub async fn resolve_relay_inbox(
    state: &AppState,
    input: &str,
) -> Result<(String, Option<String>), ApiError> {
    let input = input.trim().trim_end_matches('/');
    let candidate_origin = if input.starts_with("https://") {
        // A path of its own means the admin named an inbox (or an actor) exactly.
        let parsed =
            url::Url::parse(input).map_err(|_| ApiError::BadRequest("invalid relay URL".into()))?;
        if parsed.path().trim_end_matches('/').is_empty() {
            input.to_owned()
        } else {
            // Named exactly: it may be an inbox or an actor. Fetching tells us
            // which, and an actor contributes the identity that makes the
            // sender check collision-free.
            return Ok(match state.federation.fetch_actor(input).await {
                Ok(actor) if !actor.inbox.is_empty() => (actor.inbox, Some(actor.id)),
                _ => (input.to_owned(), None),
            });
        }
    } else if input.contains('/') || input.is_empty() {
        return Err(ApiError::BadRequest("invalid relay address".into()));
    } else {
        format!("https://{input}")
    };

    for path in RELAY_ACTOR_PATHS {
        let candidate = format!("{candidate_origin}{path}");
        if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &candidate)
            .await?
        {
            continue;
        }
        if let Ok(actor) = state.federation.fetch_actor(&candidate).await
            && !actor.inbox.is_empty()
        {
            return Ok((actor.inbox, Some(actor.id)));
        }
    }
    Err(ApiError::Unprocessable(
        "Validation failed: no relay actor found at that address".into(),
    ))
}

/// Subscribes to the relay: `pending` + `Follow(as:Public)` signed by the
/// instance actor (Mastodon's `Relay#enable!`). Returns `false` for an
/// unknown relay id.
pub async fn enable(state: &AppState, relay_id: i64) -> Result<bool, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result = enable_conn(state, &mut tx, relay_id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(result)
}

async fn enable_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    relay_id: i64,
) -> Result<bool, ApiError> {
    let Some(relay) = relay::find(&mut *conn, relay_id).await? else {
        return Ok(false);
    };
    // Mastodon mints a fresh opaque URI per handshake
    // (`TagManager#generate_uri_for`); ours is keyed by a snowflake.
    let activity_id = format!("https://{}/payloads/{}", state.config.domain, id::next());
    let follow = follow_activity(&state.config.domain, &activity_id);
    relay::mark_pending(&mut *conn, relay.id, &activity_id).await?;
    job::enqueue_from_instance_tx(&mut *conn, &relay.inbox_url, &follow).await?;
    Ok(true)
}

/// Unsubscribes: `idle` + `Undo(Follow)` referencing the accepted handshake
/// (Mastodon's `Relay#disable!`). A relay that never got its Follow out
/// (no stored activity id) is just reset.
pub async fn disable(state: &AppState, relay_id: i64) -> Result<bool, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result = disable_conn(state, &mut tx, relay_id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(result)
}

async fn disable_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    relay_id: i64,
) -> Result<bool, ApiError> {
    let Some(relay) = relay::find(&mut *conn, relay_id).await? else {
        return Ok(false);
    };
    if let Some(follow_activity_id) = relay.follow_activity_id.as_deref() {
        let domain = &state.config.domain;
        let undo_id = format!("https://{domain}/payloads/{}", id::next());
        let undo = json!({
            "@context": plamenu_ap::AS_CONTEXT,
            "id": undo_id,
            "type": "Undo",
            "actor": InstanceActorUrls::new(domain).id,
            "object": follow_activity(domain, follow_activity_id),
        });
        job::enqueue_from_instance_tx(&mut *conn, &relay.inbox_url, &undo).await?;
    }
    relay::mark_idle(&mut *conn, relay.id).await?;
    Ok(true)
}

/// Removes the relay entirely, unsubscribing first when it is active
/// (Mastodon's `before_destroy :ensure_disabled`).
pub async fn remove(state: &AppState, relay_id: i64) -> Result<bool, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result = remove_conn(state, &mut tx, relay_id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(result)
}

async fn remove_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    relay_id: i64,
) -> Result<bool, ApiError> {
    let Some(relay) = relay::find(&mut *conn, relay_id).await? else {
        return Ok(false);
    };
    if relay.state == "accepted" || relay.state == "pending" {
        disable_conn(state, conn, relay.id).await?;
    }
    Ok(relay::delete(&mut *conn, relay.id).await?)
}

fn follow_activity(domain: &str, activity_id: &str) -> Value {
    json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": activity_id,
        "type": "Follow",
        "actor": InstanceActorUrls::new(domain).id,
        "object": PUBLIC,
    })
}

/// Accepted relay inboxes when `visibility` reaches the public firehose,
/// otherwise nothing — the extra fan-out targets of a public status'
/// `Create`/`Update`/`Delete`/`Announce`.
pub async fn enabled_inboxes_for_public(
    state: &AppState,
    visibility: &str,
) -> Result<Vec<String>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    enabled_inboxes_for_public_conn(state, &mut conn, visibility).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn enabled_inboxes_for_public_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    visibility: &str,
) -> Result<Vec<String>, ApiError> {
    if visibility != "public" {
        return Ok(Vec::new());
    }
    Ok(relay::enabled_inboxes(&mut *conn).await?)
}

/// The accepted relay `sender` acts for. `None` when the sender is not a
/// subscribed relay; the id lets the inbox attribute the delivery to the relay's
/// activity counter.
///
/// Matched on the relay's **actor URI**, not on an inbox URL. Mastodon matches
/// `Relay.find_by(inbox_url: @account.inbox_url)`, which is safe for dedicated
/// relay software — but a general-purpose server acting as a relay advertises the
/// instance's *shared* inbox for its relay actor, and Mobilizon does exactly
/// that (`/relay` with inbox `/inbox`). Matching on that inbox made every group
/// and person on the host look like the relay, and since a relay's `Announce` is
/// a delivery hint rather than a boost, subscribing silently stopped that
/// server's group events from reaching any follower's timeline.
///
/// Rows predating migration 0028 have no actor URI and fall back to the sender's
/// **own** inbox — never the shared one, which is what caused the collision.
pub async fn enabled_relay_for_sender(
    state: &AppState,
    sender: &Account,
) -> Result<Option<i64>, ApiError> {
    Ok(relay::enabled_id_for_sender(&state.pool, sender.uri.as_deref(), &sender.inbox_url).await?)
}

/// Ingests the object a relay `Announce`d: a relayed announce is a delivery
/// hint ("here is a public post you haven't seen"), never a boost — Mastodon
/// fetches the object and returns without creating a reblog
/// (`requested_through_relay?`). Best-effort: unknown hosts, blocked domains
/// and non-note objects are silently skipped.
pub async fn ingest_relayed_object(state: &AppState, uri: &str) -> Result<(), ApiError> {
    let domain = &state.config.domain;
    // Our own posts, and anything already stored, need no fetch.
    if uri.starts_with(&format!("https://{domain}/"))
        || status::find_by_uri(&state.pool, uri).await?.is_some()
        || !crate::instance_policy::can_federate_url(&state.pool, domain, uri).await?
    {
        return Ok(());
    }
    let Ok(object) = state.federation.fetch_object_following(uri).await else {
        return Ok(());
    };
    let canonical = object.get("id").and_then(Value::as_str).unwrap_or(uri);
    if canonical != uri && status::find_by_uri(&state.pool, canonical).await?.is_some() {
        return Ok(());
    }
    if !crate::ingest::is_ingestible_note(&object) {
        return Ok(());
    }
    let Some(attributed_to) = plamenu_ap::activity::attributed_to_id(&object) else {
        return Ok(());
    };
    // The claimed author must live on the note's own host.
    if host_of(attributed_to) != host_of(canonical) {
        return Ok(());
    }
    let author = if let Some(known) = account::find_by_uri(&state.pool, attributed_to).await? {
        if !crate::instance_policy::account_visible(&state.pool, domain, &known).await? {
            return Ok(());
        }
        known
    } else {
        if !crate::instance_policy::can_federate_url(&state.pool, domain, attributed_to).await? {
            return Ok(());
        }
        let Ok(actor) = state.federation.fetch_actor(attributed_to).await else {
            return Ok(());
        };
        refresh_remote_actor(state, &actor).await?
    };
    crate::ingest::ingest_remote_note(state, &author, &object).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_follow_is_addressed_to_the_public_collection() {
        let follow = follow_activity("plamenu.example", "https://plamenu.example/payloads/7");
        assert_eq!(follow["type"], "Follow");
        assert_eq!(follow["actor"], "https://plamenu.example/actor");
        assert_eq!(follow["object"], PUBLIC);
        assert_eq!(follow["id"], "https://plamenu.example/payloads/7");
    }
}
