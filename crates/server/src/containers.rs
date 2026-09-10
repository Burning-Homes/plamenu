//! FEP-171b conversation containers — Plamenu as the owner of a private
//! conversation: advertising `contextHistory`, serving the collection of
//! `Add(Activity)` (the container), and distributing `Add` to the audience.
//! Owner emission is gated behind the effective `conversation_containers`
//! flag (admin override, else the config file; off by default).
//! Consuming inbound `Add` is always on and lives in the inbox
//! (`handle_container_add`).

use plamenu_ap::activity;
use plamenu_ap::urls::{LocalUserUrls, context_history_uri};
use plamenu_db::account::{self, Account};
use plamenu_db::conversation::{Conversation, StatusContext};
use plamenu_db::status::{self, Status};
use plamenu_db::{conversation, job};
use serde_json::{Value, json};

use crate::AppState;
use crate::entities::account_uri;
use crate::error::ApiError;

/// The `contextHistory` a locally-owned conversation advertises, or `None`.
///
/// Only a private/direct conversation whose container we run (the flag is on)
/// has one: public threads travel by the FEP-f228 posts collection, and a
/// remote-owned conversation's container is the owner's, not ours.
pub async fn local_history_uri(state: &AppState, ctx: &StatusContext) -> Option<String> {
    local_history_uri_when(state, ctx, state.conversation_containers().await)
}

/// [`local_history_uri`] with the container flag already resolved — so a page
/// of Notes reads the setting once rather than once per row.
#[must_use]
pub fn local_history_uri_when(
    state: &AppState,
    ctx: &StatusContext,
    containers_enabled: bool,
) -> Option<String> {
    if !containers_enabled || ctx.uri.is_some() {
        return None;
    }
    let private = ctx
        .root_visibility
        .as_deref()
        .is_some_and(|v| matches!(v, "private" | "direct"));
    private.then(|| context_history_uri(&state.config.domain, ctx.conversation_id))
}

/// The audience IRIs a private conversation's `Add` activities address: the
/// owner's followers collection for a followers-only thread, or the root's
/// named participants for a direct thread.
async fn conversation_audience(
    state: &AppState,
    owner: &Account,
    root: &Status,
) -> Result<Vec<String>, ApiError> {
    let domain = &state.config.domain;
    if root.visibility == "direct" {
        let mut uris = Vec::new();
        if let Some(mentioned) = plamenu_db::mention::for_statuses(&state.pool, &[root.id], false)
            .await?
            .remove(&root.id)
        {
            for account in mentioned {
                uris.push(account_uri(domain, &account));
            }
        }
        Ok(uris)
    } else {
        Ok(vec![
            LocalUserUrls::for_account(domain, &owner.username, owner.uri.as_deref()).followers,
        ])
    }
}

/// One container item: the owner's `Add` wrapping the `Create` of `post`. A
/// local post's Note is inlined; a remote post is referenced by its `uri` (we
/// cannot re-mint a foreign author's Create), enough for a consumer to fetch
/// and authenticate it from its own origin.
fn container_item(
    state: &AppState,
    owner_uri: &str,
    container_uri: &str,
    audience: &[String],
    post: &Status,
    author: &Account,
    batch: &crate::note::NoteBatch,
) -> Result<Value, ApiError> {
    let domain = &state.config.domain;
    let create = if post.uri.is_none() {
        let note = crate::note::note_in_batch(state, post, author, batch)?;
        activity::create_with_note(domain, &author.username, note)
    } else {
        let post_uri = post.uri.clone().unwrap_or_default();
        json!({
            "type": "Create",
            "id": format!("{post_uri}/activity"),
            "actor": account_uri(domain, author),
            "object": post_uri,
        })
    };
    let add_id = format!("{container_uri}/{}", post.id);
    Ok(activity::container_add(
        &add_id,
        owner_uri,
        container_uri,
        &create,
        audience,
    ))
}

/// Builds a page of container items (`Add(Create(...))`) for the conversation's
/// posts after `after_id`. The caller has already authorized the requester.
pub async fn history_items(
    state: &AppState,
    conv: &Conversation,
    owner: &Account,
    root: &Status,
    after_id: i64,
    limit: i64,
) -> Result<Vec<Value>, ApiError> {
    let domain = &state.config.domain;
    let container_uri = context_history_uri(domain, conv.id);
    let owner_uri = account_uri(domain, owner);
    let audience = conversation_audience(state, owner, root).await?;
    let posts = status::conversation_page(&state.pool, conv.id, after_id, limit).await?;
    if posts.is_empty() {
        return Ok(Vec::new());
    }
    // A page of 60 used to refetch its authors once per post — a two-person DM
    // thread fetched the same two accounts sixty times — and rebuild every Note
    // sidecar per row. Both load once for the page.
    let authors = account::find_by_ids(
        &state.pool,
        &posts.iter().map(|post| post.account_id).collect::<Vec<_>>(),
    )
    .await?;
    let local: Vec<&Status> = posts.iter().filter(|post| post.uri.is_none()).collect();
    let batch = crate::note::NoteBatch::load(state, &local).await?;
    let mut items = Vec::with_capacity(posts.len());
    for post in &posts {
        let author = authors
            .iter()
            .find(|a| a.id == post.account_id)
            .ok_or(ApiError::NotFound)?;
        items.push(container_item(
            state,
            &owner_uri,
            &container_uri,
            &audience,
            post,
            author,
            &batch,
        )?);
    }
    Ok(items)
}

/// Active distribution: when a local reply lands in a private
/// conversation **we own**, the owner wraps its `Create` in an `Add` and sends
/// it to the conversation audience, so container-aware participants stay
/// synchronized. A no-op when the flag is off, the conversation is remote-owned
/// or public, or there is no audience to reach. Reuses the reply's delivery
/// inboxes (which already include the inherited participants).
pub async fn distribute_reply_add(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
    create: &Value,
    audience_uris: &[String],
    inboxes: &[String],
) -> Result<(), ApiError> {
    if !state.conversation_containers().await || inboxes.is_empty() {
        return Ok(());
    }
    // The reply's conversation mapping was just written on `conn`; the owner
    // lookup below is committed data.
    let Some(ctx) = conversation::context_of_status(&mut *conn, stored.id).await? else {
        return Ok(());
    };
    // Only a private/direct conversation we own (uri IS NULL, local owner).
    if ctx.uri.is_some()
        || !ctx
            .root_visibility
            .as_deref()
            .is_some_and(|v| matches!(v, "private" | "direct"))
    {
        return Ok(());
    }
    let Some(owner) = ctx_owner(state, &ctx).await? else {
        return Ok(());
    };
    // The owner is authoritative for the container: it signs and distributes
    // the `Add`, even when a different local participant authored the reply.
    let domain = &state.config.domain;
    let container_uri = context_history_uri(domain, ctx.conversation_id);
    let owner_uri = account_uri(domain, &owner);
    let add_id = format!("{container_uri}/{}", stored.id);
    let add = activity::container_add(&add_id, &owner_uri, &container_uri, create, audience_uris);
    for inbox in inboxes {
        job::enqueue_tx(&mut *conn, owner.id, inbox, &add).await?;
    }
    Ok(())
}

/// The local owner account of a conversation, if it is locally owned.
async fn ctx_owner(state: &AppState, ctx: &StatusContext) -> Result<Option<Account>, ApiError> {
    let Some(conv) = conversation::find(&state.pool, ctx.conversation_id).await? else {
        return Ok(None);
    };
    let Some(owner_id) = conv.owner_account_id else {
        return Ok(None);
    };
    Ok(account::find_by_id(&state.pool, owner_id)
        .await?
        .filter(Account::is_local))
}
