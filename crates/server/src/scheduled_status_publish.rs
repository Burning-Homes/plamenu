//! Publishes leased scheduled posts through the normal posting transaction.
//! Failed attempts stay queued; expired claims can be retried without creating
//! duplicate posts or delivery jobs.

use plamenu_db::{account, scheduled_status};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::actions::{self, PollParams, PostParams};
use crate::error::ApiError;

const BATCH_SIZE: i64 = 20;
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Claims and publishes one batch of due scheduled statuses; returns how many
/// were claimed (0 = nothing is currently due).
pub async fn run_due(state: &AppState) -> u64 {
    let due = match scheduled_status::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "failed to claim due scheduled statuses");
            return 0;
        }
    };
    let claimed = due.len() as u64;
    for row in due {
        match publish_claimed(state, &row).await {
            Ok(()) => {}
            Err(ApiError::Conflict(error)) => {
                tracing::debug!(%error, scheduled = row.id, "scheduled claim superseded");
            }
            Err(error) => {
                tracing::warn!(error = %error.chain(), scheduled = row.id,
                    "scheduled status publish failed; retained for retry");
            }
        }
    }
    claimed
}

/// Publishes one leased entry; safe to retry after an ambiguous result.
pub async fn publish_claimed(
    state: &AppState,
    row: &scheduled_status::ScheduledStatus,
) -> Result<(), ApiError> {
    let Some(author) = account::find_by_id(&state.pool, row.account_id).await? else {
        return Ok(());
    };
    if !author.is_local() {
        return Ok(());
    }
    let poll = row.poll_options.as_ref().map(|options| PollParams {
        options: options.clone(),
        expires_in: row.poll_expires_in,
        multiple: row.poll_multiple,
        hide_totals: row.poll_hide_totals,
    });
    let params = PostParams {
        username: &author.username,
        text: &row.text,
        visibility: &row.visibility,
        in_reply_to_id: row.in_reply_to_id,
        media_ids: &row.media_ids,
        quoted_status_id: row.quoted_status_id,
        spoiler_text: &row.spoiler_text,
        sensitive: row.sensitive,
        language: row.language.as_deref(),
        content_type: crate::compose::PostFormat::from_media_type(&row.content_type),
        poll,
        // Resolved at schedule time; a pre-column row (None) falls back to
        // the visibility default, as it would have before.
        quote_approval_policy: row.quote_approval_policy,
        // Scheduling a group submission isn't supported (yet): the membership
        // check must hold at publish time, not schedule time.
        group_id: None,
        title: row.title.as_deref(),
        external_url: None,
        event: None,
        kind: if row.object_type == "Article" {
            plamenu_ap::activity::PostKind::Article
        } else {
            plamenu_ap::activity::PostKind::Note
        },
    };
    actions::post_scheduled_status(state, params, row).await?;
    tracing::info!(scheduled = row.id, "scheduled status published");
    Ok(())
}

/// Runs the publish sweep until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("scheduled status publish sweeper started");
        loop {
            // Drain everything currently due, then sleep out the interval.
            while run_due(&state).await > 0 {}
            if !crate::workers::pause(&state, SWEEP_INTERVAL).await {
                return;
            }
        }
    })
}
