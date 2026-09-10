//! Conversation bookkeeping for direct statuses — shared by the compose
//! path and inbound ingestion, so a DM lands in the same conversation row no
//! matter which side of the federation it came from.

use plamenu_db::account::Account;
use plamenu_db::conversation::{ContextRefs, EnsureConversation};
use plamenu_db::notification_policy::{self, Disposition};
use plamenu_db::status::Status;
use plamenu_db::{conversation, mention};

use crate::AppState;
use crate::error::ApiError;

/// Opens (or inherits, for a reply) the conversation a freshly stored status
/// belongs to. Called for every status before its mentions fan out, so any
/// thread can be muted and a reply into a muted thread is suppressed.
/// `refs` carries the FEP `context`/`contextHistory` IRIs an inbound object
/// declares (empty for a locally-authored status); they let a remote thread
/// converge by shared context even when the reply parent is missing, and stamp
/// a root's owner/identity. Idempotent; the direct fan-out
/// ([`record_direct_status`]) layers on top.
pub async fn ensure_conversation(
    conn: &mut plamenu_db::PgConnection,
    status: &Status,
    is_reply: bool,
    refs: ContextRefs<'_>,
) -> Result<i64, ApiError> {
    let id = conversation::ensure_for_status_conn(
        conn,
        &EnsureConversation {
            status_id: status.id,
            account_id: status.account_id,
            in_reply_to_id: status.in_reply_to_id,
            is_reply,
            refs,
        },
    )
    .await?;
    Ok(id)
}

/// For a direct status, updates the `AccountConversation` row of every local
/// participant (the DM inbox fan-out). A no-op for non-direct statuses, whose
/// conversation [`ensure_conversation`] has already opened. The status'
/// mentions must already be persisted. Idempotent — re-delivered activities
/// change nothing.
pub async fn record_direct_status(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    author: &Account,
    status: &Status,
) -> Result<(), ApiError> {
    if status.visibility != "direct" {
        return Ok(());
    }
    // The mapping already exists (ensure_conversation ran first), so refs are
    // irrelevant here — the existing conversation id is returned. Runs on `conn`
    // so it reads the mapping and mentions this transaction just wrote.
    let conversation_id = conversation::ensure_for_status_conn(
        conn,
        &EnsureConversation {
            status_id: status.id,
            account_id: status.account_id,
            in_reply_to_id: status.in_reply_to_id,
            is_reply: status.in_reply_to_id.is_some(),
            refs: ContextRefs::default(),
        },
    )
    .await?;
    let mentioned = mention::for_statuses(&mut *conn, &[status.id], false)
        .await?
        .remove(&status.id)
        .unwrap_or_default();

    // The participants: author plus everyone mentioned, locality remembered
    // so only local accounts get a conversation row.
    let mut participants: Vec<(i64, bool)> = vec![(author.id, author.is_local())];
    for account in &mentioned {
        if !participants.iter().any(|(id, _)| *id == account.id) {
            participants.push((account.id, account.is_local()));
        }
    }
    for (account_id, is_local) in &participants {
        if !is_local {
            continue;
        }
        if should_withhold_direct_row(state, *account_id, status).await? {
            continue;
        }
        let others: Vec<i64> = participants
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| id != account_id)
            .collect();
        conversation::add_status(
            &mut *conn,
            conversation::AddStatus {
                account_id: *account_id,
                conversation_id,
                participant_account_ids: &others,
                status_id: status.id,
                sender_id: status.account_id,
            },
        )
        .await?;
    }
    Ok(())
}

async fn should_withhold_direct_row(
    state: &AppState,
    account_id: i64,
    status: &Status,
) -> Result<bool, ApiError> {
    if account_id == status.account_id {
        return Ok(false);
    }
    let disposition = notification_policy::evaluate(
        &state.pool,
        account_id,
        status.account_id,
        "mention",
        Some(status.id),
    )
    .await?;
    Ok(disposition != Disposition::Accept)
}
