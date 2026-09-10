//! Shared account-moderation logic, called from both the admin REST API
//! (`routes::admin_accounts`) and the admin web dashboard (`web::admin`).
//!
//! Keeping the dispatch in one place means a moderation action applied from a
//! browser form takes exactly the same code path — the same state mutations,
//! the same `account_warnings` strike, the same suspension federation —
//! as one applied through the API.

use std::fmt::Write;

use plamenu_db::account::{self, Account};
use plamenu_db::role::{self, Role, permission};
use plamenu_db::{account_warning, follow, id, job, user};

use crate::error::ApiError;
use crate::{AppState, admin_log};

/// The moderation actions `Admin::AccountAction` accepts. `none`/`disable` are
/// only meaningful for local accounts (Mastodon's `types_for_account`).
pub const ACTION_TYPES: [&str; 5] = ["none", "disable", "sensitive", "silence", "suspend"];

/// The class of moderation action, for authorization. These are the
/// state-changing and destructive verbs a moderator applies *to* an account;
/// the lift verbs (`unsuspend`, `unsilence`, `unsensitive`, `enable`,
/// `approve`) reduce a punishment and are deliberately not gated here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// `none` — a recorded strike with no state change.
    Warn,
    Disable,
    Sensitive,
    Silence,
    Suspend,
    /// Delete a pending sign-up (`reject`).
    Reject,
    /// Permanent account deletion (`destroy`).
    Destroy,
}

impl ActionKind {
    /// Maps an [`apply_account_action`] `action_type` string. `reject`/`destroy`
    /// are not routed through that helper, so they are not accepted here.
    fn from_action_type(action_type: &str) -> Option<Self> {
        Some(match action_type {
            "none" => Self::Warn,
            "disable" => Self::Disable,
            "sensitive" => Self::Sensitive,
            "silence" => Self::Silence,
            "suspend" => Self::Suspend,
            _ => return None,
        })
    }
}

/// Decides whether `actor` may apply `kind` to a target, given the actor's role
/// and (for a local target) the target's role. Pure so the whole role lattice
/// can be unit-tested without a database.
///
/// The rules are independent of the specific state mutation, so REST and web
/// share exactly one policy:
///
/// - a moderator may never apply a state-changing/destructive verb to their own
///   account (self-suspend, self-disable, self-destroy, …);
/// - a moderator must *strictly* outrank the target's role — this also blocks
///   acting on a peer at the same position, so a Moderator cannot moderate an
///   Admin/Owner and two Moderators cannot moderate each other. A remote or
///   role-less account counts as position `0`, which every staff role outranks;
/// - permanent deletion additionally requires `DELETE_USER_DATA`, the bit the
///   role editor already exposes for exactly this action (it was previously
///   decorative — no handler checked it).
pub fn authorize_account_action(
    actor_role: &Role,
    actor_account_id: i64,
    target_account_id: i64,
    target_role: Option<&Role>,
    kind: ActionKind,
) -> Result<(), ApiError> {
    if target_account_id == actor_account_id {
        return Err(ApiError::Forbidden(
            "You cannot apply moderation actions to your own account".into(),
        ));
    }
    let target_position = target_role.map_or(0, |role| role.position);
    if actor_role.position <= target_position {
        return Err(ApiError::Forbidden(
            "You cannot moderate an account whose role ranks at or above your own".into(),
        ));
    }
    if kind == ActionKind::Destroy && !actor_role.can(permission::DELETE_USER_DATA) {
        return Err(ApiError::Forbidden(
            "Permanently deleting an account requires the delete user data permission".into(),
        ));
    }
    Ok(())
}

/// The verbs that remove an account's ability to sign in and administer the
/// instance. Applying one to the last administrator would lock everyone out of
/// the admin surfaces, so it is additionally refused by
/// [`guard_last_administrator`]. `Warn`/`Sensitive`/`Silence`
/// restrict what an account may *publish*, not whether it can still
/// administer, so they are deliberately not gated here.
fn removes_admin_access(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::Disable | ActionKind::Suspend | ActionKind::Reject | ActionKind::Destroy
    )
}

/// Whether `target` is the only account that can still administer the instance:
/// a local account whose role grants the `administrator` permission, with no
/// *other* active (approved, enabled, unsuspended) administrator behind it.
///
/// Only the built-in Owner role carries that permission by default, so in a
/// normal deployment this is the "last owner". It stays reachable — and worth
/// guarding — when a custom role is positioned above the owner without granting
/// `administrator` itself, since such a role outranks the owner (passing
/// [`authorize_account_action`]) yet cannot administer in its place. Drives both
/// the server-side refusal and hiding the destructive web forms so no button
/// 403s.
pub async fn is_last_active_administrator(
    state: &AppState,
    target: &Account,
    target_role: Option<&Role>,
) -> Result<bool, ApiError> {
    let is_admin =
        target_role.is_some_and(|role| role.permissions & permission::ADMINISTRATOR != 0);
    if !is_admin || !target.is_local() {
        return Ok(false);
    }
    Ok(role::active_administrator_count(&state.pool, Some(target.id)).await? == 0)
}

/// Refuses an access-removing verb (`disable`/`suspend`/`reject`/`destroy`)
/// against the last administrator. A no-op for every other verb
/// and for any target that is not the sole administrator, so callers can invoke
/// it unconditionally after [`authorize_account_action`].
pub async fn guard_last_administrator(
    state: &AppState,
    target: &Account,
    target_role: Option<&Role>,
    kind: ActionKind,
) -> Result<(), ApiError> {
    if removes_admin_access(kind)
        && is_last_active_administrator(state, target, target_role).await?
    {
        return Err(ApiError::Forbidden(
            "You cannot disable, suspend, or permanently delete the last administrator. \
             Grant the administrator role to another active account first."
                .into(),
        ));
    }
    Ok(())
}

/// The target's assigned role, or `None` for a remote or role-less account
/// (which every staff role outranks). Reuses the existing `role::for_account`
/// query — no new SQL.
pub async fn target_role(state: &AppState, target: &Account) -> Result<Option<Role>, ApiError> {
    if !target.is_local() {
        return Ok(None);
    }
    Ok(role::for_account(&state.pool, target.id).await?)
}

/// Applies a moderation `action_type` to `target`, records the explanatory
/// strike in `account_warnings`, and — for a `suspend` of a local account —
/// federates a blanked `Update(Actor)` carrying `suspended: true` so remotes
/// mirror the (reversible) suspension.
///
/// The caller is responsible for validating `action_type` against
/// [`ACTION_TYPES`] and that any cited `report_id` exists; this function trusts
/// both. A `disable` or `none` of a remote account is rejected with a 403,
/// mirroring Mastodon's `types_for_account`.
#[allow(
    clippy::too_many_lines,
    reason = "one moderation transaction keeps state, evidence, reports, follow severance and audit atomic"
)]
pub async fn apply_account_action(
    state: &AppState,
    moderator_role: &Role,
    moderator_account_id: i64,
    target: &Account,
    action_type: &str,
    text: &str,
    report_id: Option<i64>,
) -> Result<(), ApiError> {
    // Authorize before any state change: the moderator must outrank the target
    // and may never act on their own account. `disable`/`sensitive`/`silence`/
    // `suspend`/`none` all pass through here, so the REST and web callers share
    // one gate and cannot diverge (`reject`/`destroy` guard at their sites).
    let kind = ActionKind::from_action_type(action_type).ok_or_else(|| {
        ApiError::Unprocessable("Validation failed: Type is not included in the list".into())
    })?;
    let target_role = target_role(state, target).await?;
    authorize_account_action(
        moderator_role,
        moderator_account_id,
        target.id,
        target_role.as_ref(),
        kind,
    )?;
    // Even a moderator who outranks the target may not disable/suspend the last
    // administrator and lock everyone out of the admin surfaces.
    guard_last_administrator(state, target, target_role.as_ref(), kind).await?;

    // The cited report supplies the warning's evidence. Refuse a cross-account
    // citation even if a client bypasses the dashboard's hidden field.
    let cited_report = match report_id {
        Some(report_id) => Some(
            plamenu_db::report::find_by_id(&state.pool, report_id)
                .await?
                .filter(|report| report.target_account_id == target.id)
                .ok_or(ApiError::NotFound)?,
        ),
        None => None,
    };
    let status_ids = cited_report
        .as_ref()
        .map_or(&[][..], |report| report.status_ids.as_slice());
    let suspension_email_hash = if action_type == "suspend" && target.is_local() {
        user::find_by_account_id(&state.pool, target.id)
            .await?
            .and_then(|user| user.email)
            .map(|email| crate::instance_policy::canonical_email_hash(&email))
            .transpose()?
    } else {
        None
    };

    // Core moderation is one database decision: the state flag, strike,
    // report resolution, irreversible remote-follow rejections and audit line
    // cannot drift apart after a crash or a late constraint failure.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(target.id)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    match action_type {
        "disable" => {
            require_local(target)?;
            sqlx::query!(
                "UPDATE users SET disabled = true WHERE account_id = $1",
                target.id
            )
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        }
        "sensitive" => {
            sqlx::query!(
                "UPDATE accounts SET sensitized_at = COALESCE(sensitized_at, now()) WHERE id = $1",
                target.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        }
        "silence" => {
            sqlx::query!(
                "UPDATE accounts SET silenced_at = COALESCE(silenced_at, now()) WHERE id = $1",
                target.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        }
        "suspend" => {
            sqlx::query!(
                "UPDATE accounts
                 SET suspended_at = COALESCE(suspended_at, now()), suspension_origin = 'local'
                 WHERE id = $1",
                target.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
            sqlx::query!(
                "INSERT INTO account_deletion_requests (account_id)
                 VALUES ($1) ON CONFLICT (account_id) DO NOTHING",
                target.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
            if let Some(email_hash) = suspension_email_hash.as_deref() {
                sqlx::query!(
                    "INSERT INTO canonical_email_blocks
                        (id, canonical_email_hash, reference_account_id)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (canonical_email_hash) DO NOTHING",
                    id::next(),
                    email_hash,
                    target.id,
                )
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
            }

            // A locally-suspended remote actor is still fully active at its
            // origin. Reject and remove every follow it has toward our local
            // actors, otherwise it keeps receiving follower-only posts. This
            // is intentionally irreversible, matching Mastodon.
            if !target.is_local() {
                let edges = follow::local_edges_from(&mut *tx, target.id).await?;
                let mut deliveries = Vec::new();
                if let Some(follower_uri) = target.uri.as_deref() {
                    for edge in &edges {
                        if let Some(follow_uri) = edge.edge_uri.as_deref() {
                            let received = plamenu_ap::activity::follow_as_received(
                                follow_uri,
                                follower_uri,
                                &state.config.domain,
                                &edge.target_username,
                            );
                            deliveries.push(job::QueuedDelivery {
                                signer_account_id: edge.target_account_id,
                                inbox_url: target.inbox_url.clone(),
                                activity: plamenu_ap::activity::reject_follow(
                                    &state.config.domain,
                                    &edge.target_username,
                                    id::next(),
                                    received,
                                ),
                            });
                        }
                    }
                }
                job::enqueue_batch_tx(&mut *tx, &deliveries).await?;
                let local_targets: Vec<i64> =
                    edges.iter().map(|edge| edge.target_account_id).collect();
                follow::delete_out_many(&mut *tx, target.id, &local_targets).await?;
                sqlx::query!(
                    "DELETE FROM notifications
                     WHERE account_id = ANY($1) AND from_account_id = $2
                       AND kind IN ('follow', 'follow_request')",
                    &local_targets,
                    target.id,
                )
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
            }
            sqlx::query!("DELETE FROM status_trends WHERE account_id = $1", target.id)
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
        }
        // "none": a strike with no state change (a recorded warning).
        "none" => {}
        _ => {
            return Err(ApiError::Unprocessable(
                "Validation failed: Type is not included in the list".into(),
            ));
        }
    }

    let warning = account_warning::create_tx(
        &mut tx,
        account_warning::NewAccountWarning {
            account_id: Some(moderator_account_id),
            target_account_id: target.id,
            action: action_type,
            text,
            report_id,
            status_ids,
        },
    )
    .await?;

    // A state-changing action resolves every still-open report about the
    // target; a bare warning resolves only the report it was issued from.
    let resolved_ids = if action_type == "none" {
        match report_id {
            Some(report_id) => sqlx::query_scalar!(
                "UPDATE reports
                     SET action_taken_at = now(), action_taken_by_account_id = $2,
                         updated_at = now()
                     WHERE id = $1 AND action_taken_at IS NULL
                     RETURNING id",
                report_id,
                moderator_account_id,
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?,
            None => Vec::new(),
        }
    } else {
        sqlx::query_scalar!(
            "UPDATE reports
             SET action_taken_at = now(), action_taken_by_account_id = $2,
                 updated_at = now()
             WHERE target_account_id = $1 AND action_taken_at IS NULL
             RETURNING id",
            target.id,
            moderator_account_id,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?
    };

    // Audit-log vocabulary mirrors Mastodon's `Admin::AccountAction`: the
    // state-changing verbs target the account (`disable` its user), while a
    // bare warning logs as `create` on the strike itself.
    let log_target = match action_type {
        "disable" => admin_log::Target::user(target),
        "none" => admin_log::Target::warning(warning.id, target),
        _ => admin_log::Target::account(target),
    };
    let log_verb = if action_type == "none" {
        "create"
    } else {
        action_type
    };
    plamenu_db::admin_action_log::record_tx(
        &mut tx,
        admin_log::new_line(moderator_account_id, log_verb, &log_target),
    )
    .await?;
    if action_type == "suspend" && target.is_local() {
        // A reversible local suspension is an Update(Actor), never a Delete.
        let suspended = account::find_by_id(&mut *tx, target.id)
            .await?
            .ok_or(ApiError::NotFound)?;
        crate::profile::fan_out_actor_update_conn(state, &mut tx, &suspended).await?;
    }

    tx.commit().await.map_err(plamenu_db::DbError::from)?;

    if action_type == "suspend" {
        state.streaming.disconnect_viewer(target.id);
    }

    // Tell the warned user (Mastodon's `process_notification!`): an in-app
    // `moderation_warning` notification plus a best-effort e-mail. Local
    // accounts only — a remote target reads nothing here.
    if target.is_local() {
        plamenu_db::notification::create_moderation_warning(&state.pool, target.id, warning.id)
            .await?;
        notify_warning(state, target.id, action_type, text).await?;
    }

    for report_id in resolved_ids {
        if let Some(resolved) = plamenu_db::report::find_by_id(&state.pool, report_id).await? {
            crate::webhooks::report_event(state, plamenu_db::webhook::REPORT_UPDATED, &resolved)
                .await;
        }
    }
    Ok(())
}

/// Lifts a local suspension and runs Mastodon's restoration side effects. A
/// remote actor is immediately re-fetched: it may have become suspended or
/// deleted at its own origin while our local suspension hid its updates. A
/// local actor advertises its restored profile with `Update(Actor)`.
pub async fn unsuspend_account(state: &AppState, target: &Account) -> Result<Account, ApiError> {
    if target.suspension_origin.as_deref() != Some("local") {
        return Err(ApiError::Forbidden(
            "Only a locally applied suspension can be lifted by an administrator".into(),
        ));
    }
    if target.is_local() {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        if !account::unsuspend_conn(&mut tx, target.id).await? {
            return Err(ApiError::Gone);
        }
        let restored = account::find_by_id(&mut *tx, target.id)
            .await?
            .ok_or(ApiError::NotFound)?;
        crate::profile::fan_out_actor_update_conn(state, &mut tx, &restored).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        return Ok(restored);
    }
    if !account::unsuspend(&state.pool, target.id).await? {
        return Err(ApiError::Gone);
    }

    // A portable identity is remotely owned but this gateway is already its
    // hosting authority. Unsuspension restores gateway access directly: an
    // origin fetch would loop back into our own gateway, and only the client
    // may publish an Update for its actor document.
    if target.is_portable_on(&state.config.domain) {
        return account::find_by_id(&state.pool, target.id)
            .await?
            .ok_or(ApiError::NotFound);
    }

    if let Some(uri) = target.uri.as_deref() {
        match state.federation.fetch_actor(uri).await {
            Ok(actor) => {
                return Ok(crate::remote::refresh_remote_actor(state, &actor).await?);
            }
            Err(error) => {
                // The restoration itself remains useful when the origin is
                // temporarily unavailable; a later normal refresh reconciles
                // remote suspension/deletion state.
                tracing::warn!(%error, actor = %uri, "could not refresh remote actor after unsuspension");
            }
        }
    }
    account::find_by_id(&state.pool, target.id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Permanently deletes a temporarily, locally suspended account while keeping
/// its reserved tombstone. Local actors first enqueue `Delete(Actor)`; the
/// data purge and append-only admin audit line then commit atomically.
pub async fn destroy_suspended_account(
    state: &AppState,
    moderator_account_id: i64,
    target: &Account,
) -> Result<(), ApiError> {
    if !target.suspended() || target.suspension_origin.as_deref() != Some("local") {
        return Err(ApiError::Forbidden(
            "An account must be temporarily suspended before it can be permanently deleted".into(),
        ));
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if target.is_local() {
        federate_delete_conn(state, &mut tx, target).await?;
    }
    let log_target = admin_log::Target::account(target);
    if !account::purge_suspended_with_audit_conn(
        &mut tx,
        target.id,
        admin_log::new_line(moderator_account_id, "destroy", &log_target),
    )
    .await?
    {
        return Err(ApiError::Conflict(
            "The account is no longer temporarily suspended".into(),
        ));
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Approves a strike appeal (Mastodon's `ApproveAppealService`): reverses
/// whatever the strike did, stamps the appeal approved and the warning
/// overruled, and notifies the appellant by mail when possible. The caller
/// audit-logs the decision.
pub async fn approve_appeal(
    state: &AppState,
    appeal: &plamenu_db::appeal::Appeal,
    moderator_account_id: i64,
) -> Result<bool, ApiError> {
    let Some(approved) =
        plamenu_db::appeal::approve(&state.pool, appeal.id, moderator_account_id).await?
    else {
        return Ok(false);
    };
    let Some(warning) =
        account_warning::find_by_id(&state.pool, approved.account_warning_id).await?
    else {
        return Ok(true);
    };
    let target_id = warning.target_account_id;
    match warning.action.as_str() {
        "disable" => {
            user::set_disabled(&state.pool, target_id, false).await?;
        }
        "sensitive" => {
            account::unsensitize(&state.pool, target_id).await?;
        }
        "silence" => {
            account::unsilence(&state.pool, target_id).await?;
        }
        "suspend" => {
            if let Some(target) = account::find_by_id(&state.pool, target_id).await? {
                unsuspend_account(state, &target).await?;
            }
        }
        // A plain warning has nothing to reverse.
        _ => {}
    }
    account_warning::overrule(&state.pool, warning.id).await?;
    notify_appeal_decision(state, target_id, &warning.action, true).await?;
    Ok(true)
}

/// Rejects a strike appeal and notifies the appellant.
pub async fn reject_appeal(
    state: &AppState,
    appeal: &plamenu_db::appeal::Appeal,
    moderator_account_id: i64,
) -> Result<bool, ApiError> {
    let Some(rejected) =
        plamenu_db::appeal::reject(&state.pool, appeal.id, moderator_account_id).await?
    else {
        return Ok(false);
    };
    if let Some(warning) =
        account_warning::find_by_id(&state.pool, rejected.account_warning_id).await?
    {
        notify_appeal_decision(state, warning.target_account_id, &warning.action, false).await?;
    }
    Ok(true)
}

/// Mails the warned user about a fresh strike (Mastodon's `UserMailer#warning`;
/// best-effort like the appeal mails — the in-app notification is the
/// guaranteed channel).
async fn notify_warning(
    state: &AppState,
    target_account_id: i64,
    action: &str,
    text: &str,
) -> Result<(), ApiError> {
    if !crate::mailer::enabled(state) {
        return Ok(());
    }
    let Some(user) = user::find_by_account_id(&state.pool, target_account_id).await? else {
        return Ok(());
    };
    let Some(recipient) = user.email.as_deref() else {
        return Ok(());
    };
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    // Written in the account owner's stored locale — moderation mail is read
    // far from the staff request that triggered it.
    let locale = crate::web::i18n::Locale::for_user(&state.pool, user.id).await?;
    let heading = locale.plain(match action {
        "disable" => "email-strike-heading-disable",
        "sensitive" => "email-strike-heading-sensitive",
        "silence" => "email-strike-heading-silence",
        "suspend" => "email-strike-heading-suspend",
        _ => "email-strike-heading-warning",
    });
    let mut body = format!(
        "{greeting}\n\n{heading}\n",
        greeting = locale.plain("email-reset-greeting"),
    );
    if !text.trim().is_empty() {
        let _ = write!(
            body,
            "\n{wrote}\n\n{}\n",
            text.trim(),
            wrote = locale.plain("email-strike-wrote"),
        );
    }
    let _ = write!(
        body,
        "\n{outro}\n",
        outro = locale.plain("email-strike-outro")
    );
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("site", settings.site_title.as_str());
    let subject = locale.plain_with("email-strike-subject", &args);
    crate::mailer::enqueue(state, recipient, &subject, &body).await
}

/// Mails the appellant about the decision when they have an address and mail
/// is configured (Mastodon's `appeal_approved`/`appeal_rejected` mails; the
/// e-mail-less divergence means this is best-effort).
async fn notify_appeal_decision(
    state: &AppState,
    target_account_id: i64,
    action: &str,
    approved: bool,
) -> Result<(), ApiError> {
    if !crate::mailer::enabled(state) {
        return Ok(());
    }
    let Some(user) = user::find_by_account_id(&state.pool, target_account_id).await? else {
        return Ok(());
    };
    let Some(recipient) = user.email.as_deref() else {
        return Ok(());
    };
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    let locale = crate::web::i18n::Locale::for_user(&state.pool, user.id).await?;
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("site", settings.site_title.as_str());
    args.set("action", action);
    let (subject_id, body_id) = if approved {
        (
            "email-appeal-approved-subject",
            "email-appeal-approved-body",
        )
    } else {
        (
            "email-appeal-rejected-subject",
            "email-appeal-rejected-body",
        )
    };
    let subject = locale.plain_with(subject_id, &args);
    let body = format!(
        "{greeting}\n\n{verdict}\n",
        greeting = locale.plain("email-reset-greeting"),
        verdict = locale.plain_with(body_id, &args),
    );
    crate::mailer::enqueue(state, recipient, &subject, &body).await
}

/// Self-service account deletion — Mastodon's `DeleteAccountService`
/// with `reserve_username: true, reserve_email: false`: suspends the account
/// into a tombstone (the actor and its collections serve `410 Gone`, the
/// username stays taken), federates `Delete(Actor)`, purges the authored
/// content/relationships, and destroys the login — freeing the e-mail
/// address and killing every session and API token.
pub async fn self_delete_account(state: &AppState, target: &Account) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    self_delete_account_conn(state, &mut tx, target).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn self_delete_account_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    target: &Account,
) -> Result<(), ApiError> {
    require_local(target)?;
    account::suspend_conn(&mut *conn, target.id, "local").await?;
    // Fan out before the purge: the delivery audience is the followers list.
    federate_delete_conn(state, conn, target).await?;
    account::purge_local_data_conn(&mut *conn, target.id).await?;
    user::delete_by_account_id(&mut *conn, target.id).await?;
    // The permanent marker flips the tombstone from "temporarily suspended"
    // (blanked actor) to `410 Gone` — see [`permanently_unavailable`].
    account::mark_deleted(&mut *conn, target.id).await?;
    Ok(())
}

/// Whether a suspended local account is *permanently* unavailable — deleted
/// (`accounts.deleted_at`), not just moderator-suspended. Mastodon keys the
/// same distinction off the deletion request. Permanent unavailability
/// serves `410 Gone`; temporary suspension serves the blanked actor document
/// (and `403` collections) so peers can read `suspended: true` instead of
/// dropping the account.
pub async fn permanently_unavailable(
    pool: &plamenu_db::PgPool,
    account: &Account,
) -> Result<bool, ApiError> {
    if !account.suspended() {
        return Ok(false);
    }
    Ok(account::is_deleted(pool, account.id).await?)
}

/// Rejects an action that only applies to local accounts.
fn require_local(target: &Account) -> Result<(), ApiError> {
    if target.is_local() {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "This action is not allowed on this account".into(),
        ))
    }
}

/// Federates a deleted local account's removal as `Delete(Actor)` to its
/// followers (Mastodon's `DeleteAccountService`). Deletion only — a
/// reversible suspension federates as a blanked `Update(Actor)` instead.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn federate_delete_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
) -> Result<(), ApiError> {
    let activity = plamenu_ap::activity::delete_actor(&state.config.domain, &account.username);
    let delivered = crate::actions::fan_out_conn(state, conn, account, &activity, &[]).await?;
    tracing::info!(user = %account.username, inboxes = delivered, "account deleted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ActionKind, authorize_account_action};
    use plamenu_db::role::{Role, everyone, permission};

    /// A role at `position` granting exactly `permissions`.
    fn role(position: i32, permissions: i64) -> Role {
        Role {
            position,
            permissions,
            ..everyone()
        }
    }

    fn moderator() -> Role {
        role(10, permission::MANAGE_USERS)
    }

    /// Admin carries `DELETE_USER_DATA` on top of `MANAGE_USERS` in the baseline.
    fn admin() -> Role {
        role(50, permission::MANAGE_USERS | permission::DELETE_USER_DATA)
    }

    fn is_forbidden(result: &Result<(), crate::error::ApiError>) -> bool {
        matches!(result, Err(crate::error::ApiError::Forbidden(_)))
    }

    #[test]
    fn moderator_can_suspend_a_role_less_user() {
        // Actor 1 (Moderator) suspends target 2 (no role → position 0).
        assert!(
            authorize_account_action(&moderator(), 1, 2, None, ActionKind::Suspend).is_ok(),
            "a staff role must outrank a role-less local user"
        );
    }

    #[test]
    fn remote_target_is_outranked_by_any_staff() {
        // Remote/role-less accounts are passed as `None` too.
        assert!(authorize_account_action(&moderator(), 1, 2, None, ActionKind::Silence).is_ok());
    }

    #[test]
    fn cannot_moderate_own_account() {
        // Same account id on both sides → denied regardless of role/kind.
        for kind in [
            ActionKind::Warn,
            ActionKind::Disable,
            ActionKind::Suspend,
            ActionKind::Reject,
            ActionKind::Destroy,
        ] {
            assert!(
                is_forbidden(&authorize_account_action(
                    &admin(),
                    7,
                    7,
                    Some(&admin()),
                    kind,
                )),
                "self-moderation must be forbidden for {kind:?}"
            );
        }
    }

    #[test]
    fn cannot_moderate_a_higher_ranked_target() {
        // Moderator (10) may not act on an Admin (50).
        assert!(is_forbidden(&authorize_account_action(
            &moderator(),
            1,
            2,
            Some(&admin()),
            ActionKind::Suspend,
        )));
    }

    #[test]
    fn cannot_moderate_a_peer_at_the_same_position() {
        // Two distinct Moderators at position 10 cannot moderate each other.
        assert!(is_forbidden(&authorize_account_action(
            &moderator(),
            1,
            2,
            Some(&moderator()),
            ActionKind::Silence,
        )));
    }

    #[test]
    fn higher_rank_may_moderate_lower_rank() {
        // Owner-ish (100) over Admin (50).
        let owner = role(100, permission::ADMINISTRATOR);
        assert!(
            authorize_account_action(&owner, 1, 2, Some(&admin()), ActionKind::Suspend).is_ok()
        );
    }

    #[test]
    fn destroy_requires_delete_user_data() {
        // Moderator outranks the role-less target but lacks DELETE_USER_DATA.
        assert!(
            is_forbidden(&authorize_account_action(
                &moderator(),
                1,
                2,
                None,
                ActionKind::Destroy,
            )),
            "permanent deletion must require DELETE_USER_DATA"
        );
        // The same actor may still suspend/reject — only Destroy is gated on it.
        assert!(authorize_account_action(&moderator(), 1, 2, None, ActionKind::Suspend).is_ok());
        assert!(authorize_account_action(&moderator(), 1, 2, None, ActionKind::Reject).is_ok());
    }

    #[test]
    fn destroy_allowed_with_delete_user_data() {
        assert!(authorize_account_action(&admin(), 1, 2, None, ActionKind::Destroy).is_ok());
    }
}
