//! `ActivityPub` inboxes: signature verification and activity processing.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use plamenu_ap::activity::{Activity, accept_follow, attributed_to_ids, id_of, one_or_many};
use plamenu_ap::actor::RemoteActor;
use plamenu_ap::urls;
use plamenu_db::account::{self, Account};
use plamenu_db::{dislike, favourite, follow, job, notification, report, status, webhook};
use plamenu_federation::{PreparedRequestAuth, RequestFacts};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::error::ApiError;
use crate::ingest::{
    fetch_remote_status_value, is_ingestible_note, resolve_status_ref, signed_fetch_recipient,
    update_remote_note,
};
use crate::remote::{host_of, refresh_remote_actor};

// Keep the dispatch body available at the signed HTTP inbox without adding
// another async poll frame to its already deep verification/ingestion stack.
// The callable wrapper below lets the authenticated portable gateway reuse the
// exact same logic. This is a macro rather than a second implementation so the
// two authenticated entry points cannot drift.
macro_rules! dispatch_authenticated_activity {
    ($state:expr, $sender:expr, $activity:expr, $raw:expr, $delivered_to_account_id:expr) => {{
        'dispatch: {
            let state = $state;
            let sender = $sender;
            let activity = $activity;
            let raw = $raw;
            let delivered_to_account_id = $delivered_to_account_id;
            let actor_id = activity.actor_id().unwrap_or_default();

            // Keep the small recovery/control subset Mastodon accepts from a
            // suspended actor. In particular, Update can carry a remote
            // restoration and Delete must still make suspension permanent.
            if sender.suspended()
                && !matches!(
                    activity.kind.as_str(),
                    "Delete" | "Reject" | "Undo" | "Update"
                )
            {
                tracing::debug!(kind = %activity.kind, actor = %actor_id, "ignoring activity from suspended actor");
                break 'dispatch Ok(StatusCode::ACCEPTED);
            }

            tracing::debug!(kind = %activity.kind, actor = %actor_id, "dispatching authenticated activity");

            if crate::webxdc_realtime::handle(state, sender, raw).await? {
                break 'dispatch Ok(StatusCode::ACCEPTED);
            }

            let webxdc_handled = match activity.kind.as_str() {
                "Follow" => crate::webxdc::handle_follow(state, sender, raw).await?,
                "Accept" => crate::webxdc::handle_accept(state, sender, raw).await?,
                "Reject" => crate::webxdc::handle_reject(state, sender, raw).await?,
                "Create" => crate::webxdc::handle_create(state, sender, raw).await?,
                // `Announce` is also the entry point for nested FEP-1b12
                // community activities. Keep the unrelated Webxdc probe out
                // of that already deep future before falling through below.
                "Announce" if raw.get("webxdcSerial").is_some() => {
                    Box::pin(crate::webxdc::handle_announce(state, sender, raw)).await?
                }
                "Announce" => false,
                "Undo" => crate::webxdc::handle_undo(state, sender, raw).await?,
                "Remove" => crate::webxdc::handle_remove(state, sender, raw).await?,
                "Update" => crate::webxdc::handle_update(state, sender, raw).await?,
                "Delete" => crate::webxdc::handle_delete(state, sender, raw).await?,
                _ => false,
            };
            if webxdc_handled {
                break 'dispatch Ok(StatusCode::ACCEPTED);
            }

            match activity.kind.as_str() {
                "Follow" => handle_follow(state, sender, activity, raw).await?,
                "Undo" => handle_undo(state, sender, activity, raw).await?,
                "Create" => {
                    handle_create(state, sender, activity, raw, delivered_to_account_id).await?
                }
                "Update" => handle_update(state, sender, activity, raw).await?,
                "Delete" => handle_delete(state, sender, activity, raw).await?,
                "Like" => handle_like(state, sender, activity, raw).await?,
                "EmojiReact" | "EmojiReaction" => {
                    handle_emoji_react(state, sender, activity, raw).await?;
                }
                "Dislike" => handle_dislike(state, sender, activity, raw).await?,
                "Announce" => Box::pin(handle_announce(state, sender, activity)).await?,
                "Block" => handle_block(state, sender, activity, raw).await?,
                "Lock" => handle_group_lock(state, sender, activity, raw).await?,
                "Flag" => handle_flag(state, sender, activity, raw).await?,
                "QuoteRequest" => handle_quote_request(state, sender, activity, raw).await?,
                "FeatureRequest" => handle_feature_request(state, sender, activity, raw).await?,
                "Add" => {
                    Box::pin(handle_featured_change(state, sender, activity, raw, true)).await?;
                }
                "Remove" => {
                    Box::pin(handle_featured_change(state, sender, activity, raw, false)).await?;
                }
                "Move" => handle_move(state, sender, activity, raw).await?,
                "Join" => handle_join(state, sender, activity, raw).await?,
                "Leave" => handle_leave(state, sender, activity).await?,
                "Invite" => handle_invite(state, sender, activity, raw).await?,
                "Accept" => handle_response(state, sender, activity, raw, true).await?,
                "Reject" => handle_response(state, sender, activity, raw, false).await?,
                kind => {
                    tracing::debug!(kind, actor = %actor_id, "ignoring unhandled activity type");
                }
            }
            break 'dispatch Ok(StatusCode::ACCEPTED);
        }
    }};
}

/// Shared inbox: `POST /inbox`.
pub async fn shared_inbox(
    state: State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    Box::pin(process(state, uri, headers, body, None)).await
}

/// Per-user inbox: `POST /users/{username}/inbox`. Processing is identical to
/// the shared inbox, but unknown users are a 404 like Mastodon's.
pub async fn user_inbox(
    state: State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let recipient = super::local_actor_account(&state, &username, &uri).await?;
    let recipient_id = recipient.id;
    Box::pin(process(state, uri, headers, body, Some(recipient_id))).await
}

#[allow(clippy::too_many_lines)] // the inbox activity dispatch match
async fn process(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    delivered_to_account_id: Option<i64>,
) -> Result<StatusCode, ApiError> {
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("body is not valid JSON".into()))?;
    let activity: Activity = serde_json::from_value(raw.clone())
        .map_err(|_| ApiError::BadRequest("body is not an activity".into()))?;

    // `Delete` for an actor we never saw: answer 202 without dereferencing.
    // Mastodon fans these out widely on account deletion; fetching the (now
    // gone) actor for every one of them would be a self-inflicted fetch storm.
    if activity.kind == "Delete"
        && let Some(actor_id) = activity.actor_id()
        && activity.object_id() == Some(actor_id)
        && account::find_by_uri(&state.pool, actor_id).await?.is_none()
    {
        return Ok(StatusCode::ACCEPTED);
    }

    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |pq| pq.as_str());
    // RFC 9421 senders sign the absolute `@target-uri`; reconstruct it from
    // our own domain (never the spoofable Host header).
    let target_uri = format!("https://{}{path_and_query}", state.config.domain);
    let prepared = PreparedRequestAuth::from_post_request(
        &RequestFacts {
            method: "POST",
            target_uri: &target_uri,
            path_and_query,
            headers: &headers,
        },
        &body,
        SystemTime::now(),
    )
    .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

    // Most activities name their `actor`; a FEP-7aa9 `FeatureRequest` carries
    // none, so the sender is the signature's key owner (the keyId without its
    // fragment), like Mastodon's `signed_request_account`.
    let actor_id = match activity.actor_id() {
        Some(id) => id.to_owned(),
        None => prepared
            .key_id()
            .split('#')
            .next()
            .filter(|base| !base.is_empty())
            .ok_or_else(|| ApiError::BadRequest("activity has no actor".into()))?
            .to_owned(),
    };

    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &actor_id)
        .await?
    {
        tracing::info!(actor = %actor_id, "ignoring activity from blocked or non-allowed domain");
        return Ok(StatusCode::ACCEPTED);
    }
    // The delivering server (the signature's key host) must be federable too;
    // for a direct delivery this is the actor's own domain again.
    if !crate::instance_policy::can_federate_url(
        &state.pool,
        &state.config.domain,
        prepared.key_id(),
    )
    .await?
    {
        tracing::info!(key = %prepared.key_id(), "ignoring delivery signed by blocked or non-allowed domain");
        return Ok(StatusCode::ACCEPTED);
    }

    let signer = if crate::webxdc_realtime::is_packet(&raw) {
        let cached = crate::remote::find_cached_signer(&state.pool, prepared.key_id())
            .await?
            .ok_or_else(|| ApiError::Unauthorized("unknown ephemeral signer".into()))?;
        crate::remote::prepared_matches_account(&state.pool, &prepared, &cached)
            .await?
            .map_err(|_| ApiError::Unauthorized("invalid ephemeral signature".into()))?;
        cached
    } else {
        verified_signer(&state, &prepared, &actor_id).await?
    };

    // Ephemeral traffic is authenticated directly from already-known keys.
    // Do not run bookkeeping, forwarding/refetch or persistence side effects.
    if crate::webxdc_realtime::is_packet(&raw) {
        if signer.uri.as_deref() != Some(actor_id.as_str()) {
            return Err(ApiError::Unauthorized(
                "ephemeral delivery must be signed by its actor".into(),
            ));
        }
        crate::webxdc_realtime::handle(&state, &signer, &raw).await?;
        return Ok(StatusCode::ACCEPTED);
    }

    // Positive RFC 9421 signal: a peer that signs us a *verified* RFC 9421
    // request demonstrably speaks the dialect, so record it for the signer's
    // host (the delivering server, correct even for forwarded/relay traffic).
    // This is the ONLY way a host earns RFC 9421 emission — an outbound `200`
    // is not proof (upstream Pleroma 200s then async-drops a 9421 inbox POST).
    // Best-effort: never fail the inbox on a preference write.
    if prepared.is_rfc9421()
        && let Some(host) = crate::link_preview::url_host(prepared.key_id())
        && let Err(error) = plamenu_db::signature_prefs::record(&state.pool, &host, true).await
    {
        tracing::warn!(%error, host, "failed to record inbound RFC 9421 signature preference");
    }

    crate::followers_sync::process_inbound_header(&state, &signer, &headers).await?;

    // A delivery HTTP-signed by a subscribed relay on someone else's behalf
    // (LitePub-style forwarding of the origin's own activity) is relay
    // traffic: count it here, where the signer is known — the dispatch paths
    // below only see the origin author as the actor. Disjoint from the count
    // in `handle_announce`, where the relay actor is the activity's own actor.
    if signer.uri.as_deref() != Some(actor_id.as_str())
        && let Some(relay_id) = crate::relays::enabled_relay_for_sender(&state, &signer).await?
    {
        plamenu_db::relay::record_activity(&state.pool, relay_id).await?;
    }

    // A forwarded copy: validly signed, but by a different server than the
    // activity's actor. An FEP-8b32 integrity proof bound to the actor's own
    // Ed25519 key authenticates the payload wholesale — the activity then
    // proceeds as if delivered directly. Without one (or with a dud), only
    // what can be confirmed against the origin is acted on (M31's re-fetch,
    // Mastodon's equivalent fallback when a copy carries no LDS).
    let sender = if signer.uri.as_deref() == Some(actor_id.as_str()) {
        signer
    } else if let Some(proven) =
        Box::pin(proof_verified_actor(&state, &raw, &activity, &actor_id)).await?
    {
        proven
    } else {
        Box::pin(handle_forwarded(&state, &activity, &actor_id)).await?;
        return Ok(StatusCode::ACCEPTED);
    };

    dispatch_authenticated_activity!(&state, &sender, &activity, &raw, delivered_to_account_id)
}

/// Projects an already-authenticated activity through Plamenu's ordinary
/// social-data pipeline. The HTTP inbox verifies signatures before calling
/// this; the portable gateway verifies the client's did:key proof (outbox) or
/// HTTP signature (inbox) before using the same path.
pub(crate) async fn dispatch_authenticated(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<StatusCode, ApiError> {
    dispatch_authenticated_activity!(state, sender, activity, raw, None)
}

/// Inbound `Like`: a favourite from a remote account — unless it carries a
/// Misskey-family reaction (`_misskey_reaction`, with `content`/`name`
/// fallbacks), which is an emoji reaction wearing a `Like` type: Sharkey
/// sends *every* reaction that way, including its default one, to peers it
/// doesn't recognise as Mastodon-family. A reaction that doesn't parse (too
/// long to be an emoji, bad shortcode) degrades to a plain favourite rather
/// than being dropped.
async fn handle_like(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };
    let Some(target) = resolve_status_ref(state, object_uri).await? else {
        return Ok(()); // a Like of something we don't know
    };
    if let Some((name, custom_emoji_url)) = parse_reaction(state, sender, raw).await? {
        return store_reaction(
            state,
            sender,
            &target,
            &name,
            custom_emoji_url.as_deref(),
            activity.id.as_deref(),
        )
        .await;
    }
    // Group votes: on a group post a Like is an upvote — outcasts drop at the
    // gate, the sender's downvote is displaced (mutually exclusive, Lemmy's
    // model) and local groups relay the vote to their followers.
    let group_post = crate::groups::is_group_post(state, target.id).await?;
    if group_post && !crate::groups::may_vote(state, sender.id, target.id).await? {
        return Ok(());
    }
    let marker =
        favourite::create(&state.pool, sender.id, target.id, activity.id.as_deref()).await?;
    // A redelivered Like (the favourite already existed) must not re-notify.
    if !marker.inserted {
        return Ok(());
    }
    if group_post {
        dislike::delete(&state.pool, sender.id, target.id).await?;
        crate::groups::announce_vote(state, &target, raw).await?;
    }
    let author = account::find_by_id(&state.pool, target.account_id).await?;
    if let Some(author) = author.filter(Account::is_local) {
        notification::create(
            &state.pool,
            author.id,
            sender.id,
            "favourite",
            Some(target.id),
        )
        .await?;
    }
    Ok(())
}

/// Inbound `EmojiReact` (Pleroma/litepub) or its legacy Misskey-family alias
/// `EmojiReaction`: an emoji reaction on a status we know. The reaction
/// `content` is a Unicode emoji or a `:shortcode:`; a shortcode resolves its
/// image from the activity's `Emoji` tag (also cached so future renders can
/// find it). Mirrors [`handle_like`]: stored whoever the author is, with a
/// notification only when the author is local.
async fn handle_emoji_react(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };
    let Some(target) = resolve_status_ref(state, object_uri).await? else {
        return Ok(()); // a reaction to something we don't know
    };
    let Some((name, custom_emoji_url)) = parse_reaction(state, sender, raw).await? else {
        return Ok(());
    };
    store_reaction(
        state,
        sender,
        &target,
        &name,
        custom_emoji_url.as_deref(),
        activity.id.as_deref(),
    )
    .await
}

/// Stores a parsed inbound reaction — shared by `EmojiReact`/`EmojiReaction`
/// and the Misskey Like-reaction dialect. The activity id lands in the row's
/// `uri` so any `Undo` shape naming it retracts the same reaction.
async fn store_reaction(
    state: &AppState,
    sender: &Account,
    target: &plamenu_db::status::Status,
    name: &str,
    custom_emoji_url: Option<&str>,
    activity_id: Option<&str>,
) -> Result<(), ApiError> {
    let data = plamenu_db::reaction::NewReaction {
        account_id: sender.id,
        status_id: target.id,
        name,
        custom_emoji_url,
        uri: activity_id,
    };
    let managed = if let Some(custom_emoji_url) = custom_emoji_url {
        if let Some(file_name) =
            crate::emoji::local_media_file_name(&state.config.domain, custom_emoji_url)
        {
            plamenu_db::custom_emoji::find_managed_by_file_name(&state.pool, file_name).await?
        } else if let Some(domain) = sender.domain.as_deref() {
            let rows =
                plamenu_db::custom_emoji::lookup(&state.pool, &[name.to_owned()], Some(domain))
                    .await?;
            let ids: Vec<i64> = rows.iter().map(|emoji| emoji.id).collect();
            plamenu_db::custom_emoji::find_managed_by_ids(&state.pool, &ids)
                .await?
                .into_iter()
                .next()
        } else {
            None
        }
    } else {
        None
    };
    let marker = if let Some(emoji) = &managed {
        plamenu_db::reaction::create_custom(&state.pool, data, emoji.id, emoji.origin_id).await?
    } else {
        plamenu_db::reaction::create(&state.pool, data).await?
    };
    // A redelivered reaction (the row already existed) must not re-notify or
    // re-push the status — nothing visible changed.
    if !marker.inserted {
        return Ok(());
    }
    if matches!(target.visibility.as_str(), "public" | "unlisted" | "local")
        && let Some(emoji) = managed
    {
        plamenu_db::custom_emoji::record_reaction_usage(&state.pool, sender.id, emoji.origin_id)
            .await?;
    }
    if let Some(author) = account::find_by_id(&state.pool, target.account_id)
        .await?
        .filter(Account::is_local)
    {
        // The notification carries the displayable reaction (`:shortcode:`
        // for custom emoji, like Pleroma's `emoji` field).
        let display = display_emoji(name, custom_emoji_url.is_some());
        notification::create_reaction(&state.pool, author.id, sender.id, target.id, &display)
            .await?;
    }
    // Re-push the status so connected clients refresh its reaction chips
    // (Pleroma streams reaction changes as `status.update`).
    crate::streaming::status_edited(state, target.id).await;
    Ok(())
}

/// Inbound `Dislike`, discriminated by context: on a group post it
/// is a downvote — stored, mutually exclusive with the sender's upvote, and
/// relayed by local groups. Everywhere else the Misskey-family legacy
/// meaning is kept: an idempotent withdrawal of the sender's reactions and
/// favourite on the target (whichever the original arrived as).
async fn handle_dislike(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };
    let Some(target) = resolve_status_ref(state, object_uri).await? else {
        return Ok(()); // a vote or withdrawal on something we don't know
    };
    // The group claim: local attribution rows / a group's boost — or the
    // activity naming a known group in `audience`/`to`/`cc` (Lemmy stamps
    // votes with the community).
    if crate::groups::is_group_post(state, target.id).await?
        || names_group_audience(state, raw).await?
    {
        if !crate::groups::may_vote(state, sender.id, target.id).await? {
            return Ok(());
        }
        let marker =
            dislike::create(&state.pool, sender.id, target.id, activity.id.as_deref()).await?;
        // A redelivered Dislike must not re-announce.
        if !marker.inserted {
            return Ok(());
        }
        // The downvote displaces the sender's upvote (Lemmy's mutual
        // exclusion) — and takes the favourite notification with it.
        favourite::delete(&state.pool, sender.id, target.id).await?;
        notification::clear_kind_for_status(
            &state.pool,
            target.account_id,
            sender.id,
            "favourite",
            target.id,
        )
        .await?;
        crate::groups::announce_vote(state, &target, raw).await?;
        return Ok(());
    }
    favourite::delete(&state.pool, sender.id, target.id).await?;
    notification::clear_kind_for_status(
        &state.pool,
        target.account_id,
        sender.id,
        "favourite",
        target.id,
    )
    .await?;
    let removed = plamenu_db::reaction::delete_all_for(&state.pool, sender.id, target.id).await?;
    finish_reactions_removed(state, sender, &target, &removed).await?;
    Ok(())
}

/// [`finish_reaction_removal`] for every reaction withdrawn from one status at
/// once: the status and its author are in hand, so the whole batch costs one
/// author lookup, one notification sweep and one streaming event instead of
/// four statements per reaction.
async fn finish_reactions_removed(
    state: &AppState,
    sender: &Account,
    target: &plamenu_db::status::Status,
    removed: &[plamenu_db::reaction::RemovedReaction],
) -> Result<(), ApiError> {
    if removed.is_empty() {
        return Ok(());
    }
    let author = account::find_by_id(&state.pool, target.account_id).await?;
    if let Some(author) = author.filter(Account::is_local) {
        let displays: Vec<String> = removed
            .iter()
            .map(|reaction| display_emoji(&reaction.name, reaction.custom_emoji_url.is_some()))
            .collect();
        notification::clear_reactions(&state.pool, author.id, sender.id, target.id, &displays)
            .await?;
    }
    crate::streaming::status_edited(state, target.id).await;
    Ok(())
}

/// Whether the raw activity names a known group actor in `audience`, `to` or
/// `cc` — FEP-1b12's community claim, the fallback vote discrimination for a
/// `Dislike` whose target has no local attribution rows yet. The whole
/// addressing set (deduplicated — `to` and `cc` normally repeat each other)
/// resolves in at most two batched lookups; the common miss case used to pay
/// one query per entry, remote-controlled up to 3 × [`MAX_ADDRESSING_ENTRIES`].
async fn names_group_audience(state: &AppState, raw: &Value) -> Result<bool, ApiError> {
    let mut local_names: Vec<&str> = Vec::new();
    let mut remote_uris: Vec<&str> = Vec::new();
    for field in ["audience", "to", "cc"] {
        for uri in one_or_many(raw.get(field))
            .iter()
            .filter_map(id_of)
            .take(MAX_ADDRESSING_ENTRIES)
        {
            if matches!(uri, plamenu_ap::activity::PUBLIC | "as:Public" | "Public") {
                continue;
            }
            match urls::parse_local_user_url(&state.config.domain, uri) {
                Some(name) => local_names.push(name),
                None => remote_uris.push(uri),
            }
        }
    }
    local_names.sort_unstable();
    local_names.dedup();
    remote_uris.sort_unstable();
    remote_uris.dedup();
    if !local_names.is_empty()
        && account::find_local_by_usernames(&state.pool, &local_names)
            .await?
            .iter()
            .any(Account::is_group)
    {
        return Ok(true);
    }
    if !remote_uris.is_empty()
        && account::find_by_uris(&state.pool, &remote_uris)
            .await?
            .iter()
            .any(Account::is_group)
    {
        return Ok(true);
    }
    Ok(false)
}

/// Extracts a reaction's `(name, custom_emoji_url)` from an `EmojiReact`
/// activity or a Misskey-dialect `Like`. The reaction is read from
/// `_misskey_reaction` first (Misskey-family, who mirror it into `content`),
/// then `content` (Pleroma/litepub), then `name` (legacy Misskey builds) —
/// the same fallback order Sharkey applies. It is a `:shortcode:`, which
/// resolves its image from a matching `Emoji` tag (and the tag is cached
/// under the sender's domain), or a bare Unicode emoji carrying no URL.
/// Returns `None` for a missing or implausibly long reaction.
async fn parse_reaction(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<Option<(String, Option<String>)>, ApiError> {
    let Some(content) = ["_misskey_reaction", "content", "name"]
        .into_iter()
        .find_map(|field| {
            raw.get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
        })
    else {
        return Ok(None);
    };
    let tags = plamenu_ap::activity::one_or_many(raw.get("tag"));
    // A `:shortcode:` reaction; anything else is treated as a Unicode emoji.
    if let Some(shortcode) = content
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .filter(|code| {
            plamenu_ap::emoji::is_valid_shortcode(
                code,
                plamenu_ap::emoji::MAX_FEDERATED_SHORTCODE_LEN,
            )
        })
    {
        // Cache the custom emoji (and read back its image) from the tag whose
        // name matches the reaction.
        if let Some(domain) = sender.domain.as_deref() {
            crate::emoji::ingest_remote_emoji_tags(
                &state.pool,
                Some(&state.config.domain),
                domain,
                tags,
            )
            .await?;
        }
        let url = reaction_emoji_url(tags, shortcode);
        return Ok(Some((shortcode.to_owned(), url)));
    }
    // A Unicode reaction: keep it only if it is short enough to be a single
    // emoji (covering ZWJ/flag sequences) rather than smuggled text.
    if content.chars().count() <= MAX_REACTION_CHARS {
        return Ok(Some((content.to_owned(), None)));
    }
    Ok(None)
}

/// The longest Unicode reaction we accept — generous enough for ZWJ emoji and
/// regional-indicator flag pairs, short enough to reject text.
const MAX_REACTION_CHARS: usize = 16;

/// Cap on distinct reported-object URIs one inbound `Flag` may fan out into
/// reports and webhooks — see [`flag_object_uris`].
const MAX_FLAG_OBJECTS: usize = 50;

/// Cap on how many addressing URIs the group-audience probes will examine per
/// field before giving up. These probes short-circuit on the first local group
/// they find, but a hostile `to`/`cc`/`audience` array would otherwise force an
/// unbounded serial account lookup per entry; the cap bounds that work (QC
/// audit #33). A group's community claim is a single URI, so 100 never elides
/// an honest match.
const MAX_ADDRESSING_ENTRIES: usize = 100;

/// The `icon.url` of the `Emoji` tag whose `name` is the reaction shortcode.
/// The tag name is compared with its colons stripped: Misskey-family peers
/// wrap it (`:blobcat:`) while Pleroma sends it bare (`blobcat`).
fn reaction_emoji_url(entries: &[Value], shortcode: &str) -> Option<String> {
    entries.iter().find_map(|entry| {
        (entry.get("type").and_then(Value::as_str) == Some("Emoji")
            && entry
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.trim_matches(':') == shortcode))
        .then(|| {
            entry
                .get("icon")
                .and_then(plamenu_ap::actor::image_url)
                .filter(|url| plamenu_federation::is_federation_url(url))
                .map(str::to_owned)
        })
        .flatten()
    })
}

/// How a reaction is shown in its notification: a custom emoji keeps its
/// `:shortcode:` form, a Unicode emoji stands alone.
fn display_emoji(name: &str, custom: bool) -> String {
    if custom {
        format!(":{name}:")
    } else {
        name.to_owned()
    }
}

/// Inbound `Undo(EmojiReact)`: a withdrawn reaction. Matched by the original
/// activity id when present (`object` is the embedded `EmojiReact` or its
/// bare id), otherwise by the `(reactor, status, emoji)` it names. Removing it
/// also clears the reaction notification and re-pushes the status.
async fn handle_undo_emoji_react(
    state: &AppState,
    sender: &Account,
    object: &Value,
) -> Result<(), ApiError> {
    let removed = match object.get("id").and_then(Value::as_str) {
        Some(react_uri) => {
            plamenu_db::reaction::delete_by_uri(&state.pool, sender.id, react_uri).await?
        }
        // No reusable id: fall back to the named (status, emoji) pair.
        None => match (
            object.get("object").and_then(id_of),
            parse_reaction(state, sender, object).await?,
        ) {
            (Some(status_uri), Some((name, _))) => match resolve_status_ref(state, status_uri)
                .await?
            {
                Some(target) => {
                    plamenu_db::reaction::delete_returning(&state.pool, sender.id, target.id, &name)
                        .await?
                }
                None => None,
            },
            _ => None,
        },
    };
    let Some(removed) = removed else {
        return Ok(());
    };
    finish_reaction_removal(state, sender, &removed).await
}

/// A removed reaction takes its notification with it and re-pushes the
/// edited status to streams.
async fn finish_reaction_removal(
    state: &AppState,
    sender: &Account,
    removed: &plamenu_db::reaction::RemovedReaction,
) -> Result<(), ApiError> {
    let author = match status::find_by_id(&state.pool, removed.status_id).await? {
        Some(target) => account::find_by_id(&state.pool, target.account_id).await?,
        None => None,
    };
    if let Some(author) = author.filter(Account::is_local) {
        let display = display_emoji(&removed.name, removed.custom_emoji_url.is_some());
        notification::clear_reaction(
            &state.pool,
            author.id,
            sender.id,
            removed.status_id,
            &display,
        )
        .await?;
    }
    crate::streaming::status_edited(state, removed.status_id).await;
    Ok(())
}

/// Inbound `Announce`: a boost from a remote account.
async fn handle_announce(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
) -> Result<(), ApiError> {
    let (Some(announce_id), Some(object_uri)) = (activity.id.as_deref(), activity.object_id())
    else {
        return Ok(());
    };
    // An Announce from a subscribed relay is a delivery hint, not a boost:
    // count it toward the relay's activity, fetch the object and stop
    // (Mastodon's `requested_through_relay?`).
    if let Some(relay_id) = crate::relays::enabled_relay_for_sender(state, sender).await? {
        plamenu_db::relay::record_activity(&state.pool, relay_id).await?;
        return crate::relays::ingest_relayed_object(state, object_uri).await;
    }
    // A Group actor speaks the FEP-1b12 dialect (Lemmy/Mbin/PieFed/Mitra):
    // its Announce delivers community activity rather than a plain boost.
    if sender.is_group() {
        return Box::pin(handle_group_announce(state, sender, activity, announce_id)).await;
    }
    // A followed account boosting a post we have never seen is the common case
    // (they boost across the whole network, not just our corner of it), so the
    // boosted post is fetched from its origin when unknown — otherwise the boost
    // is silently dropped and never surfaces in a follower's feed, exactly as
    // Mastodon's `Announce` handler fetches the original before reblogging. A
    // local URI we do not have cannot be fetched into existence (`None`).
    let Some(target) = crate::ingest::resolve_or_fetch_status(state, object_uri).await? else {
        return Ok(());
    };
    record_remote_boost(state, sender, activity, announce_id, &target).await
}

/// Stores the boost row for a verified inbound `Announce` of `target`,
/// notifying a local author and streaming it into live timelines.
async fn record_remote_boost(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    announce_id: &str,
    target: &status::Status,
) -> Result<(), ApiError> {
    let published = activity
        .published
        .as_deref()
        .and_then(|s| time::OffsetDateTime::parse(s, &Rfc3339).ok());
    // The same atomic claim covers both redelivery and promotion of a boost
    // first seen in a hydrated outbox. Only the winner may notify or stream.
    let (boost, delivery_effects) = status::upsert_remote_reblog_delivery(
        &state.pool,
        announce_id,
        sender.id,
        target.id,
        published,
    )
    .await?;
    if !delivery_effects {
        return Ok(());
    }
    let author = account::find_by_id(&state.pool, target.account_id).await?;
    if let Some(author) = author.filter(Account::is_local) {
        // A community announcing a local member's own submission back is
        // not a "reblog" to notify about — it mirrors local-group posting,
        // which records the boost silently. A community only ever announces a
        // local author's post as the return leg of that member's own
        // submission, so a group sender here is always that self-boost.
        if !sender.is_group() {
            notification::create(&state.pool, author.id, sender.id, "reblog", Some(target.id))
                .await?;
        }
    }
    // Old boosts (a replayed backlog) notify but stay out of live feeds,
    // like Mastodon's realtime-window gate on `DistributionWorker`.
    if crate::streaming::within_realtime_window(&boost) {
        crate::streaming::status_created(state, boost.id).await;
    }
    Ok(())
}

/// Activity types a FEP-1b12 group wraps in its `Announce`. Anything embedded
/// there that is *not* one of these is an object being boosted directly —
/// Lemmy's Mastodon-compat duplicate `Announce(Page)` (which carries a
/// vestigial `actor` field, so the discriminator must be the type).
const GROUP_WRAPPED_ACTIVITY_TYPES: &[&str] = &[
    "Create",
    "Update",
    "Delete",
    "Like",
    "Dislike",
    "Undo",
    "EmojiReact",
    "EmojiReaction",
    "Lock",
    "Add",
    "Remove",
    "Block",
    "Flag",
];

/// Whether `value` (an activity or object) names `group_uri` in its
/// `audience`, `to` or `cc` — the FEP-1b12 claim that it belongs to the
/// group's community. Lemmy stamps `audience` on objects and puts the group
/// in `to`/`cc`; Mitra stamps `audience` on the `Announce` wrapper.
fn addressed_to_group(value: &Value, group_uri: &str) -> bool {
    ["audience", "to", "cc"].iter().any(|key| {
        one_or_many(value.get(*key))
            .iter()
            .filter_map(id_of)
            .any(|iri| iri == group_uri)
    })
}

/// FEP-1b12: an `Announce` from a Group actor delivering community activity
/// (Lemmy/Mbin/PieFed communities, Mitra groups).
///
/// Two shapes arrive. The wrapper form embeds a whole inner activity
/// (`Announce(Create(Page))`, `Announce(Delete)`, …): the inner activity is
/// authenticated exactly like a forwarded copy — trusted outright only with a
/// valid FEP-8b32 proof from its own author (Mitra), otherwise confirmed
/// against its origin (Lemmy sends no proofs). The object form
/// (`Announce(Page)` — Lemmy's Mastodon-compat duplicate — or a bare URI) is
/// an ordinary boost, except the target is fetched when unknown: delivering
/// posts we have never seen is the whole point of following a group.
///
/// Only top-level public posts get a boost row. Lemmy announces every
/// comment and vote in the community; boosting comments would flood home
/// timelines, so they are ingested silently and threads stay complete
/// (Mastodon, unable to unwrap the dialect, drops them entirely).
#[allow(
    clippy::too_many_lines,
    reason = "one linear dispatch over the wrapped inner activity types; \
              splitting it would hide which trust rule applies to which verb"
)]
async fn handle_group_announce(
    state: &AppState,
    group: &Account,
    activity: &Activity,
    announce_id: &str,
) -> Result<(), ApiError> {
    let Some(group_uri) = group.uri.as_deref() else {
        return Ok(());
    };
    let inner_kind = activity.object.get("type").and_then(Value::as_str);
    if !inner_kind.is_some_and(|kind| GROUP_WRAPPED_ACTIVITY_TYPES.contains(&kind)) {
        // Object form: boost of a post, fetched from its origin if unknown
        // (the untrusted embedded copy is never ingested directly).
        let Some(object_uri) = activity.object_id() else {
            return Ok(());
        };
        let Some(target) = crate::ingest::resolve_or_fetch_status(state, object_uri).await? else {
            return Ok(());
        };
        // Only root posts get a boost row. `is_reply` (not `in_reply_to_id`)
        // also excludes an orphaned reply — one whose parent could not be
        // fetched — so a failed parent resolution can't promote a comment into
        // home timelines.
        if !status::is_reply(&state.pool, target.id).await? && target.visibility == "public" {
            record_remote_boost(state, group, activity, announce_id, &target).await?;
        }
        return Ok(());
    }

    // Wrapper form: parse the embedded activity.
    let Ok(inner) = serde_json::from_value::<Activity>(activity.object.clone()) else {
        return Ok(());
    };
    let Some(inner_actor) = inner.actor_id().map(str::to_owned) else {
        return Ok(());
    };
    // The announced activity must claim membership of this group's community
    // — on the wrapper (Mitra), the inner activity, or its object (Lemmy).
    let wrapper_audience_matches = one_or_many(Some(&activity.audience))
        .iter()
        .filter_map(id_of)
        .any(|iri| iri == group_uri);
    if !(wrapper_audience_matches
        || addressed_to_group(&activity.object, group_uri)
        || addressed_to_group(&inner.object, group_uri))
    {
        tracing::debug!(group = %group_uri, "ignoring group Announce of unrelated activity");
        return Ok(());
    }
    // The group editing its own profile (Lemmy's `UpdateCommunity`): refresh
    // the actor; there is no status to touch.
    if inner.kind == "Update" && inner.object_id() == Some(group_uri) {
        if let Ok(fetched) = state.federation.fetch_actor(group_uri).await {
            let stored = refresh_remote_actor(state, &fetched).await?;
            // A community edit is exactly when its mod roster may have changed
            // (Lemmy's `UpdateCommunity` carries no mod list of its own).
            crate::remote::spawn_group_moderators_sync(state, &stored, &fetched);
        }
        return Ok(());
    }

    // Community lifecycle the group relays about its own content — its own
    // `Delete`, a thread `Lock`/`Undo(Lock)`, or a mod-removal restore
    // (`Undo(Delete)`). Handled ahead of the author-proof / vote / forwarded
    // dispatch that would otherwise swallow them.
    if Box::pin(handle_relayed_lifecycle(
        state,
        group,
        group_uri,
        &inner,
        activity,
        announce_id,
    ))
    .await?
    {
        return Ok(());
    }

    // Our own member's activity announced back by a remote community: we
    // already hold the authoritative local copy. Skip the inner dispatch — a
    // no-proof forward would otherwise re-fetch and clobber our own status, and
    // a proven Create would early-return anyway — but let the boost row below
    // still record the community attribution. Mirrors the same short-circuit in
    // `handle_group_relayed_interaction` for our members' returning votes.
    let inner_is_local =
        crate::local_identity::has_local_actor_shape(&state.config.domain, &inner_actor);
    if inner_is_local {
        // nothing to dispatch; the attribution boost row is recorded below
    } else if let Some(proven) = Box::pin(proof_verified_actor(
        state,
        &activity.object,
        &inner,
        &inner_actor,
    ))
    .await?
    {
        // The author's own FEP-8b32 proof authenticates the embedded copy
        // wholesale: dispatch it as if delivered directly (Mitra signs the
        // activities its groups relay).
        match inner.kind.as_str() {
            "Create" => handle_create(state, &proven, &inner, &activity.object, None).await?,
            "Update" => handle_update(state, &proven, &inner, &activity.object).await?,
            "Delete" => handle_delete(state, &proven, &inner, &activity.object).await?,
            "Like" => handle_like(state, &proven, &inner, &activity.object).await?,
            "EmojiReact" | "EmojiReaction" => {
                handle_emoji_react(state, &proven, &inner, &activity.object).await?;
            }
            "Dislike" => handle_dislike(state, &proven, &inner, &activity.object).await?,
            "Undo" => handle_undo(state, &proven, &inner, &activity.object).await?,
            "Join" => handle_join(state, &proven, &inner, &activity.object).await?,
            "Leave" => handle_leave(state, &proven, &inner).await?,
            "Invite" => handle_invite(state, &proven, &inner, &activity.object).await?,
            kind => {
                tracing::debug!(kind, group = %group_uri, "ignoring group-wrapped activity type");
            }
        }
    } else if matches!(
        inner.kind.as_str(),
        "Like" | "Dislike" | "Undo" | "Join" | "Leave"
    ) {
        // No proof, but it's an interaction the group is authoritative for on its
        // own community's content — every Lemmy-family consumer trusts the
        // announce (Lemmy signs nothing). Accepted exactly when this group
        // boosted the target. Participation rides the same rule: a group that
        // announced an event relays RSVPs to it (E3).
        Box::pin(handle_group_relayed_interaction(
            state,
            group,
            &inner,
            &inner_actor,
            &activity.object,
        ))
        .await?;
    } else {
        // No proof (Lemmy): the forwarded-copy rules apply — a Create/Update
        // is replaced by the origin's copy, a Delete needs the origin to
        // confirm, everything else is ignored.
        Box::pin(handle_forwarded(state, &inner, &inner_actor)).await?;
    }

    // The group's announcement is what puts a member's post into follower
    // timelines: boost the (now ingested) top-level post.
    // `is_reply` (not `in_reply_to_id`) also excludes an orphaned reply — one
    // whose parent could not be fetched at ingest — so a comment the community
    // announces is never mistaken for a root post and boosted into timelines.
    if inner.kind == "Create"
        && let Some(object_uri) = inner.object_id()
        && let Some(target) = resolve_status_ref(state, object_uri).await?
        && !status::is_reply(&state.pool, target.id).await?
        && target.visibility == "public"
    {
        record_remote_boost(state, group, activity, announce_id, &target).await?;
    }
    Ok(())
}

/// Community-lifecycle activities a consumed group relays about its own content:
/// the community's own `Delete`, a thread `Lock`/`Undo(Lock)`, and a
/// mod-removal restore (`Undo(Delete)`). Lemmy sends these unproven, so — like a
/// relayed vote — trust rests on the HTTP-signed Announce plus the audience gate
/// the caller already applied, and (for a lock) on this group having actually
/// announced the target; no mod-identity check is made. Returns whether the
/// activity was consumed, so the caller returns early ahead of the author-proof
/// / vote / forwarded dispatch that would otherwise drop it.
async fn handle_relayed_lifecycle(
    state: &AppState,
    group: &Account,
    group_uri: &str,
    inner: &Activity,
    activity: &Activity,
    announce_id: &str,
) -> Result<bool, ApiError> {
    // The community deletes itself (Lemmy: `Delete(actor=person, object=community)`).
    // Gone-confirm stays fail-closed: a transient fetch error keeps the account.
    if inner.kind == "Delete" && inner.object_id() == Some(group_uri) {
        if origin_confirms_gone(state, group_uri).await? {
            account::delete_by_uri(&state.pool, group_uri).await?;
            tracing::info!(group = %group_uri, "consumed community deleted upstream; dropped stored group account");
        }
        return Ok(true);
    }
    // A moderator (or the author) removes a post/comment the community
    // announced (Lemmy: one `Delete` type for both — a `summary` marks a mod
    // removal, whose actor is then the MOD, not the author). Our author-gated
    // delete path drops a mod removing someone else's post (the deferred
    // residual); handle it here. Authorization is fail-closed against the post's
    // OWN origin — we drop it only when that origin confirms it gone
    // (410/Tombstone), which a forgeable Announce cannot fake — and only for a
    // post THIS community actually announced. Subsumes and hardens author
    // self-delete of a group post too.
    if inner.kind == "Delete"
        && let Some(target_uri) = inner.object_id()
        && let Some(target) = status::find_by_uri(&state.pool, target_uri).await?
    {
        if plamenu_db::group::boosting_group_ids(&state.pool, target.id)
            .await?
            .contains(&group.id)
            && origin_confirms_gone(state, target_uri).await?
        {
            status::stub_by_uri_any(&state.pool, target_uri).await?;
            tracing::info!(group = %group_uri, status = target.id, "consumed community removed a post; stubbed locally");
        }
        return Ok(true);
    }
    // A community ban that purges the banned user's content (Lemmy:
    // `Block`/`BlockUser` with `removeData: true`). Lemmy runs the purge locally
    // and sends NO per-post `Delete` — this wrapper is the only signal — so
    // mirror it: drop every post/comment of the banned person that THIS
    // community announced. A plain ban (no `removeData`) touches no content.
    // Scoped to the group's own boost rows, so a community reaches only its own
    // space. `removeData` sits on the activity itself, not its object, so it is
    // read from the raw inner JSON (the `Activity` struct drops unknown fields).
    if inner.kind == "Block"
        && activity.object.get("removeData").and_then(Value::as_bool) == Some(true)
        && let Some(person_uri) = inner.object_id()
        && let Some(person) = account::find_by_uri(&state.pool, person_uri).await?
    {
        let removed = status::remove_group_content_of(&state.pool, group.id, person.id).await?;
        if removed > 0 {
            tracing::info!(group = %group_uri, person = %person_uri, removed, "consumed community banned a user; purged their content");
        }
        return Ok(true);
    }
    // A thread lock / unlock (Lemmy: `Lock` / `Undo(Lock)`). Keyed on the (remote)
    // group account and the thread root, so it surfaces through the group-agnostic
    // `group_locked` observable and refuses new replies to the thread.
    let undone_kind = (inner.kind == "Undo")
        .then(|| inner.object.get("type").and_then(Value::as_str))
        .flatten();
    if inner.kind == "Lock" || undone_kind == Some("Lock") {
        let locking = inner.kind == "Lock";
        let target_uri = if locking {
            inner.object_id()
        } else {
            inner.object.get("object").and_then(id_of)
        };
        // `resolve_status_ref`, not `find_by_uri`: a community locks the
        // threads it hosts, and one of our own members' submissions is a
        // *local* status — which has no stored `uri` to match on.
        if let Some(target_uri) = target_uri
            && let Some(status) = crate::ingest::resolve_status_ref(state, target_uri).await?
        {
            let root = status::thread_root(&state.pool, status.id).await?;
            if locking {
                // Authorize the lock against the post's *own* origin copy: a
                // moderation lock is honored only from the community the post
                // genuinely claims as its `audience`. A boost row is not proof
                // — any Group can `Announce` any public post — so authorizing on
                // `boosting_group_ids` would let a hostile community lock (and,
                // via the reply gate below, block replies to) arbitrary remote
                // posts. Fail-closed: an unverifiable post is not locked.
                //
                // Our own member's post is the one object we never fetch: the
                // origin is us. Read the same community claim we would serve
                // as its `audience` — a self-fetch would return exactly this
                // and nothing more.
                let hosts_it = if status.uri.is_none() {
                    crate::groups::communities_of_status(state, &status)
                        .await?
                        .iter()
                        .any(|community| community.id == group.id)
                } else {
                    matches!(
                        state.federation.fetch_object(target_uri).await,
                        Ok(object) if addressed_to_group(&object, group_uri)
                    )
                };
                if hosts_it {
                    plamenu_db::group::lock_thread(&state.pool, group.id, root).await?;
                    tracing::info!(group = %group_uri, status = status.id, "consumed community locked a thread");
                }
            } else {
                // Unlock clears only *this* group's own lock row, so removing a
                // restriction is always safe and needs no origin check.
                plamenu_db::group::unlock_thread(&state.pool, group.id, root).await?;
                tracing::info!(group = %group_uri, status = status.id, "consumed community unlocked a thread");
            }
        }
        return Ok(true);
    }
    // The community restores a post it had mod-removed (Lemmy: `Undo(Delete)`):
    // re-fetch the now-live origin Page and re-record the community boost —
    // exactly the object-form boost path. Idempotent (`resolve_or_fetch_status`
    // no-ops when the post is present, `record_remote_boost` when the boost is),
    // so it is a safe no-op when nothing was dropped (a non-author mod removal).
    if undone_kind == Some("Delete") {
        if let Some(target_uri) = inner.object.get("object").and_then(id_of) {
            // The mod-removal recorded a fetch-failure for the post (its origin
            // 410'd it), whose backoff would otherwise suppress the re-fetch.
            // The community's explicit restore is a positive "it is live again"
            // signal, so clear that negative cache before re-fetching the now-
            // live origin Page.
            let _ =
                plamenu_db::remote_fetch_failure::clear(&state.pool, "resource", target_uri).await;
            if let Some(target) = crate::ingest::resolve_or_fetch_status(state, target_uri).await?
                && !status::is_reply(&state.pool, target.id).await?
                && target.visibility == "public"
            {
                record_remote_boost(state, group, activity, announce_id, &target).await?;
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// An interaction relayed by a remote group without an author proof: a
/// vote (`Like`/`Dislike`, or an `Undo` of one) or an RSVP (`Join`/`Leave`, E3).
///
/// Scoped trust, Lemmy's model: the group is authoritative for interactions with
/// content it announced — accepted exactly when this group has a boost row for
/// the target, then dispatched as if the actor had delivered it directly. An
/// unknown actor is resolved with a policy-checked fetch for a fresh
/// interaction; a retraction from an actor we never stored has nothing to undo
/// and fetches nobody.
async fn handle_group_relayed_interaction(
    state: &AppState,
    group: &Account,
    inner: &Activity,
    inner_actor: &str,
    raw_inner: &Value,
) -> Result<(), ApiError> {
    // Our own members' interactions come back around through the group's relay;
    // we already hold the original.
    if crate::local_identity::has_local_actor_shape(&state.config.domain, inner_actor) {
        return Ok(());
    }
    let target_uri = if inner.kind == "Undo" {
        match inner.object.get("type").and_then(Value::as_str) {
            Some("Like" | "Dislike" | "Join") => inner.object.get("object").and_then(id_of),
            // A bare-uri Undo names no target to gate on; the by-uri
            // deletions below are inherently scoped to the voter's own rows.
            None => None,
            Some(_) => return Ok(()),
        }
    } else {
        inner.object_id()
    };
    if let Some(target_uri) = target_uri {
        let Some(target) = resolve_status_ref(state, target_uri).await? else {
            return Ok(());
        };
        if !plamenu_db::group::boosting_group_ids(&state.pool, target.id)
            .await?
            .contains(&group.id)
        {
            tracing::debug!(group = %group.uri.as_deref().unwrap_or_default(),
                "ignoring a group-relayed interaction with content the group never announced");
            return Ok(());
        }
    } else if inner.kind != "Undo" {
        return Ok(()); // a vote of nothing
    }
    let actor = match account::find_by_uri(&state.pool, inner_actor).await? {
        Some(known) => known,
        None if inner.kind == "Undo" => return Ok(()),
        None => {
            if !crate::instance_policy::can_federate_url(
                &state.pool,
                &state.config.domain,
                inner_actor,
            )
            .await?
            {
                return Ok(());
            }
            let Ok(fetched) = state.federation.fetch_actor(inner_actor).await else {
                return Ok(());
            };
            refresh_remote_actor(state, &fetched).await?
        }
    };
    match inner.kind.as_str() {
        "Like" => handle_like(state, &actor, inner, raw_inner).await,
        "Dislike" => handle_dislike(state, &actor, inner, raw_inner).await,
        "Undo" => handle_undo(state, &actor, inner, raw_inner).await,
        "Join" => handle_join(state, &actor, inner, raw_inner).await,
        "Leave" => handle_leave(state, &actor, inner).await,
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Inbound group moderation (remote moderators + community reports)
// ---------------------------------------------------------------------------

/// A local group named in the activity's `audience` (where Lemmy stamps the
/// community on every mod action and community report). `None` when no local
/// group is addressed.
async fn local_group_in_audience(
    state: &AppState,
    activity: &Activity,
) -> Result<Option<Account>, ApiError> {
    let entries = one_or_many(Some(&activity.audience));
    let actor_uris: Vec<&str> = entries
        .iter()
        .filter_map(id_of)
        .take(MAX_ADDRESSING_ENTRIES)
        .filter(|uri| crate::local_identity::has_local_actor_shape(&state.config.domain, uri))
        .collect();
    if actor_uris.is_empty() {
        return Ok(None);
    }
    let locals =
        crate::local_identity::find_actors(&state.pool, &state.config.domain, &actor_uris).await?;
    for uri in actor_uris {
        if let Some(local) = locals.get(uri)
            && local.is_group()
        {
            return Ok(Some(local.clone()));
        }
    }
    Ok(None)
}

/// The local group `sender` is moderating with this activity: a FEP-1b12 mod
/// action addressed (via `audience`) to one of our hosted groups, where the
/// signature-verified `sender` is an owner or moderator. Remote moderators of a
/// Plamenu-hosted group co-moderate exactly as Lemmy instances do.
async fn moderated_local_group(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
) -> Result<Option<Account>, ApiError> {
    let Some(group) = local_group_in_audience(state, activity).await? else {
        return Ok(None);
    };
    let elevated = matches!(
        plamenu_db::group::affiliation_of(&state.pool, group.id, sender.id).await?,
        Some(plamenu_db::group::Affiliation::Owner | plamenu_db::group::Affiliation::Moderator)
    );
    if !elevated {
        tracing::debug!(group = %group.username, actor = %sender.username,
            "ignoring group mod action from a non-moderator");
    }
    Ok(elevated.then_some(group))
}

/// A remote moderator's `Block` targeting one of our hosted groups (Lemmy's
/// community ban): record the outcast, sever the follow, and re-announce the
/// wrapped ban to the group's members so every server converges.
async fn handle_group_ban(
    state: &AppState,
    group: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(target_uri) = activity.object_id() else {
        return Ok(());
    };
    // A ban of an actor we've never seen has nothing to enforce locally.
    let Some(target) = account::find_by_uri(&state.pool, target_uri).await? else {
        return Ok(());
    };
    let expires = raw
        .get("expires")
        .and_then(Value::as_str)
        .and_then(|s| time::OffsetDateTime::parse(s, &Rfc3339).ok());
    crate::groups::apply_ban(state, group.id, target.id, expires).await?;
    crate::groups::announce_wrapped(state, group, raw).await?;
    tracing::info!(group = %group.username, target = %target.username, "remote group ban applied");
    Ok(())
}

/// A remote moderator's `Lock` targeting a post in one of our hosted groups:
/// lock the thread and re-announce.
async fn handle_group_lock(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(group) = moderated_local_group(state, sender, activity).await? else {
        return Ok(());
    };
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };
    let Some(status) = status::find_by_uri(&state.pool, object_uri).await? else {
        return Ok(());
    };
    let root = status::thread_root(&state.pool, status.id).await?;
    plamenu_db::group::lock_thread(&state.pool, group.id, root).await?;
    crate::groups::announce_wrapped(state, &group, raw).await?;
    tracing::info!(group = %group.username, status = status.id, "remote group thread lock applied");
    Ok(())
}

/// A remote moderator's `Add`/`Remove` targeting our group's `featured`
/// collection (pin) or `moderators` collection (mod change). Applies the local
/// state and re-announces.
async fn handle_group_collection_change(
    state: &AppState,
    group: &Account,
    activity: &Activity,
    raw: &Value,
    add: bool,
) -> Result<(), ApiError> {
    let Some(target_uri) = raw.get("target").and_then(id_of) else {
        return Ok(());
    };
    let group_urls = urls::LocalUserUrls::for_account(
        &state.config.domain,
        &group.username,
        group.uri.as_deref(),
    );
    if target_uri == group_urls.featured {
        let Some(object_uri) = activity.object_id() else {
            return Ok(());
        };
        let Some(status) = status::find_by_uri(&state.pool, object_uri).await? else {
            return Ok(());
        };
        if add {
            plamenu_db::pin::create(&state.pool, group.id, status.id).await?;
        } else {
            plamenu_db::pin::delete(&state.pool, group.id, status.id).await?;
        }
        crate::groups::announce_wrapped(state, group, raw).await?;
    } else if target_uri == group_urls.moderators {
        let Some(object_uri) = activity.object_id() else {
            return Ok(());
        };
        let Some(target) = account::find_by_uri(&state.pool, object_uri).await? else {
            return Ok(());
        };
        if add {
            plamenu_db::group::set_affiliation(
                &state.pool,
                group.id,
                target.id,
                plamenu_db::group::Affiliation::Moderator,
                None,
            )
            .await?;
        } else if plamenu_db::group::affiliation_of(&state.pool, group.id, target.id).await?
            == Some(plamenu_db::group::Affiliation::Moderator)
        {
            plamenu_db::group::remove_affiliation(&state.pool, group.id, target.id).await?;
        }
        crate::groups::announce_wrapped(state, group, raw).await?;
    }
    Ok(())
}

/// A community report (`Flag` addressed to a local group's `audience`): filed
/// as a group-scoped report routed to the group's moderators, not instance
/// staff. Reports come from any member, so — unlike the other mod actions —
/// the sender need not be elevated. A local-user target still surfaces in the
/// instance-staff console too (the report row carries both scopes).
async fn handle_group_flag(
    state: &AppState,
    sender: &Account,
    group: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    // Lemmy carries the reason in `summary`; Mastodon-style `content` too.
    let comment: String = raw
        .get("summary")
        .or_else(|| raw.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(5000)
        .collect();
    let report_uri = match activity.id.as_deref() {
        Some(id)
            if crate::remote::host_of(id)
                == sender.uri.as_deref().and_then(crate::remote::host_of) =>
        {
            Some(id)
        }
        _ => None,
    };
    if let Some(uri) = report_uri
        && report::exists_by_uri(&state.pool, uri).await?
    {
        return Ok(());
    }
    // The reported object is a group post/comment or member account. Only
    // content the group actually hosts can be reported to its mods. Resolve the
    // (bounded, deduplicated) URIs set-based, so one Flag packed with distinct
    // URLs costs two queries rather than one or two per URI.
    let object_uris = flag_object_uris(&activity.object);
    let uri_refs: Vec<&str> = object_uris.iter().map(String::as_str).collect();
    let mut targets: HashMap<i64, Vec<i64>> = HashMap::new();
    let statuses = status::find_by_uris(&state.pool, &uri_refs).await?;
    let matched: HashSet<&str> = statuses.iter().filter_map(|s| s.uri.as_deref()).collect();
    for status in &statuses {
        targets
            .entry(status.account_id)
            .or_default()
            .push(status.id);
    }
    // URIs that were not statuses may name a member account instead.
    let account_uris: Vec<&str> = uri_refs
        .iter()
        .copied()
        .filter(|uri| !matched.contains(uri))
        .collect();
    for account in account::find_by_uris(&state.pool, &account_uris).await? {
        targets.entry(account.id).or_default();
    }
    if targets.is_empty() {
        return Ok(());
    }
    for (target_account_id, status_ids) in targets {
        let report = report::create_scoped(
            &state.pool,
            report::NewReport {
                account_id: sender.id,
                target_account_id,
                status_ids: &status_ids,
                comment: &comment,
                category: "other",
                forwarded: None,
                rule_ids: None,
                uri: report_uri,
            },
            Some(group.id),
        )
        .await?;
        tracing::info!(group = %group.username, report = report.id, "community report filed");
    }
    Ok(())
}

/// Routes `Accept`/`Reject` to the follow, quote or feature-request handler.
/// A `FeatureRequest` response carries its request id as a bare `object`, so it
/// is recognised by matching that id to a pending membership of the sender,
/// like Mastodon's `feature_request_from_object`.
async fn handle_response(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    accepted: bool,
) -> Result<(), ApiError> {
    if activity.object.get("type").and_then(Value::as_str) == Some("QuoteRequest") {
        return handle_quote_response(state, sender, activity, raw, accepted).await;
    }
    // An organizer's verdict on one of our RSVPs (E2). Routed by the inner type
    // rather than by a new top-level arm, since `Accept`/`Reject` already fan out
    // that way. `Join` may be embedded whole (Mobilizon echoes it) or named by id.
    if activity.object.get("type").and_then(Value::as_str) == Some("Join")
        || crate::events::names_our_join(state, activity.object_id()).await?
    {
        return handle_join_response(state, sender, activity, raw, accepted).await;
    }
    // A relay answering our instance actor's Follow(as:Public): the object is
    // that Follow (embedded or by bare id) — flip the relay's state (Mastodon's
    // `accept_follow_for_relay` / `reject_follow_for_relay`).
    if let Some(follow_activity_id) = activity.object_id()
        && plamenu_db::relay::resolve_follow_response(&state.pool, follow_activity_id, accepted)
            .await?
    {
        tracing::info!(relay = %sender.inbox_url, accepted, "relay follow response");
        return Ok(());
    }
    if let Some(request_uri) = id_of(&activity.object)
        && plamenu_db::collection::find_item_by_activity_uri(&state.pool, request_uri)
            .await?
            .is_some_and(|item| item.account_id == Some(sender.id))
    {
        let approval_uri = raw.get("result").and_then(id_of);
        return crate::collections::process_feature_response(
            state,
            sender,
            request_uri,
            approval_uri,
            accepted,
        )
        .await;
    }
    handle_follow_response(state, sender, activity, accepted).await
}

/// Inbound `Join`: a remote attendee RSVPs to an event we host (E3). The join
/// mode and our capacity decide whether an `Accept` goes straight back, the
/// request waits for a moderator, or it is refused.
async fn handle_join(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    crate::events::handle_inbound_join(
        state,
        sender,
        activity.id.as_deref(),
        activity.object_id(),
        raw.get("participationMessage").and_then(Value::as_str),
    )
    .await
}

/// Inbound `Leave`: the attendee withdraws an RSVP to an event we host.
async fn handle_leave(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
) -> Result<(), ApiError> {
    // Mobilizon's `Leave` names the *event*; the participation url is only
    // available when a peer names it instead, hence both are offered.
    crate::events::handle_inbound_leave(
        state,
        sender,
        activity.object.get("id").and_then(Value::as_str),
        activity.object_id(),
    )
    .await
}

/// Inbound `Invite`: an organizer invites one of our accounts to their event.
async fn handle_invite(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    crate::events::handle_inbound_invite(
        state,
        sender,
        activity.object_id(),
        // AS2 puts the invitee in `target`; some senders use `to` instead.
        raw.get("target")
            .and_then(id_of)
            .or_else(|| one_or_many(raw.get("to")).iter().find_map(id_of)),
    )
    .await
}

/// An organizer's `Accept(Join)` / `Reject(Join)`: the verdict on an RSVP we
/// sent (E2).
///
/// The `Join` is found by its activity id — the only handle the origin echoes
/// back — whether it arrives embedded or as a bare IRI. An `Accept` for an RSVP
/// we have no row for is dropped silently: it is either a replay of one we have
/// since left, or an organizer answering something we never asked.
///
/// Trust: for a group event the `actor` is a group *moderator* and
/// `attributedTo` is the group, so the signer is legitimately neither the event's
/// author nor the group itself. Rather than inventing a second rule, this reuses
/// the group moderation shape — the verdict is honoured when the sender is the event's
/// author, the announcing group, or an actor the announcing group vouches for by
/// naming itself in `attributedTo`.
async fn handle_join_response(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    accepted: bool,
) -> Result<(), ApiError> {
    let Some(join_uri) = activity
        .object
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| activity.object_id())
    else {
        return Ok(());
    };
    let Some(row) = plamenu_db::status_participation::find_by_uri(&state.pool, join_uri).await?
    else {
        return Ok(());
    };
    let Some(item) = status::find_by_id(&state.pool, row.status_id).await? else {
        return Ok(());
    };
    if !crate::events::verdict_is_trusted(state, &item, sender, raw).await? {
        tracing::info!(
            join = join_uri,
            actor = %sender.inbox_url,
            "ignoring a Join verdict from an actor with no claim on the event"
        );
        return Ok(());
    }
    let next = if accepted {
        plamenu_db::status_participation::State::Accepted
    } else {
        plamenu_db::status_participation::State::Rejected
    };
    if plamenu_db::status_participation::settle_by_uri(&state.pool, join_uri, next)
        .await?
        .is_some()
    {
        crate::events::notify_attendee(state, &item, row.account_id, accepted).await?;
    }
    Ok(())
}

/// Inbound `FeatureRequest` (FEP-7aa9): a remote owner asks to feature one of
/// our local accounts in their collection.
async fn handle_feature_request(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let (Some(request_uri), Some(object_uri)) = (activity.id.as_deref(), activity.object_id())
    else {
        return Ok(());
    };
    let Some(instrument_uri) = raw.get("instrument").and_then(id_of) else {
        return Ok(());
    };
    crate::collections::process_feature_request(
        state,
        sender,
        request_uri,
        object_uri,
        instrument_uri,
    )
    .await
}

/// FEP-044f: a remote user asks to quote one of our posts. Public and
/// unlisted originals are quotable by anyone (our fixed policy) — answer
/// with `Accept` + a `QuoteAuthorization` stamp, otherwise `Reject`.
async fn handle_quote_request(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let (Some(request_uri), Some(quoted_uri)) = (activity.id.as_deref(), activity.object_id())
    else {
        return Ok(());
    };
    let Some(instrument_uri) = raw.get("instrument").and_then(id_of) else {
        return Ok(());
    };
    let Some(quoted) = resolve_status_ref(state, quoted_uri).await? else {
        return Ok(());
    };
    let Some(quoted_author) = account::find_by_id(&state.pool, quoted.account_id).await? else {
        return Ok(());
    };
    if !quoted_author.is_local() {
        return Ok(()); // not ours to authorize
    }
    let Some(sender_uri) = sender.uri.as_deref() else {
        return Ok(());
    };

    let quote_id = plamenu_db::id::next();
    let params = plamenu_ap::activity::QuoteResponseParams {
        domain: &state.config.domain,
        username: &quoted_author.username,
        actor_id: quoted_author.uri.as_deref(),
        quote_id,
        request_activity_uri: request_uri,
        quoting_actor_uri: sender_uri,
        quote_status_uri: instrument_uri,
        quoted_status_uri: &crate::entities::status_uri_for_account(
            &state.config.domain,
            &quoted,
            &quoted_author,
        ),
    };

    let allowed = quote_request_allowed(state, &quoted, &quoted_author, sender).await?;
    if allowed {
        let row = plamenu_db::quote::create(
            &state.pool,
            plamenu_db::quote::NewQuote {
                quote_id,
                status_id: status::find_by_uri(&state.pool, instrument_uri)
                    .await?
                    .map(|s| s.id),
                status_uri: instrument_uri,
                account_id: sender.id,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(quoted_author.id),
                state: "accepted",
                activity_uri: Some(request_uri),
                approval_uri: None,
                quoted_uri: Some(params.quoted_status_uri),
                legacy: false,
            },
        )
        .await?;
        // The quote notification is deferred to when the quoting post is
        // actually ingested (`link_inbound_quote`): only then is there a
        // status to point it at — the post that did the quoting, rather than
        // the quoted author's own post. Mastodon may deliver that Create
        // before the QuoteRequest, so notify here too when the quote row is
        // already linked.
        if let Some(status_id) = row.status_id
            && !notification::exists(
                &state.pool,
                quoted_author.id,
                sender.id,
                "quote",
                Some(status_id),
            )
            .await?
        {
            notification::create(
                &state.pool,
                quoted_author.id,
                sender.id,
                "quote",
                Some(status_id),
            )
            .await?;
        }
        //
        // The stamp id must reference the actual row (idempotent redelivery
        // may have returned an older one).
        let response = plamenu_ap::activity::accept_quote_request(
            &plamenu_ap::activity::QuoteResponseParams {
                quote_id: row.id,
                ..params
            },
        );
        job::enqueue(&state.pool, quoted_author.id, &sender.inbox_url, &response).await?;
        tracing::info!(quoting = %sender.username, quoted = %quoted_author.username, "quote authorized");
    } else {
        let response = plamenu_ap::activity::reject_quote_request(&params);
        job::enqueue(&state.pool, quoted_author.id, &sender.inbox_url, &response).await?;
    }
    Ok(())
}

/// Whether `sender` may quote the local `quoted` post under its stored quote
/// policy (the automatic sub-policy — local posts never carry a manual one, so
/// a manual-only or `nobody` policy is a `Reject`). `public` lets anyone quote;
/// `followers` requires the quoter to follow the author; `following` requires
/// the author to follow the quoter.
async fn quote_request_allowed(
    state: &AppState,
    quoted: &status::Status,
    quoted_author: &Account,
    sender: &Account,
) -> Result<bool, ApiError> {
    // Boosts are never quotable; their policy bitmap is 0 anyway.
    if quoted.uri.is_some() || quoted.reblog_of_id.is_some() {
        return Ok(false);
    }
    let automatic =
        plamenu_ap::quote_policy::QuotePolicy::from_bitmap(quoted.quote_approval_policy)
            .automatic();
    if automatic.public() {
        return Ok(true);
    }
    if automatic.followers()
        && follow::find(&state.pool, sender.id, quoted_author.id)
            .await?
            .is_some_and(|edge| !edge.pending)
    {
        return Ok(true);
    }
    if automatic.following()
        && follow::find(&state.pool, quoted_author.id, sender.id)
            .await?
            .is_some_and(|edge| !edge.pending)
    {
        return Ok(true);
    }
    Ok(false)
}

/// `Accept`/`Reject` of a `QuoteRequest` we sent: resolves the pending quote
/// and, on acceptance, re-distributes the post with its authorization stamp.
async fn handle_quote_response(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    accepted: bool,
) -> Result<(), ApiError> {
    let Some(request_uri) = activity.object.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let approval_uri = raw.get("result").and_then(id_of);
    if accepted {
        // The stamp must live on the quoted author's server.
        let valid_host = approval_uri.is_some_and(|uri| {
            crate::remote::host_of(uri).is_some()
                && crate::remote::host_of(uri)
                    == sender.uri.as_deref().and_then(crate::remote::host_of)
        });
        if !valid_host {
            return Ok(());
        }
    }
    let state_str = if accepted { "accepted" } else { "rejected" };
    let Some(row) = plamenu_db::quote::set_state_by_activity_uri(
        &state.pool,
        request_uri,
        sender.id,
        state_str,
        approval_uri,
    )
    .await?
    else {
        return Ok(());
    };
    tracing::info!(state = state_str, quote = row.id, "quote request resolved");

    // Re-distribute the now-authorized post so remotes render the quote.
    if accepted
        && let Some(status_id) = row.status_id
        && let Some(stored) = status::find_by_id(&state.pool, status_id).await?
        && let Some(author) = account::find_by_id(&state.pool, stored.account_id).await?
        && author.is_local()
    {
        let note = crate::note::note_for_status(state, &stored, &author).await?;
        let updated = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        let update = plamenu_ap::activity::update_note(
            &state.config.domain,
            &author.username,
            note,
            &updated,
        );
        let mut inboxes = follow::follower_inboxes(&state.pool, author.id).await?;
        if !inboxes.iter().any(|i| i == &sender.inbox_url) {
            inboxes.push(sender.inbox_url.clone());
        }
        job::enqueue_many(&state.pool, author.id, &inboxes, &update, false).await?;
    }
    Ok(())
}

/// `Accept`/`Reject` of a follow we sent: the object is our `Follow`, either
/// embedded or as a bare id. The local side is recovered from the activity
/// and the remote side must be the (signature-verified) sender.
async fn handle_follow_response(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    accepted: bool,
) -> Result<(), ApiError> {
    let local_actor_url = match &activity.object {
        Value::Object(map) => {
            let object_type = map.get("type").and_then(Value::as_str);
            if object_type != Some("Follow") {
                tracing::debug!(
                    remote = %sender.username,
                    accepted,
                    object_type = object_type.unwrap_or("<none>"),
                    "ignoring follow response: embedded object is not a Follow"
                );
                return Ok(());
            }
            map.get("actor").and_then(id_of).map(str::to_owned)
        }
        // Bare id: our follow ids live on the actor URL, before the fragment.
        Value::String(uri) => Some(uri.split('#').next().unwrap_or(uri).to_owned()),
        _ => None,
    };
    let Some(local_actor_url) = local_actor_url else {
        tracing::debug!(
            remote = %sender.username,
            accepted,
            "ignoring follow response: could not resolve the local follower actor from the object"
        );
        return Ok(());
    };
    let Some(local) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, &local_actor_url)
            .await?
    else {
        tracing::debug!(
            remote = %sender.username,
            accepted,
            local_actor_url = %local_actor_url,
            "ignoring follow response: object actor is not a known local actor"
        );
        return Ok(());
    };
    if accepted {
        follow::mark_accepted(&state.pool, local.id, sender.id).await?;
        if let Err(error) = crate::remote_history::request(
            state,
            sender,
            plamenu_db::remote_history::JobKind::Initial,
            Some(local.id),
        )
        .await
        {
            // Follow acceptance is authoritative; hydration is optional and
            // must never turn a valid Accept into an inbox failure.
            tracing::debug!(remote = sender.id, error = %error.chain(), "history enqueue after follow acceptance skipped");
        }
        tracing::info!(local = %local.username, remote = %sender.username, "follow accepted");
    } else {
        follow::delete(&state.pool, local.id, sender.id).await?;
        tracing::info!(local = %local.username, remote = %sender.username, "follow rejected");
    }
    Ok(())
}

/// Resolves the signature's keyId to the account that produced it and checks
/// the signature, dereferencing (or re-dereferencing, for key rotations) the
/// actor when the cached key is missing or stale. The signer is usually the
/// activity's actor (a direct delivery); when it is another server's actor the
/// delivery is a forwarded copy, and the caller must not trust the payload on
/// the actor's behalf.
pub(crate) async fn verified_signer(
    state: &AppState,
    prepared: &PreparedRequestAuth,
    claimed_actor: &str,
) -> Result<Account, ApiError> {
    let key_id = prepared.key_id();

    if let Some(cached) = crate::remote::find_cached_signer(&state.pool, key_id).await?
        && crate::remote::prepared_matches_account(&state.pool, prepared, &cached)
            .await?
            .is_ok()
    {
        return Ok(cached);
    }
    // Cache miss or stale key. Direct deliveries dominate, so the claimed
    // actor is dereferenced first; when the key turns out not to be theirs,
    // the keyId's own base URL names the actual signer (a forwarder we may
    // never have seen).
    let claimed = state.federation.fetch_actor(claimed_actor).await;
    let fetched = match claimed {
        // `fetch_actor` guarantees fetched.id == claimed_actor.
        Ok(actor) if actor.owns_key(key_id) => actor,
        other => {
            let key_owner = key_id
                .split('#')
                .next()
                .filter(|base| !base.is_empty() && *base != claimed_actor);
            let Some(key_owner) = key_owner else {
                return Err(match other {
                    Ok(actor) => ApiError::Unauthorized(format!(
                        "keyId {key_id} does not belong to the claimed actor {claimed_actor} (declared key {})",
                        actor.public_key.id
                    )),
                    Err(e) => ApiError::Unauthorized(format!(
                        "cannot dereference signer of keyId {key_id}: {e}"
                    )),
                });
            };
            let actor =
                state.federation.fetch_actor(key_owner).await.map_err(|e| {
                    ApiError::Unauthorized(format!("cannot dereference signer: {e}"))
                })?;
            if !actor.owns_key(key_id) {
                return Err(ApiError::Unauthorized(format!(
                    "keyId {key_id} does not belong to its actor {} (declared key {})",
                    actor.id, actor.public_key.id
                )));
            }
            actor
        }
    };
    let stored = refresh_remote_actor(state, &fetched).await?;
    crate::remote::prepared_matches_account(&state.pool, prepared, &stored)
        .await?
        .map_err(|e| ApiError::Unauthorized(format!("{e} (keyId {key_id})")))?;
    Ok(stored)
}

/// FEP-8b32: authenticates a forwarded activity by its integrity proof. Only
/// a proof whose verification method lives on the activity's own actor
/// counts; the key is the actor's stored Ed25519 Multikey, (re)dereferencing
/// the actor when we have no key yet or the stored one no longer verifies (a
/// rotation). Returns the actor's account when the payload is proven theirs,
/// `None` (never a hard error) to fall back to the origin-confirmation path.
async fn proof_verified_actor(
    state: &AppState,
    raw: &Value,
    activity: &Activity,
    actor_id: &str,
) -> Result<Option<Account>, ApiError> {
    let Ok(prepared) = plamenu_ap::proof::PreparedProof::from_document(raw) else {
        return Ok(None);
    };
    // The exact method must resolve to a normalized key controlled by the
    // activity actor. It may be an external FEP-521a document; URL safety is
    // enforced during actor-key refresh. A spoofed activity id on a third
    // host must not ride an otherwise valid proof (Mitra's same-origin rule).
    let method = prepared.verification_method();
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, method).await? {
        return Ok(None);
    }
    if let Some(id) = activity.id.as_deref()
        && crate::remote::host_of(id) != crate::remote::host_of(actor_id)
    {
        return Ok(None);
    }

    let cached = account::find_by_uri(&state.pool, actor_id).await?;
    if let Some(account) = &cached
        && proof_matches_exact_key(&state.pool, &prepared, account).await?
    {
        return Ok(cached);
    }
    // Unknown actor, no stored Ed25519 key, or a stale one: dereference the
    // actor once (which refreshes the stored key) and retry. Any failure
    // degrades to the forwarded fallback rather than rejecting the delivery.
    let Ok(fetched) = state.federation.fetch_actor(actor_id).await else {
        return Ok(None);
    };
    let stored = refresh_remote_actor(state, &fetched).await?;
    if proof_matches_exact_key(&state.pool, &prepared, &stored).await? {
        return Ok(Some(stored));
    }
    Ok(None)
}

async fn proof_matches_exact_key(
    pool: &plamenu_db::PgPool,
    prepared: &plamenu_ap::proof::PreparedProof,
    account: &Account,
) -> Result<bool, ApiError> {
    let Some(key) = plamenu_db::actor_key::usable_by_uri(pool, prepared.verification_method())
        .await?
        .filter(|key| {
            key.account_id == Some(account.id)
                && key.controller_uri == account.uri.as_deref().unwrap_or_default()
        })
    else {
        return Ok(false);
    };
    let result = match (prepared.algorithm(), key.algorithm.as_str()) {
        (plamenu_ap::proof::ProofAlgorithm::Ed25519, "ed25519") => prepared.verify(&key.public_key),
        (plamenu_ap::proof::ProofAlgorithm::MlDsa44, "ml-dsa-44") => {
            prepared.verify_ml_dsa_44(&key.public_key)
        }
        _ => return Ok(false),
    };
    Ok(result.is_ok())
}

/// Whether the origin no longer serves `uri`: an authoritative 404/410, or a
/// Tombstone left in its place. A network failure confirms nothing.
async fn origin_confirms_gone(state: &AppState, uri: &str) -> Result<bool, ApiError> {
    match state.federation.fetch_object(uri).await {
        Ok(object) => Ok(object.get("type").and_then(Value::as_str) == Some("Tombstone")),
        Err(plamenu_federation::FederationError::Status(404 | 410)) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// A forwarded activity copy: HTTP-signed by a server other than the actor's.
/// Only what can be independently confirmed at the origin is acted on —
/// a `Create`/`Update` is replaced by fetching the authoritative object, and
/// a `Delete` (of a status, an actor, or a `QuoteAuthorization` stamp) only
/// applies once the origin confirms the object is gone. Everything else a
/// server might forward (Likes, Announces, …) is ignored, like Mastodon
/// ignores forwarded copies it cannot verify.
async fn handle_forwarded(
    state: &AppState,
    activity: &Activity,
    actor_id: &str,
) -> Result<(), ApiError> {
    let Some(object_uri) = activity.object_id().map(str::to_owned) else {
        return Ok(());
    };
    // A server may only speak for objects on the actor's own host.
    if crate::remote::host_of(&object_uri) != crate::remote::host_of(actor_id) {
        return Ok(());
    }
    match activity.kind.as_str() {
        // A forwarded post (e.g. a reply passed down a thread, or a LitePub
        // relay): ingest the origin's copy instead of the forwarded one.
        "Create" | "Update" => {
            if let Some(existing) = status::find_by_uri(&state.pool, &object_uri).await? {
                if let Some(updated) =
                    crate::ingest::refresh_remote_status(state, &existing, &object_uri).await?
                    && updated.edited_at != existing.edited_at
                {
                    crate::streaming::status_edited(state, updated.id).await;
                }
            } else if let Some(stored) =
                crate::ingest::resolve_or_fetch_status(state, &object_uri).await?
                && crate::streaming::within_realtime_window(&stored)
            {
                crate::streaming::status_created(state, stored.id).await;
            }
        }
        "Delete" => {
            // Fetch the origin only when the copy names something we hold:
            // a stored status or actor of the origin's, or a quote stamp.
            let Some(origin_actor) = account::find_by_uri(&state.pool, actor_id).await? else {
                return Ok(());
            };
            let names_actor = object_uri == actor_id;
            let names_status = status::find_by_uri(&state.pool, &object_uri)
                .await?
                .is_some_and(|row| row.account_id == origin_actor.id);
            let names_stamp = plamenu_db::quote::find_live_by_approval_uri(
                &state.pool,
                &object_uri,
                origin_actor.id,
            )
            .await?
            .is_some();
            if !(names_actor || names_status || names_stamp) {
                return Ok(());
            }
            if origin_confirms_gone(state, &object_uri).await? {
                // The forwarded copy itself was never authenticated; if a
                // group must relay this Delete onward, wrap a minimal
                // origin-confirmed rebuild rather than the untrusted copy.
                let confirmed = serde_json::json!({
                    "@context": plamenu_ap::AS_CONTEXT,
                    "id": activity.id,
                    "type": "Delete",
                    "actor": actor_id,
                    "object": object_uri,
                });
                handle_delete(state, &origin_actor, activity, &confirmed).await?;
            }
        }
        kind => {
            tracing::debug!(kind, actor = %actor_id, "ignoring forwarded activity type");
        }
    }
    Ok(())
}

/// FEP-7aa9 dispatch for `Add`/`Remove`: a `FeaturedCollection` (a whole
/// account collection) or a `FeaturedItem` (one membership). Returns whether
/// the activity was a collection change (so the caller skips the pinned-status
/// path). The collection must be owned by the (signature-verified) sender.
async fn handle_featured_collection_change(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    add: bool,
) -> Result<bool, ApiError> {
    let object_type = activity.object.get("type").and_then(Value::as_str);
    let target_uri = raw.get("target").and_then(id_of);

    if add {
        match object_type {
            Some("FeaturedCollection") => {
                crate::collections::process_remote_collection(state, sender, &activity.object)
                    .await?;
                return Ok(true);
            }
            Some("FeaturedItem") => {
                if let Some(target) = target_uri
                    && let Some(collection) =
                        crate::collections::owned_remote_collection(state, sender, target).await?
                {
                    crate::collections::add_remote_item(
                        state,
                        sender,
                        &collection,
                        &activity.object,
                    )
                    .await?;
                    return Ok(true);
                }
                return Ok(false);
            }
            _ => return Ok(false),
        }
    }

    // `Remove` carries a bare URI: a whole collection (object = its URI) or one
    // item (object = item URI, target = the collection URI).
    let Some(object_uri) = activity.object_id() else {
        return Ok(false);
    };
    if crate::collections::owned_remote_collection(state, sender, object_uri)
        .await?
        .is_some()
    {
        crate::collections::remove_remote_collection(state, sender, object_uri).await?;
        return Ok(true);
    }
    if let Some(target) = target_uri
        && let Some(collection) =
            crate::collections::owned_remote_collection(state, sender, target).await?
    {
        crate::collections::remove_remote_item(state, &collection, object_uri).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Inbound `Add`/`Remove` targeting the sender's featured collection: a
/// remote pin or unpin (Mastodon's `Activity::Add`/`Activity::Remove`). The
/// target must be exactly the featured-collection URL the actor advertises;
/// an unknown pinned status is fetched and ingested, like Mastodon's
/// `status_from_object`.
/// Activity types a FEP-171b conversation container wraps in its `Add`. An
/// `Add` whose `object` is one of these is a conversation item, not a featured
/// pin/collection/hashtag (whose `object` is a Note/FeaturedCollection/Hashtag).
const CONTAINER_WRAPPED_ACTIVITY_TYPES: &[&str] = &[
    "Create",
    "Update",
    "Delete",
    "Like",
    "Dislike",
    "EmojiReact",
    "EmojiReaction",
];

/// Whether an inbound `Add` is a conversation-container item (FEP-171b).
fn is_container_add(activity: &Activity) -> bool {
    activity
        .object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| CONTAINER_WRAPPED_ACTIVITY_TYPES.contains(&kind))
}

/// Consumes a FEP-171b container `Add`: unwrap the embedded activity, then
/// authenticate it exactly like a forwarded copy (FEP-fe34 / §Authentication) —
/// an author's own FEP-8b32 proof authenticates the embedded copy wholesale,
/// otherwise the authoritative object is re-fetched from its origin. Processing
/// unauthenticated embedded activities is refused (cache-poisoning guard: a
/// forged `Update(Actor)` in an `Add` could otherwise overwrite a cached
/// actor). The inner activity is then dispatched as if delivered directly, so
/// its own `context`/`inReplyTo` thread it into the conversation.
async fn handle_container_add(
    state: &AppState,
    _sender: &Account,
    activity: &Activity,
) -> Result<(), ApiError> {
    let Ok(inner) = serde_json::from_value::<Activity>(activity.object.clone()) else {
        return Ok(());
    };
    let Some(inner_actor) = inner.actor_id().map(str::to_owned) else {
        return Ok(());
    };
    if let Some(proven) = Box::pin(proof_verified_actor(
        state,
        &activity.object,
        &inner,
        &inner_actor,
    ))
    .await?
    {
        match inner.kind.as_str() {
            "Create" => handle_create(state, &proven, &inner, &activity.object, None).await?,
            "Update" => handle_update(state, &proven, &inner, &activity.object).await?,
            "Delete" => handle_delete(state, &proven, &inner, &activity.object).await?,
            "Like" => handle_like(state, &proven, &inner, &activity.object).await?,
            "EmojiReact" | "EmojiReaction" => {
                handle_emoji_react(state, &proven, &inner, &activity.object).await?;
            }
            "Dislike" => handle_dislike(state, &proven, &inner, &activity.object).await?,
            kind => tracing::debug!(kind, "ignoring container-wrapped activity type"),
        }
    } else {
        // No proof: only what the origin independently confirms is acted on.
        Box::pin(handle_forwarded(state, &inner, &inner_actor)).await?;
    }
    Ok(())
}

async fn handle_featured_change(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    add: bool,
) -> Result<(), ApiError> {
    // FEP-171b conversation container: an `Add` wrapping an activity (not a
    // pinned Note / featured collection / hashtag). Always consumed, regardless
    // of the owner-emission flag.
    if add && is_container_add(activity) {
        return Box::pin(handle_container_add(state, sender, activity)).await;
    }
    // A remote moderator's `Add`/`Remove` targeting our group's featured
    // (pin) or moderators collection.
    if let Some(group) = moderated_local_group(state, sender, activity).await? {
        return handle_group_collection_change(state, &group, activity, raw, add).await;
    }
    // FEP-7aa9: an `Add`/`Remove` of a whole `FeaturedCollection` or a single
    // `FeaturedItem` is dispatched ahead of the pinned-status handling.
    if handle_featured_collection_change(state, sender, activity, raw, add).await? {
        return Ok(());
    }
    let Some(target_uri) = raw.get("target").and_then(id_of) else {
        return Ok(());
    };
    if !is_featured_collection_of(state, sender, target_uri).await? {
        tracing::debug!(
            target = target_uri,
            "ignoring Add/Remove for a foreign target"
        );
        return Ok(());
    }
    // A featured hashtag (`Add`/`Remove` of a `Hashtag` object) — Mastodon
    // dispatches these on object type at the same featured-collection target.
    if activity.object.get("type").and_then(Value::as_str) == Some("Hashtag") {
        return handle_featured_hashtag(state, sender, &activity.object, add).await;
    }
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };

    if add {
        let item = match resolve_status_ref(state, object_uri).await? {
            Some(known) => known,
            None => match fetch_sender_note(state, sender, object_uri).await? {
                Some(fetched) => fetched,
                None => return Ok(()),
            },
        };
        // Only the (signature-verified) author may pin their own posts.
        if item.account_id != sender.id {
            return Ok(());
        }
        plamenu_db::pin::create(&state.pool, sender.id, item.id).await?;
        tracing::info!(actor = %sender.username, status = item.id, "remote pin recorded");
    } else if let Some(item) = resolve_status_ref(state, object_uri).await?
        && item.account_id == sender.id
    {
        plamenu_db::pin::delete(&state.pool, sender.id, item.id).await?;
    }
    Ok(())
}

/// Inbound `Add`/`Remove` of a featured `Hashtag` for the sender — Mastodon's
/// `add_featured_tags` / `remove_featured_tags`. The name comes from the
/// object's `name` (`#tag`); the tag is created if unseen so a remote account's
/// featured tags can be listed and re-served.
async fn handle_featured_hashtag(
    state: &AppState,
    sender: &Account,
    object: &Value,
    add: bool,
) -> Result<(), ApiError> {
    let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .map(|n| n.trim_start_matches(['#', '＃']).to_lowercase())
        .filter(|n| !n.is_empty())
    else {
        return Ok(());
    };
    let tag_id = plamenu_db::tag::ensure(&state.pool, &name).await?;
    if add {
        plamenu_db::featured_tag::feature(&state.pool, sender.id, tag_id).await?;
        tracing::info!(actor = %sender.username, tag = %name, "remote featured tag recorded");
    } else {
        plamenu_db::featured_tag::unfeature(&state.pool, sender.id, tag_id).await?;
    }
    Ok(())
}

/// Whether `target_uri` is the sender's featured collection. Accounts cached
/// before the featured URL was stored are re-dereferenced once to learn it.
async fn is_featured_collection_of(
    state: &AppState,
    sender: &Account,
    target_uri: &str,
) -> Result<bool, ApiError> {
    if let Some(stored) = account::featured_url_of(&state.pool, sender.id).await? {
        return Ok(stored == target_uri);
    }
    let Some(sender_uri) = sender.uri.as_deref() else {
        return Ok(false);
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, sender_uri)
        .await?
    {
        return Ok(false);
    }
    let Ok(actor) = state.federation.fetch_actor(sender_uri).await else {
        return Ok(false);
    };
    let refreshed = refresh_remote_actor(state, &actor).await?;
    Ok(account::featured_url_of(&state.pool, refreshed.id)
        .await?
        .is_some_and(|stored| stored == target_uri))
}

/// Fetches and ingests a pinned status we have not seen, with the same trust
/// rules as search-by-URL — but the author must be the sender itself.
async fn fetch_sender_note(
    state: &AppState,
    sender: &Account,
    object_uri: &str,
) -> Result<Option<status::Status>, ApiError> {
    if object_uri.starts_with(&format!("https://{}/", state.config.domain)) {
        return Ok(None); // never fetch ourselves
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, object_uri)
        .await?
    {
        return Ok(None);
    }
    let Ok(object) = state.federation.fetch_object(object_uri).await else {
        return Ok(None);
    };
    if !crate::ingest::is_ingestible_note(&object) {
        return Ok(None);
    }
    if !attributed_to_ids(&object).any(|author| Some(author) == sender.uri.as_deref()) {
        return Ok(None);
    }
    Ok(Some(
        crate::ingest::ingest_remote_note_in_context(
            state,
            sender,
            &object,
            crate::ingest::RemoteIngestContext::ExplicitResolution,
        )
        .await?,
    ))
}

/// Inbound `Move`: a remote account announces it has migrated to a new actor.
/// Our local followers re-follow the target (carrying their blocks and mutes),
/// like Mastodon's `Activity::Move`. The mover may only move *itself*
/// (`actor == object`); the target must list the origin in its `alsoKnownAs`;
/// a repeat `Move` to the same target is a no-op.
async fn handle_move(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let Some(sender_uri) = sender.uri.as_deref() else {
        return Ok(());
    };
    if activity.object_id() != Some(sender_uri) {
        return Ok(()); // only the actor itself may declare its move
    }
    let Some(target_uri) = raw.get("target").and_then(id_of) else {
        return Ok(());
    };
    // A degenerate self-move is all there is to refuse here. Deliberately NOT
    // gated on `moved_to_uri` already pointing at the target: an actor
    // document carries `movedTo` too, so any refresh of the source actor —
    // which happens on ordinary signature verification, usually before the
    // `Move` is even dequeued — would record the redirect and make the real
    // activity look like a replay, leaving every follower behind on the old
    // account. `process_move` is idempotent instead: it re-follows and
    // unfollows per remaining local follower, so a genuine replay finds
    // nothing left to move.
    if target_uri == sender_uri {
        return Ok(());
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, target_uri)
        .await?
    {
        return Ok(());
    }
    // The destination may be an account on *this* server: somebody moving
    // onto us. Their new home is the local row — fetching our own actor and
    // storing it as a remote one would leave our followers re-pointed at a
    // duplicate of an account we already host.
    let local_target =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, target_uri).await?;
    let target = if let Some(local) = local_target {
        // Same anti-hijack rule, read from where a local account keeps its
        // aliases instead of from an actor document we would be fetching
        // from ourselves.
        let aliases = plamenu_db::account_alias::list(&state.pool, local.id).await?;
        if !aliases.iter().any(|alias| alias.uri == sender_uri) {
            tracing::debug!(
                target = target_uri,
                "ignoring Move: local target does not alias the origin"
            );
            return Ok(());
        }
        local
    } else {
        let Ok(fetched) = state.federation.fetch_actor(target_uri).await else {
            return Ok(()); // target unreachable; nothing safe to do
        };
        // Anti-hijack: the destination must claim the origin as an alias.
        if !fetched
            .also_known_as_uris()
            .iter()
            .any(|alias| alias == sender_uri)
        {
            tracing::debug!(
                target = target_uri,
                "ignoring Move: target does not alias the origin"
            );
            return Ok(());
        }
        refresh_remote_actor(state, &fetched).await?
    };
    crate::migration::enqueue_move(state, sender, &target).await?;
    tracing::info!(source = %sender.username, target = %target.username, "remote account migration queued");
    Ok(())
}

/// Inbound `Block`: a remote actor blocks a local user. The block is
/// recorded (so the local user's content stays hidden from the blocker) and
/// the follows between the two are severed, like Mastodon's
/// `Activity::Block`.
async fn handle_block(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    // A remote moderator's community ban targets one of our hosted groups.
    if let Some(group) = moderated_local_group(state, sender, activity).await? {
        return handle_group_ban(state, &group, activity, raw).await;
    }
    let local = local_target(state, activity.object_id()).await?;
    follow::delete(&state.pool, sender.id, local.id).await?;
    follow::delete(&state.pool, local.id, sender.id).await?;
    plamenu_db::block::create(&state.pool, sender.id, local.id, activity.id.as_deref()).await?;
    tracing::info!(blocker = %sender.username, target = %local.username, "remote block recorded");
    Ok(())
}

/// Inbound `Flag`: a moderation report from a remote server, signed by its
/// instance actor. The `object` is the reported account's URI plus any
/// reported status URIs. We persist reports about our local users for staff to
/// review at `/admin/reports` (raising each managing staff member's
/// `admin.report` notification and firing the `report.created` webhook) and
/// ignore reports that name no local target.
async fn handle_flag(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    // A domain block with `reject_reports` drops the Flag before anything is
    // filed — Mastodon's `skip_reports?`.
    if !sender.is_portable_on(&state.config.domain)
        && let Some(domain) = sender.domain.as_deref()
        && plamenu_db::instance_policy::domain_rejects_reports(&state.pool, domain).await?
    {
        tracing::info!(reporter = %sender.username, domain, "report rejected by domain policy");
        return Ok(());
    }
    // A community report (`Flag` addressed to a local group) routes to that
    // group's moderators instead of instance staff.
    if let Some(group) = local_group_in_audience(state, activity).await? {
        return handle_group_flag(state, sender, &group, activity, raw).await;
    }
    let object_uris = flag_object_uris(&activity.object);
    if object_uris.is_empty() {
        return Ok(());
    }
    // Mastodon truncates an inbound report comment to its 5000-char limit.
    let comment: String = raw
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(5000)
        .collect();
    // The report's id is kept only when it lives on the sender's host, like
    // Mastodon's `report_uri`.
    let report_uri = match activity.id.as_deref() {
        Some(id)
            if crate::remote::host_of(id).is_some()
                && crate::remote::host_of(id)
                    == sender.uri.as_deref().and_then(crate::remote::host_of) =>
        {
            Some(id)
        }
        _ => None,
    };
    // A redelivered Flag must not file a second report (or re-fire its
    // webhook). Only uri-carrying reports can be recognised; anonymous ones
    // pass through, like every other server.
    if let Some(uri) = report_uri
        && report::exists_by_uri(&state.pool, uri).await?
    {
        return Ok(());
    }

    // Split the referenced URIs into our local target accounts and our local
    // statuses (grouped by author) — set-based, so a Flag packed with distinct
    // local URLs costs a fixed handful of queries, not one per URL (QC #28).
    let (targets, statuses_by_account) = resolve_flag_targets(state, &object_uris).await?;

    for target in &targets {
        let status_ids = statuses_by_account
            .get(&target.id)
            .cloned()
            .unwrap_or_default();
        let report = report::create(
            &state.pool,
            report::NewReport {
                account_id: sender.id,
                target_account_id: target.id,
                status_ids: &status_ids,
                comment: &comment,
                category: "other",
                forwarded: None,
                rule_ids: None,
                uri: report_uri,
            },
        )
        .await?;
        tracing::info!(reporter = %sender.username, target = %target.username, "remote report stored");
        crate::actions::notify_staff_about_report(state, &report).await?;
        crate::webhooks::report_event(state, webhook::REPORT_CREATED, &report).await;
    }
    Ok(())
}

/// The reported-object URIs of a `Flag`, whether given as an array (Mastodon's
/// shape) or a single value. Deduplicated and capped at [`MAX_FLAG_OBJECTS`]:
/// a single signed `Flag` can pack thousands of distinct local URLs into its
/// `object` array, and both report handlers resolve, file, and fire a webhook
/// per distinct local target — so the array is bounded before dispatch (QC
/// audit #28). A cap of 50 is far above any honest report (an account plus a
/// handful of its statuses) yet blunts the amplification.
fn flag_object_uris(object: &Value) -> Vec<String> {
    let items = match object {
        Value::Array(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if let Some(uri) = id_of(item)
            && seen.insert(uri)
        {
            out.push(uri.to_owned());
            if out.len() >= MAX_FLAG_OBJECTS {
                break;
            }
        }
    }
    out
}

/// Splits a `Flag`'s (bounded, deduplicated) object URIs into our local target
/// accounts — reported by their actor URL, in first-seen order — and our local
/// statuses grouped by author. Resolution is set-based: a fixed handful of
/// batched queries regardless of how many URIs one Flag packs, rather than a
/// lookup (or two or three) per URI as the former loop did.
async fn resolve_flag_targets(
    state: &AppState,
    object_uris: &[String],
) -> Result<(Vec<Account>, HashMap<i64, Vec<i64>>), ApiError> {
    // Classify each URI without touching the database: a local actor URL names
    // an account target; everything else is a candidate status reference.
    let mut actor_uris: Vec<&str> = Vec::new();
    let mut status_uris: Vec<&str> = Vec::new();
    for uri in object_uris {
        if crate::local_identity::has_local_actor_shape(&state.config.domain, uri) {
            actor_uris.push(uri);
        } else {
            status_uris.push(uri.as_str());
        }
    }

    let actors =
        crate::local_identity::find_actors(&state.pool, &state.config.domain, &actor_uris).await?;
    let mut targets: Vec<Account> = Vec::new();
    for uri in &actor_uris {
        if let Some(account) = actors.get(*uri)
            && !targets.iter().any(|t| t.id == account.id)
        {
            targets.push(account.clone());
        }
    }

    // Status candidates: match by stored URI in one query.
    let mut statuses = status::find_by_uris(&state.pool, &status_uris).await?;
    let matched: HashSet<&str> = statuses.iter().filter_map(|s| s.uri.as_deref()).collect();
    // Any URI not matched by stored URI may still be one of our own status
    // permalinks (`/users/{name}/statuses/{id}`); resolve those in batch too,
    // so no residual falls back to a per-URI lookup.
    let residual: Vec<(&str, String, i64)> = status_uris
        .iter()
        .filter(|uri| !matched.contains(**uri))
        .filter_map(|uri| {
            if let Some((account_id, status_id)) =
                urls::parse_local_numeric_status_url(&state.config.domain, uri)
            {
                Some((
                    *uri,
                    format!("https://{}/ap/accounts/{account_id}", state.config.domain),
                    status_id,
                ))
            } else {
                urls::parse_local_status_url(&state.config.domain, uri).map(
                    |(username, status_id)| {
                        (
                            *uri,
                            format!("https://{}/users/{username}", state.config.domain),
                            status_id,
                        )
                    },
                )
            }
        })
        .collect();
    if !residual.is_empty() {
        let owner_uri_refs: Vec<&str> = residual
            .iter()
            .map(|(_, owner_uri, _)| owner_uri.as_str())
            .collect();
        let owners =
            crate::local_identity::find_actors(&state.pool, &state.config.domain, &owner_uri_refs)
                .await?;
        let ids: Vec<i64> = residual.iter().map(|(_, _, id)| *id).collect();
        let by_id: HashMap<i64, status::Status> = status::find_by_ids(&state.pool, &ids)
            .await?
            .into_iter()
            .map(|stored| (stored.id, stored))
            .collect();
        for (_, owner_uri, status_id) in residual {
            if let Some(owner) = owners.get(&owner_uri)
                && let Some(stored) = by_id.get(&status_id)
                && stored.account_id == owner.id
                && !statuses.iter().any(|existing| existing.id == stored.id)
            {
                statuses.push(stored.clone());
            }
        }
    }

    // Keep only statuses authored by a local account, grouped by author — the
    // locality check the former loop did per status, now one batched lookup.
    let author_ids: Vec<i64> = statuses.iter().map(|stored| stored.account_id).collect();
    let local_authors: HashSet<i64> = account::find_by_ids(&state.pool, &author_ids)
        .await?
        .into_iter()
        .filter(|account| account.has_local_account_on(&state.config.domain))
        .map(|account| account.id)
        .collect();
    let mut statuses_by_account: HashMap<i64, Vec<i64>> = HashMap::new();
    for stored in statuses {
        if local_authors.contains(&stored.account_id) {
            statuses_by_account
                .entry(stored.account_id)
                .or_default()
                .push(stored.id);
        }
    }

    Ok((targets, statuses_by_account))
}

async fn handle_follow(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let local = local_target(state, activity.object_id()).await?;
    // A follow from someone the user blocks is auto-rejected, like
    // Mastodon's `Activity::Follow#reject_follow_request!`.
    let domain_blocked = match sender
        .domain
        .as_deref()
        .filter(|_| !sender.is_portable_on(&state.config.domain))
    {
        Some(domain) => {
            plamenu_db::account_domain_block::exists(&state.pool, local.id, domain).await?
        }
        None => false,
    };
    // A group ban works like a block: outcasts can't (re-)join.
    let outcast = local.is_group()
        && plamenu_db::group::affiliation_of(&state.pool, local.id, sender.id).await?
            == Some(plamenu_db::group::Affiliation::Outcast);
    if plamenu_db::block::exists(&state.pool, local.id, sender.id).await?
        || domain_blocked
        || outcast
    {
        let reject = plamenu_ap::activity::reject_follow(
            &state.config.domain,
            &local.username,
            plamenu_db::id::next(),
            raw.clone(),
        );
        job::enqueue_relationship_response(&state.pool, local.id, &sender.inbox_url, &reject)
            .await?;
        return Ok(());
    }
    // Repeat follows fast-forward, like Mastodon's `Activity::Follow`: an
    // existing request just refreshes its activity URI, an existing follow
    // is re-`Accept`ed under the new URI — neither re-notifies.
    if let Some(edge) = follow::find(&state.pool, sender.id, local.id).await? {
        if edge.pending {
            follow::create_request(&state.pool, sender.id, local.id, activity.id.as_deref())
                .await?;
        } else {
            let follow_row_id =
                follow::create(&state.pool, sender.id, local.id, activity.id.as_deref()).await?;
            let accept = accept_follow(&state.config.domain, &local.username, follow_row_id, raw);
            job::enqueue_relationship_response(&state.pool, local.id, &sender.inbox_url, &accept)
                .await?;
        }
        return Ok(());
    }

    // A locked target, or a sender limited by account/domain moderation, holds
    // the request for manual review; no Accept yet.  This prevents a silenced
    // actor from bypassing limited visibility by following every local user.
    // group's join requests wait in its moderation queue (the group has no user
    // of its own to notify); an ordinary locked account notifies itself.
    if local.locked || account::effectively_silenced(&state.pool, sender.id).await? {
        follow::create_request(&state.pool, sender.id, local.id, activity.id.as_deref()).await?;
        if !local.is_group() {
            notification::create(&state.pool, local.id, sender.id, "follow_request", None).await?;
        }
        tracing::info!(follower = %sender.username, target = %local.username, "new follow request");
        return Ok(());
    }

    let follow_row_id =
        follow::create(&state.pool, sender.id, local.id, activity.id.as_deref()).await?;
    tracing::info!(follower = %sender.username, target = %local.username, "new follower");

    // The delivery worker sends it (with retries); the inbox response never
    // waits on the remote server.
    notification::create(&state.pool, local.id, sender.id, "follow", None).await?;
    let accept = accept_follow(&state.config.domain, &local.username, follow_row_id, raw);
    job::enqueue_relationship_response(&state.pool, local.id, &sender.inbox_url, &accept).await?;
    Ok(())
}

async fn handle_undo(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let inner_kind = activity.object.get("type").and_then(Value::as_str);
    match inner_kind {
        Some("Follow") => {
            let inner_target = activity.object.get("object").and_then(id_of);
            if let Ok(local) = local_target(state, inner_target).await {
                follow::delete(&state.pool, sender.id, local.id).await?;
                // A withdrawn follow or follow request must not leave its
                // notification behind (Mastodon destroys it with the
                // Follow/FollowRequest row).
                notification::clear_kind_from(&state.pool, local.id, sender.id, "follow_request")
                    .await?;
                notification::clear_kind_from(&state.pool, local.id, sender.id, "follow").await?;
            }
        }
        Some("Like") => handle_undo_like(state, sender, &activity.object, raw).await?,
        Some("Dislike") => handle_undo_dislike(state, sender, &activity.object, raw).await?,
        Some("EmojiReact" | "EmojiReaction") => {
            handle_undo_emoji_react(state, sender, &activity.object).await?;
        }
        // Leniency only: Mobilizon withdraws an RSVP with a bare `Leave` and has
        // no `Undo(Join)` arm of its own, so we emit `Leave` — but accepting an
        // `Undo(Join)` costs one arm, and another event implementation may well
        // choose the `Undo` spelling.
        Some("Join") => {
            crate::events::handle_inbound_leave(
                state,
                sender,
                activity.object.get("id").and_then(Value::as_str),
                activity.object.get("object").and_then(id_of),
            )
            .await?;
        }
        Some("Announce") => {
            // The inner Announce's id is the boost status' uri.
            let undone = match activity.object.get("id").and_then(Value::as_str) {
                Some(announce_id) => undo_boost_by_uri(state, sender, announce_id).await?,
                None => false,
            };
            // …but a community announces the same post twice — the FEP-1b12
            // wrapper and the Mastodon-compat `Announce(Page)` beside it — and
            // the boost row keeps whichever id arrived first, while a
            // retraction can only reconstruct the compat one. So fall back to
            // what the Announce points *at*, which is unambiguous: an account
            // has at most one boost of a given post. Without this, a group's
            // removal of a post leaves it standing as community content on
            // every consumer that recorded the wrapper.
            if !undone
                && let Some(target_uri) = activity.object.get("object").and_then(id_of)
                && let Some(target) = crate::ingest::resolve_status_ref(state, target_uri).await?
            {
                undo_boost_of_target(state, sender, &target).await?;
            }
        }
        Some("Block") => {
            // A remote moderator lifting a community ban (Undo of the group
            // `Block`), addressed by the outer Undo's `audience`.
            if let Some(group) = moderated_local_group(state, sender, activity).await? {
                if let Some(target_uri) = activity.object.get("object").and_then(id_of)
                    && let Some(target) = account::find_by_uri(&state.pool, target_uri).await?
                {
                    crate::groups::apply_unban(state, group.id, target.id).await?;
                    crate::groups::announce_wrapped(state, &group, raw).await?;
                }
                return Ok(());
            }
            let inner_target = activity.object.get("object").and_then(id_of);
            if let Ok(local) = local_target(state, inner_target).await {
                plamenu_db::block::delete(&state.pool, sender.id, local.id).await?;
            }
        }
        Some("Lock") => {
            // A remote moderator reopening a thread (Undo of the group `Lock`).
            if let Some(group) = moderated_local_group(state, sender, activity).await?
                && let Some(object_uri) = activity.object.get("object").and_then(id_of)
                && let Some(status) = status::find_by_uri(&state.pool, object_uri).await?
            {
                let root = status::thread_root(&state.pool, status.id).await?;
                plamenu_db::group::unlock_thread(&state.pool, group.id, root).await?;
                crate::groups::announce_wrapped(state, &group, raw).await?;
            }
        }
        None => {
            // Pleroma names the undone activity by its bare id instead of
            // embedding it (Mastodon embeds). With no type to dispatch on,
            // look the uri up where inbound activities record it.
            if let Some(uri) = id_of(&activity.object) {
                handle_undo_by_uri(state, sender, uri, raw).await?;
            }
        }
        other => {
            tracing::debug!(?other, "ignoring Undo of unhandled type");
        }
    }
    Ok(())
}

/// Inbound `Undo(Like)` (embedded form): withdraws a favourite — or, for the
/// Misskey Like-reaction dialect, the reaction that Like created. Matched by
/// the Like's id first (reaction rows record the inbound activity uri), then
/// by the `(status, emoji)` the embedded Like names, and finally as a plain
/// favourite of the target — which also covers a Like whose reaction had
/// degraded to a favourite at ingest. That last step is guarded on the Like's
/// id too, except on a group post where the retraction is a vote and the
/// embedded Like is Lemmy's fabrication.
async fn handle_undo_like(
    state: &AppState,
    sender: &Account,
    object: &Value,
    raw: &Value,
) -> Result<(), ApiError> {
    if let Some(like_uri) = object.get("id").and_then(Value::as_str)
        && let Some(removed) =
            plamenu_db::reaction::delete_by_uri(&state.pool, sender.id, like_uri).await?
    {
        return finish_reaction_removal(state, sender, &removed).await;
    }
    let Some(liked_uri) = object.get("object").and_then(id_of) else {
        return Ok(());
    };
    let Some(target) = resolve_status_ref(state, liked_uri).await? else {
        return Ok(());
    };
    if let Some((name, _)) = parse_reaction(state, sender, object).await?
        && let Some(removed) =
            plamenu_db::reaction::delete_returning(&state.pool, sender.id, target.id, &name).await?
    {
        return finish_reaction_removal(state, sender, &removed).await;
    }
    // Lemmy's vote-clear is *always* an `Undo(Like)`, whatever the original
    // direction (its `UndoVote` hardcodes an embedded `Like`), so on a group
    // post an `Undo(Like)` also drops the sender's downvote. Mutual exclusion
    // means at most one of the two rows existed.
    let group_post = crate::groups::is_group_post(state, target.id).await?;
    // Which is also why a vote can only be matched by what it points at: the
    // `Like` Lemmy embeds is *fabricated* at retraction time, under a fresh
    // activity id it never sent us.
    //
    // An ordinary favourite is the opposite. Its `Undo` carries the original
    // `Like`'s id, and matching on it is what keeps a rapid
    // favourite/unfavourite/favourite from settling wrong: the three
    // activities are delivered concurrently, so the `Undo` naming the *first*
    // `Like` routinely arrives after the second one. Deleting by
    // (actor, status) then withdraws a favourite the actor still holds.
    let removed = match object.get("id").and_then(Value::as_str) {
        Some(undone) if !group_post => {
            favourite::delete_activity(&state.pool, sender.id, target.id, undone).await?
        }
        _ => favourite::delete(&state.pool, sender.id, target.id).await?,
    };
    // The favourite's notification goes with it (Mastodon destroys it with
    // the Favourite row); a remote author has no notification rows, so this
    // is a no-op then. Nothing removed means the favourite still stands, and
    // so must the notification that announced it.
    if removed.is_some() {
        notification::clear_kind_for_status(
            &state.pool,
            target.account_id,
            sender.id,
            "favourite",
            target.id,
        )
        .await?;
    }
    let removed_dislike = group_post
        && dislike::delete(&state.pool, sender.id, target.id)
            .await?
            .is_some();
    // A retracted vote on a group post is relayed like the vote was.
    if group_post && (removed.is_some() || removed_dislike) {
        crate::groups::announce_vote(state, &target, raw).await?;
    }
    Ok(())
}

/// Inbound `Undo(Dislike)`: retracts a downvote — matched by the
/// Dislike's id first, then by the named target — and relays the retraction
/// through the target's local groups like the vote was.
async fn handle_undo_dislike(
    state: &AppState,
    sender: &Account,
    object: &Value,
    raw: &Value,
) -> Result<(), ApiError> {
    let mut removed_status_id = match object.get("id").and_then(Value::as_str) {
        Some(dislike_uri) => dislike::delete_by_uri(&state.pool, sender.id, dislike_uri).await?,
        None => None,
    };
    if removed_status_id.is_none()
        && let Some(target_uri) = object.get("object").and_then(id_of)
        && let Some(target) = resolve_status_ref(state, target_uri).await?
        && dislike::delete(&state.pool, sender.id, target.id)
            .await?
            .is_some()
    {
        removed_status_id = Some(target.id);
    }
    if let Some(status_id) = removed_status_id
        && let Some(target) = status::find_by_id(&state.pool, status_id).await?
    {
        crate::groups::announce_vote(state, &target, raw).await?;
    }
    Ok(())
}

/// An `Undo` that only names the original activity's id: try each table that
/// stores inbound activity uris — boosts (the Announce id is the boost row's
/// uri), favourites, dislikes, emoji reactions — until one owns it.
async fn handle_undo_by_uri(
    state: &AppState,
    sender: &Account,
    uri: &str,
    raw: &Value,
) -> Result<(), ApiError> {
    if undo_boost_by_uri(state, sender, uri).await? {
        return Ok(());
    }
    if let Some(status_id) = favourite::delete_by_uri(&state.pool, sender.id, uri).await? {
        // The favourite's notification goes with it, as on the embedded-
        // object path; a remote author has no notification rows.
        if let Some(target) = status::find_by_id(&state.pool, status_id).await? {
            notification::clear_kind_for_status(
                &state.pool,
                target.account_id,
                sender.id,
                "favourite",
                target.id,
            )
            .await?;
            // A retracted upvote on a group post is relayed onward.
            if crate::groups::is_group_post(state, target.id).await? {
                crate::groups::announce_vote(state, &target, raw).await?;
            }
        }
        return Ok(());
    }
    if let Some(status_id) = dislike::delete_by_uri(&state.pool, sender.id, uri).await? {
        if let Some(target) = status::find_by_id(&state.pool, status_id).await? {
            crate::groups::announce_vote(state, &target, raw).await?;
        }
        return Ok(());
    }
    if let Some(removed) = plamenu_db::reaction::delete_by_uri(&state.pool, sender.id, uri).await? {
        finish_reaction_removal(state, sender, &removed).await?;
    }
    Ok(())
}

/// Removes `sender`'s boost whose row uri is `announce_uri` (an Announce's id
/// doubles as its boost row's uri), publishing the streaming delete and
/// clearing the `reblog` notification. Says whether a boost was deleted.
async fn undo_boost_by_uri(
    state: &AppState,
    sender: &Account,
    announce_uri: &str,
) -> Result<bool, ApiError> {
    let boost = status::find_by_uri(&state.pool, announce_uri)
        .await?
        .filter(|row| row.account_id == sender.id);
    let Some(boost) = boost else {
        return Ok(false);
    };
    remove_boost_row(state, sender, &boost).await
}

/// Removes `sender`'s boost of `target`, whatever id its row was recorded
/// under — the fallback when an `Undo(Announce)` names an Announce id we
/// never stored.
async fn undo_boost_of_target(
    state: &AppState,
    sender: &Account,
    target: &status::Status,
) -> Result<bool, ApiError> {
    let Some(boost) = status::find_reblog_by(&state.pool, sender.id, target.id).await? else {
        return Ok(false);
    };
    remove_boost_row(state, sender, &boost).await
}

/// Deletes one boost row and its side effects.
async fn remove_boost_row(
    state: &AppState,
    sender: &Account,
    boost: &status::Status,
) -> Result<bool, ApiError> {
    let delete_event = crate::streaming::prepare_delete_event(state, boost).await?;
    // A boost recorded from an Announce carries that Announce's id as its
    // uri; only a boost this server made itself has none.
    let deleted = match boost.uri.as_deref() {
        Some(uri) => status::delete_by_uri(&state.pool, uri, sender.id).await?,
        None => status::delete_local(&state.pool, boost.id, sender.id)
            .await?
            .is_some(),
    };
    if !deleted {
        return Ok(false);
    }
    crate::streaming::publish(state, &delete_event).await;
    // The boost's `reblog` notification points at the original status (not
    // the boost row, which is gone), so it must be cleared explicitly —
    // Mastodon's cascades from the boost row instead.
    if let Some(original_id) = boost.reblog_of_id
        && let Some(original) = status::find_by_id(&state.pool, original_id).await?
    {
        notification::clear_kind_for_status(
            &state.pool,
            original.account_id,
            sender.id,
            "reblog",
            original.id,
        )
        .await?;
    }
    Ok(true)
}

async fn handle_create(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
    delivered_to_account_id: Option<i64>,
) -> Result<(), ApiError> {
    let fetched_object = if let Some(object_uri) = activity.object.as_str() {
        let Some(sender_uri) = sender.uri.as_deref() else {
            return Ok(());
        };
        let (Some(object_host), Some(sender_host)) = (host_of(object_uri), host_of(sender_uri))
        else {
            tracing::debug!(
                uri = object_uri,
                "ignoring Create with an invalid object origin"
            );
            return Ok(());
        };
        if object_host != sender_host {
            tracing::debug!(
                uri = object_uri,
                actor = sender_uri,
                "ignoring Create whose object is hosted by another origin"
            );
            return Ok(());
        }
        let fetch_account_id = match delivered_to_account_id {
            Some(account_id) => Some(account_id),
            None => signed_fetch_recipient(state, sender, raw).await?,
        };
        let Some(object) = fetch_remote_status_value(state, object_uri, fetch_account_id).await?
        else {
            return Ok(());
        };
        Some(object)
    } else {
        None
    };
    let object = fetched_object.as_ref().unwrap_or(&activity.object);
    if !crate::ingest::is_ingestible_note(object) {
        return Ok(());
    }
    let Some(object_uri) = id_of(object) else {
        return Ok(());
    };
    // The note must belong to the (verified) sender of the activity.
    if !attributed_to_ids(object).any(|author| Some(author) == sender.uri.as_deref()) {
        tracing::debug!(uri = object_uri, "ignoring Create for a foreign object");
        return Ok(());
    }
    // A reply naming an option of one of our polls is a vote, not a post.
    if crate::polls::handle_inbound_vote(state, sender, object).await? {
        return Ok(());
    }
    let crate::ingest::RemoteIngestResult {
        status: stored,
        delivery_effects,
        mention_notify,
        quote_notify,
    } = crate::ingest::ingest_remote_note_delivery_deferred(state, sender, object).await?;
    if !delivery_effects {
        return Ok(());
    }
    // A submission addressed to one of our groups: validate the sender
    // and announce it to the group's followers, wrapping the delivered
    // activity verbatim. Only a *fresh* ingest reaches this point (the
    // early-return above), so a redelivery never re-announces.
    crate::groups::process_inbound_submission(state, sender, &stored, raw).await?;
    // Per-follow `notify` subscribers hear about the sender's new post —
    // like local posting, only for the activity's own note (backfilled
    // ancestors are old posts), and even outside the realtime window,
    // matching Mastodon's fan-out.
    let mut post_notifications: Vec<notification::PostNotification> = mention_notify
        .into_iter()
        .map(notification::PostNotification::mention)
        .collect();
    if let Some(account_id) = quote_notify {
        post_notifications.push(notification::PostNotification {
            account_id,
            quote: true,
            ..notification::PostNotification::default()
        });
    }
    post_notifications.extend(
        crate::actions::new_status_follower_ids(state, &stored)
            .await?
            .into_iter()
            .map(notification::PostNotification::status),
    );
    notification::create_post_notifications_many(
        &state.pool,
        &post_notifications,
        stored.account_id,
        stored.id,
    )
    .await?;
    // Only the activity's own note is announced — backfilled thread
    // ancestors are old posts and must not surface as live updates — and
    // only while it is fresh (Mastodon's realtime window): a replayed
    // backlog of old posts notifies but must not surface on live timelines.
    if crate::streaming::within_realtime_window(&stored) {
        crate::streaming::status_created(state, stored.id).await;
    }
    Ok(())
}

/// Inbound `Update`: a profile change (embedded actor) or a status edit.
/// The signature-verified sender may only update itself and its own posts.
async fn handle_update(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    let object = &activity.object;
    match object.get("type").and_then(Value::as_str) {
        _ if is_ingestible_note(object) => {
            let Some(object_uri) = id_of(object) else {
                return Ok(());
            };
            if !attributed_to_ids(object).any(|author| Some(author) == sender.uri.as_deref()) {
                tracing::debug!(uri = object_uri, "ignoring Update for a foreign note");
                return Ok(());
            }
            // Edits only apply to statuses we already have; an Update for an
            // unknown status is not a Create (Mastodon may re-send old posts
            // this way, e.g. quote-stamp redistribution).
            let Some(existing) = status::find_by_uri(&state.pool, object_uri).await? else {
                return Ok(());
            };
            if existing.account_id != sender.id {
                return Ok(());
            }
            // A stale Update — older than an edit already applied — is
            // rejected whole (content, tallies and all), so a replayed
            // backlog can never roll an edited post back: Mastodon's
            // `already_updated_more_recently?`.
            let incoming_updated = object
                .get("updated")
                .and_then(Value::as_str)
                .and_then(|s| time::OffsetDateTime::parse(s, &Rfc3339).ok());
            if let Some(stored_edited_at) = existing.edited_at
                && incoming_updated.is_none_or(|updated| updated < stored_edited_at)
            {
                return Ok(());
            }
            // Poll tallies refresh even when nothing else changed — vote
            // updates arrive as Update(Question) with identical content.
            crate::polls::refresh_remote_poll(state, &existing, object).await?;
            let updated = update_remote_note(state, sender, &existing, object).await?;
            // A no-change Update (e.g. quote-stamp redistribution) is not
            // an edit and must not stream one — nor re-announce it to any
            // group the submission belongs to.
            if updated.edited_at != existing.edited_at {
                crate::streaming::status_edited(state, updated.id).await;
                crate::groups::reannounce_to_groups(state, &updated, raw).await?;
            }
        }
        Some("Person" | "Service" | "Application" | "Group" | "Organization") => {
            if id_of(object) != sender.uri.as_deref() {
                tracing::debug!("ignoring Update for a foreign actor");
                return Ok(());
            }
            let Ok(actor) = serde_json::from_value::<RemoteActor>(object.clone()) else {
                return Ok(());
            };
            // Never store a key claimed to belong to someone else.
            if actor.public_key.owner != actor.id {
                return Ok(());
            }
            let stored = refresh_remote_actor(state, &actor).await?;
            crate::remote::spawn_featured_tags_sync(state, &stored, actor.featured_tags_uri());
            crate::remote::spawn_outbox_stats_sync(state, &stored, actor.outbox_url());
            crate::remote::spawn_group_moderators_sync(state, &stored, &actor);
            tracing::info!(actor = %sender.username, "remote profile updated");
        }
        Some("FeaturedCollection") => {
            crate::collections::process_remote_collection(state, sender, object).await?;
        }
        other => {
            tracing::debug!(?other, "ignoring Update of unhandled type");
        }
    }
    Ok(())
}

async fn handle_delete(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
    raw: &Value,
) -> Result<(), ApiError> {
    // FEP-7aa9: a remote member revoking the consent it granted to one of our
    // local collections (`Delete(FeatureAuthorization)`).
    if activity.object.get("type").and_then(Value::as_str) == Some("FeatureAuthorization") {
        if let Some(stamp_uri) = id_of(&activity.object) {
            crate::collections::process_feature_authorization_delete(state, sender, stamp_uri)
                .await?;
        }
        return Ok(());
    }
    // FEP-044f: the quoted author deletes the `QuoteAuthorization` stamp it
    // issued, revoking consent for an already-authorized quote.
    if activity.object.get("type").and_then(Value::as_str) == Some("QuoteAuthorization") {
        if let Some(stamp_uri) = id_of(&activity.object) {
            process_quote_revocation(state, sender, stamp_uri).await?;
        }
        return Ok(());
    }
    // A group moderator's mod-removal: Lemmy's `Delete` carrying a `summary`
    // (the reason) is a moderator action, not an author's own delete. Retract
    // the post from the community; the status itself survives.
    if raw.get("summary").is_some()
        && let Some(group) = moderated_local_group(state, sender, activity).await?
        && let Some(object_uri) = activity.object_id()
        && let Some(status) = status::find_by_uri(&state.pool, object_uri).await?
    {
        crate::groups::announce_wrapped(state, &group, raw).await?;
        crate::groups::apply_remove(state, &group, &status).await?;
        tracing::info!(group = %group.username, status = status.id, "remote group post removal applied");
        return Ok(());
    }
    let Some(object_uri) = activity.object_id() else {
        return Ok(());
    };
    if Some(object_uri) == sender.uri.as_deref() {
        account::delete_by_uri(&state.pool, object_uri).await?;
    } else {
        // The streaming event's routing data, and a DM's conversation, must
        // be captured before the delete cascades those rows away.
        let existing = status::find_by_uri(&state.pool, object_uri)
            .await?
            .filter(|row| row.account_id == sender.id);
        let (delete_event, conversation_id) = match &existing {
            Some(row) => (
                Some(crate::streaming::prepare_delete_event(state, row).await?),
                plamenu_db::conversation::of_status(&state.pool, row.id)
                    .await?
                    .map(|conversation| (conversation, row.id)),
            ),
            None => (None, None),
        };
        // A group submission's retraction data must be captured before the
        // delete cascades the mention and boost rows away.
        let (group_accounts, group_boosts) = match &existing {
            Some(row) => (
                crate::groups::group_accounts_of_status(state, row).await?,
                crate::groups::capture_boosts(state, row, sender).await?,
            ),
            None => (Vec::new(), Vec::new()),
        };
        let deleted = status::stub_by_uri(&state.pool, object_uri, sender.id).await?;
        if deleted
            && existing
                .as_ref()
                .is_some_and(|row| row.visibility == "public")
        {
            // The author's Delete travels onward wrapped in the group's
            // Announce (FEP-1b12 consumers), and the compat bare Announce is
            // undone (plain-Announce consumers drop the boost).
            for group_account in &group_accounts {
                crate::groups::announce_wrapped(state, group_account, raw).await?;
            }
            for captured in &group_boosts {
                crate::groups::retract_boost(state, captured).await?;
            }
        }
        if let Some((conversation, status_id)) = conversation_id {
            plamenu_db::conversation::remove_status(&state.pool, conversation, status_id).await?;
        }
        if deleted && let Some(event) = &delete_event {
            crate::streaming::publish(state, event).await;
        }
        // A bare-id Delete that named no status may still be a quote-stamp
        // revocation — Mastodon's `delete_status || revoke_quote` fallback.
        if !deleted {
            process_quote_revocation(state, sender, object_uri).await?;
        }
    }
    Ok(())
}

/// The (signature-verified) quoted author withdrew a quote authorization it
/// had issued: the quote row loses its stamp (accepted → `revoked`,
/// pending → `rejected`), and a local quoting post is re-federated so remotes
/// drop the embed — the inbound mirror of `actions::revoke_quote`.
async fn process_quote_revocation(
    state: &AppState,
    sender: &Account,
    stamp_uri: &str,
) -> Result<(), ApiError> {
    let Some(revoked) =
        plamenu_db::quote::revoke_by_approval_uri(&state.pool, stamp_uri, sender.id).await?
    else {
        return Ok(());
    };
    tracing::info!(quote = revoked.id, quoted = %sender.username, "quote authorization revoked");
    let Some(status_id) = revoked.status_id else {
        return Ok(());
    };
    if let Some(quoting) = status::find_by_id(&state.pool, status_id).await?
        && let Some(author) = account::find_by_id(&state.pool, quoting.account_id).await?
        && author.is_local()
    {
        let inboxes = crate::actions::remote_mention_inboxes(state, quoting.id).await?;
        crate::actions::federate_note_update(
            state,
            &author,
            &quoting,
            time::OffsetDateTime::now_utc(),
            inboxes,
        )
        .await?;
    }
    // Timelines re-render the quoting post without its embed.
    crate::streaming::status_edited(state, status_id).await;
    Ok(())
}

/// Extracts the local username a Follow/Undo points at, rejecting objects
/// that are not this instance's actor URLs.
async fn local_target(state: &AppState, object_id: Option<&str>) -> Result<Account, ApiError> {
    let object_id =
        object_id.ok_or_else(|| ApiError::BadRequest("activity has no object".into()))?;
    crate::local_identity::find_actor(&state.pool, &state.config.domain, object_id)
        .await?
        .ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flag_object_uris_reads_array_and_singleton() {
        // The Mastodon array shape, mixing bare strings and `{id}` objects.
        assert_eq!(
            flag_object_uris(&json!([
                "https://h.example/users/a",
                { "id": "https://h.example/users/b", "type": "Note" },
            ])),
            ["https://h.example/users/a", "https://h.example/users/b"],
        );
        // A single bare value (some servers don't wrap in an array).
        assert_eq!(
            flag_object_uris(&json!("https://h.example/users/solo")),
            ["https://h.example/users/solo"],
        );
        // Nothing addressable.
        assert!(flag_object_uris(&json!(42)).is_empty());
    }

    #[test]
    fn flag_object_uris_deduplicates() {
        // A repeated URL must not file (or re-webhook) a second report.
        assert_eq!(
            flag_object_uris(&json!([
                "https://h.example/users/a",
                "https://h.example/users/a",
                "https://h.example/users/b",
                "https://h.example/users/a",
            ])),
            ["https://h.example/users/a", "https://h.example/users/b"],
        );
    }

    #[test]
    fn flag_object_uris_caps_the_fan_out() {
        // A hostile Flag packs far more distinct targets than any honest report.
        let many: Vec<String> = (0..5_000)
            .map(|n| format!("https://h.example/users/u{n}"))
            .collect();
        let uris = flag_object_uris(&json!(many));
        assert_eq!(uris.len(), MAX_FLAG_OBJECTS);
        // The cap keeps the first-seen prefix, so it is deterministic.
        assert_eq!(uris[0], "https://h.example/users/u0");
    }
}
