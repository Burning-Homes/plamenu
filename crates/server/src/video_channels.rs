//! Native channel lifecycle and explicit, recipient-consented authority.
use crate::{AppState, error::ApiError};
use plamenu_db::{
    account::{self, Account},
    video_channel_access as access,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Change {
    pub operation: String,
    pub username: Option<String>,
    pub role: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    /// Destructive actions require the immutable channel username.
    pub confirm: Option<String>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}

/// Resolve a local post for mutation/preview. Native Video authority comes
/// from its channel even when the requester originally authored the Video.
/// Mutations must recheck under `lock_publication` before writing.
pub async fn editable_status(
    state: &AppState,
    requester: i64,
    id: i64,
) -> Result<plamenu_db::status::Status, ApiError> {
    let stored = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if stored.uri.is_some() || stored.reblog_of_id.is_some() {
        return Err(ApiError::NotFound);
    }
    if stored.object_type.as_deref() == Some("Video") {
        if access::publication_author(&state.pool, id, requester)
            .await?
            .is_none()
        {
            return Err(ApiError::NotFound);
        }
    } else if stored.account_id != requester {
        return Err(ApiError::NotFound);
    }
    Ok(stored)
}

pub(crate) async fn lock_status(
    conn: &mut plamenu_db::PgConnection,
    requester: i64,
    id: i64,
) -> Result<(), ApiError> {
    let media: Option<i64> =
        sqlx::query_scalar("SELECT media_id FROM video_publications WHERE status_id=$1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let media = media.ok_or(ApiError::NotFound)?;
    if !access::lock_publication(conn, media, requester).await? {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

async fn target(conn: &mut plamenu_db::PgConnection, username: &str) -> Result<Account, ApiError> {
    let account = account::find_local_by_username(&mut *conn, username.trim())
        .await?
        .ok_or_else(|| invalid("Choose an active local person"))?;
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR SHARE")
        .bind(account.id)
        .fetch_one(&mut *conn)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let refreshed = account::find_by_id(conn, account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if refreshed.is_group() || refreshed.suspended() || !refreshed.is_local() {
        return Err(invalid("Choose an active local person"));
    }
    Ok(refreshed)
}

/// No network access inside the authority transaction. Actor updates and their
/// outbox deliveries commit with the change or roll back with it.
#[allow(
    clippy::too_many_lines,
    reason = "one auditable channel authority state machine"
)]
pub async fn change(
    state: &AppState,
    actor: &Account,
    channel_id: i64,
    input: &Change,
) -> Result<(), ApiError> {
    if input.operation == "profile" {
        return update_profile(
            state,
            actor,
            channel_id,
            ProfileEdit {
                display_name: input.display_name.clone(),
                description: input.description.clone(),
                ..Default::default()
            },
        )
        .await;
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT owner_account_id,transfer_to FROM video_channels WHERE account_id=$1 FOR UPDATE",
    )
    .bind(channel_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let (owner_id, transfer_to) = row.ok_or(ApiError::NotFound)?;
    let channel = account::find_by_id(&mut *tx, channel_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Acquire the recipient quota lock before taking shared identity locks;
    // concurrent acceptances into different channels must not both upgrade a
    // held SHARE lock and deadlock one another.
    if input.operation == "accept_transfer" {
        sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(actor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    let role = access::lock_access(&mut tx, channel_id, actor.id).await?;
    // Recipients have no granted authority yet; suspension still forbids consent.
    if account::find_by_id(&mut *tx, actor.id)
        .await?
        .is_none_or(|a| a.suspended())
        || channel.suspended()
        || account::find_by_id(&mut *tx, owner_id)
            .await?
            .is_none_or(|a| a.suspended())
    {
        return Err(ApiError::NotFound);
    }
    let mut offer = None;
    match input.operation.as_str() {
        "accept" | "refuse" => {
            let accepted = input.operation == "accept";
            let affected=if accepted {
                sqlx::query("UPDATE video_channel_grants SET accepted=true WHERE channel_id=$1 AND account_id=$2 AND NOT accepted")
                    .bind(channel_id).bind(actor.id).execute(&mut *tx).await
            } else {
                sqlx::query("DELETE FROM video_channel_grants WHERE channel_id=$1 AND account_id=$2")
                    .bind(channel_id).bind(actor.id).execute(&mut *tx).await
            }.map_err(plamenu_db::DbError::from)?.rows_affected();
            if affected == 0 {
                return Err(ApiError::NotFound);
            }
        }
        "accept_transfer" | "refuse_transfer" => {
            if transfer_to != Some(actor.id) {
                return Err(ApiError::NotFound);
            }
            if input.operation == "accept_transfer" {
                if input.confirm.as_deref() != Some(channel.username.as_str()) {
                    return Err(invalid(
                        "Confirm the channel address before accepting ownership",
                    ));
                }
                // A handoff must never invalidate an encoder or abandon an
                // in-flight private recording/upload. The UI explains recovery.
                let busy: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_live_broadcasts WHERE channel_id=$1 AND (state <> 'ended' OR permanent)) OR EXISTS(SELECT 1 FROM native_live_sessions ls JOIN native_live_broadcasts b ON b.media_id=ls.media_id WHERE b.channel_id=$1 AND (ls.ended_at IS NULL OR ls.replay_media_id IS NULL)) OR EXISTS(SELECT 1 FROM scheduled_statuses WHERE video_channel_id=$1) OR EXISTS(SELECT 1 FROM video_drafts d JOIN media_attachments m ON m.id=d.media_id WHERE d.channel_id=$1 AND m.status_id IS NULL)")
                    .bind(channel_id).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                if busy {
                    return Err(ApiError::Conflict("Finish or cancel unpublished uploads, scheduled posts and live broadcasts before transferring this channel".into()));
                }
                // Serialize the recipient's channel quota with channel creation.
                if plamenu_db::video::owned_channels(&mut *tx, actor.id)
                    .await?
                    .len()
                    >= 50
                {
                    return Err(invalid(
                        "The recipient has reached the limit of 50 video channels",
                    ));
                }
                sqlx::query("UPDATE video_channels SET owner_account_id=$2, transfer_to=NULL WHERE account_id=$1")
                    .bind(channel_id).bind(actor.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                // A transfer is a privilege boundary: all previous invitations
                // and grants are revoked, not inherited silently by a new owner.
                sqlx::query("DELETE FROM video_channel_grants WHERE channel_id=$1")
                    .bind(channel_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
                let mut retired_key = [0_u8; 32];
                getrandom::fill(&mut retired_key).map_err(|e| ApiError::Internal(Box::new(e)))?;
                sqlx::query("UPDATE native_live_broadcasts SET owner_account_id=$2, key_generation=key_generation+1, ingest_key_hash=$3 WHERE channel_id=$1")
                    .bind(channel_id).bind(actor.id).bind(retired_key.as_slice()).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                crate::profile::fan_out_actor_update_conn(state, &mut tx, &channel).await?;
            } else {
                sqlx::query("UPDATE video_channels SET transfer_to=NULL WHERE account_id=$1")
                    .bind(channel_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
            }
        }
        "invite" | "revoke" | "transfer" | "cancel_transfer" | "delete" => {
            if role.as_deref() != Some("owner") {
                return Err(ApiError::NotFound);
            }
            match input.operation.as_str() {
                "cancel_transfer" => {
                    sqlx::query("UPDATE video_channels SET transfer_to=NULL WHERE account_id=$1")
                        .bind(channel_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(plamenu_db::DbError::from)?;
                }
                "delete" => {
                    if input.confirm.as_deref() != Some(channel.username.as_str()) {
                        return Err(invalid("Confirm the channel address before deleting it"));
                    }
                    crate::moderation::self_delete_account_conn(state, &mut tx, &channel).await?;
                    sqlx::query("DELETE FROM video_channel_grants WHERE channel_id=$1")
                        .bind(channel_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(plamenu_db::DbError::from)?;
                    sqlx::query("UPDATE video_channels SET transfer_to=NULL WHERE account_id=$1")
                        .bind(channel_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(plamenu_db::DbError::from)?;
                }
                _ => {
                    let target =
                        target(&mut tx, input.username.as_deref().unwrap_or_default()).await?;
                    if target.id == owner_id {
                        return Err(invalid("The channel owner already has full authority"));
                    }
                    match input.operation.as_str() {
                        "invite" => {
                            let role = input.role.as_deref().unwrap_or("publisher");
                            if !matches!(role, "publisher" | "editor") {
                                return Err(invalid("Choose publisher or editor"));
                            }
                            let count: i64 = sqlx::query_scalar(
                                "SELECT count(*) FROM video_channel_grants WHERE channel_id=$1",
                            )
                            .bind(channel_id)
                            .fetch_one(&mut *tx)
                            .await
                            .map_err(plamenu_db::DbError::from)?;
                            let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM video_channel_grants WHERE channel_id=$1 AND account_id=$2)").bind(channel_id).bind(target.id).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                            if count >= 50 && !exists {
                                return Err(invalid(
                                    "A channel can invite at most 50 collaborators",
                                ));
                            }
                            let previous: Option<String> = sqlx::query_scalar("SELECT role FROM video_channel_grants WHERE channel_id=$1 AND account_id=$2")
                                .bind(channel_id).bind(target.id).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                            if previous.as_deref() != Some(role) {
                                // Replacing a pending offer invalidates its old action.
                                sqlx::query("DELETE FROM notifications n USING notification_actor_contexts c WHERE c.notification_id=n.id AND c.actor_account_id=$1 AND n.account_id=$2 AND c.event='invitation'")
                                    .bind(channel_id).bind(target.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                                offer = Some((
                                    target.id,
                                    plamenu_db::actor_notification::Event::Invitation,
                                ));
                            }
                            // Changing a role requires fresh consent; especially
                            // an upgrade must not silently expand authority.
                            sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role) VALUES($1,$2,$3) ON CONFLICT(channel_id,account_id) DO UPDATE SET role=EXCLUDED.role,accepted=CASE WHEN video_channel_grants.role=EXCLUDED.role THEN video_channel_grants.accepted ELSE false END")
                                .bind(channel_id).bind(target.id).bind(role).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                        }
                        "revoke" => {
                            sqlx::query("DELETE FROM video_channel_grants WHERE channel_id=$1 AND account_id=$2").bind(channel_id).bind(target.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                        }
                        "transfer" => {
                            if transfer_to != Some(target.id) {
                                sqlx::query("DELETE FROM notifications n USING notification_actor_contexts c WHERE c.notification_id=n.id AND c.actor_account_id=$1 AND c.event='transfer'")
                                    .bind(channel_id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                                offer = Some((
                                    target.id,
                                    plamenu_db::actor_notification::Event::Transfer,
                                ));
                            }
                            sqlx::query(
                                "UPDATE video_channels SET transfer_to=$2 WHERE account_id=$1",
                            )
                            .bind(channel_id)
                            .bind(target.id)
                            .execute(&mut *tx)
                            .await
                            .map_err(plamenu_db::DbError::from)?;
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        _ => return Err(invalid("Unknown channel action")),
    }
    // Remove resolved work items in the authority transaction. This clears
    // every recipient's pending badge, including queued push/email jobs.
    sqlx::query("DELETE FROM notifications n USING notification_actor_contexts c WHERE c.notification_id=n.id AND c.actor_account_id=$1 AND c.event IN ('invitation','transfer','follow_request') AND NOT notification_visible(n.account_id,n.id,n.report_id)")
        .bind(channel_id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if let Some((recipient, event)) = offer {
        plamenu_db::notification::create_for_actor(
            &state.pool,
            &mut tx,
            recipient,
            actor.id,
            plamenu_db::actor_notification::Context {
                actor_id: channel_id,
                source_account_id: channel_id,
                event,
            },
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Delegated profile editing uses account profile media, not community roles.
#[derive(Default)]
pub struct ProfileEdit {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar: Option<Vec<u8>>,
    pub header: Option<Vec<u8>>,
    pub avatar_description: Option<String>,
    pub header_description: Option<String>,
}

pub async fn update_profile(
    state: &AppState,
    actor: &Account,
    channel_id: i64,
    input: ProfileEdit,
) -> Result<(), ApiError> {
    if !matches!(
        access::role(&state.pool, channel_id, actor.id)
            .await?
            .as_deref(),
        Some("owner" | "editor")
    ) {
        return Err(ApiError::NotFound);
    }
    if input
        .display_name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty() || name.trim().chars().count() > 120)
        || input
            .description
            .as_ref()
            .is_some_and(|text| text.chars().count() > 1000)
        || [&input.avatar_description, &input.header_description]
            .iter()
            .any(|text| {
                text.as_ref()
                    .is_some_and(|text| text.chars().count() > 1500)
            })
    {
        return Err(invalid(
            "Use a name of 1–120 characters, a description of at most 1000 characters and image descriptions of at most 1500 characters",
        ));
    }
    let images =
        crate::profile::PreparedProfileImages::prepare(state, input.avatar, input.header).await?;
    let result = async {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        // Serialize profile replacements with each other and grant/owner changes.
        sqlx::query("SELECT account_id FROM video_channels WHERE account_id=$1 FOR UPDATE")
            .bind(channel_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        if !matches!(
            access::lock_access(&mut tx, channel_id, actor.id)
                .await?
                .as_deref(),
            Some("owner" | "editor")
        ) {
            return Err(ApiError::NotFound);
        }
        let old = account::find_by_id(&mut *tx, channel_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let note = input
            .description
            .as_deref()
            .map(plamenu_ap::text::plain_to_html);
        let mut update = account::ProfileUpdate {
            display_name: input.display_name.as_deref().map(str::trim),
            note: note.as_deref(),
            note_source: input.description.as_deref(),
            avatar_description: input.avatar_description.as_deref(),
            header_description: input.header_description.as_deref(),
            ..Default::default()
        };
        images.apply(&mut update);
        let updated = account::update_local_profile_conn(&mut tx, channel_id, update)
            .await?
            .ok_or(ApiError::NotFound)?;
        crate::profile::fan_out_actor_update_conn(state, &mut tx, &updated).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Ok::<_, ApiError>((old, updated))
    }
    .await;
    match result {
        Ok((old, updated)) => {
            images.finish(state, &old, &updated).await;
            Ok(())
        }
        Err(error) => {
            images.abandon(state).await;
            Err(error)
        }
    }
}
