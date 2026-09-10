//! Account migration — Mastodon's `Move`.
//!
//! Two directions share one core ([`process_move`], Mastodon's `MoveWorker`):
//! re-point every *local* follower of the moving account at its new home, and
//! carry the local blocks and mutes over.
//!
//! * **Inbound** ([`crate::routes::inbox`]): a remote account announces it has moved, and our local
//!   followers should follow the new account.
//! * **Outbound** ([`migrate_local_account`]): a local account moves away and tells its followers,
//!   so *their* servers re-follow the target.
//!
//! A move is only honoured when the target lists the origin in its
//! `alsoKnownAs` — the check that stops one account dragging another's
//! followers somewhere they never agreed to go.
//!
//! Both directions [`enqueue_move`] rather than replaying inline, and
//! [`run_due`] does the work on the background worker — see `enqueue_move`
//! for why.

use std::fmt;
use std::time::Duration;

use plamenu_ap::acct::Acct;
use plamenu_ap::{activity, urls::LocalUserUrls};
use plamenu_db::account::{self, Account};
use plamenu_db::{
    account_alias, account_migration, account_move_job, conversation, follow, id, job, move_replay,
    mute, notification,
};
use tokio::task::JoinHandle;

use crate::error::ApiError;
use crate::remote::refresh_remote_actor;
use crate::{AppState, actions};

/// Minimum time between outbound moves, matching Mastodon's
/// `AccountMigration::COOLDOWN_PERIOD`.
pub const MIGRATION_COOLDOWN: time::Duration = time::Duration::days(30);

/// A refusal the person driving the move can act on, kept typed so the web
/// forms can phrase it in the reader's language while the CLI keeps the
/// wording it always printed (the `oauth_app::Invalid` pattern; see
/// `docs/LOCALIZATION.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// A move happened inside [`MIGRATION_COOLDOWN`].
    Cooldown,
    /// The target resolved to the account being moved.
    SameAccount,
    /// The target does not claim this account in `alsoKnownAs`.
    NotAnAlias,
    /// The handle is not a valid `user@domain` acct.
    BadHandle(String),
    /// The target is on this server.
    LocalTarget,
    /// The target's domain is blocked or outside the allow-list.
    BlockedDomain,
    /// The target's `user@domain` handle could not be resolved (webfinger).
    Unresolvable { acct: String, detail: String },
    /// The target's actor document could not be fetched.
    Unfetchable { uri: String, detail: String },
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cooldown => f.write_str("You can only move your account once every 30 days"),
            Self::SameAccount => {
                f.write_str("The target account cannot be the same as the account being moved")
            }
            Self::NotAnAlias => f.write_str(
                "The target account does not list this account as an alias (alsoKnownAs)",
            ),
            Self::BadHandle(detail) => f.write_str(detail),
            Self::LocalTarget => f.write_str("the target must be a remote account"),
            Self::BlockedDomain => f.write_str("the target domain is blocked or not allowed"),
            Self::Unresolvable { acct, detail } => write!(f, "cannot resolve {acct}: {detail}"),
            Self::Unfetchable { uri, detail } => write!(f, "cannot fetch {uri}: {detail}"),
        }
    }
}

/// How a migration or alias change can fail: a typed refusal the caller may
/// want to phrase itself, or an ordinary error raised while carrying it out.
#[derive(Debug)]
pub enum Failure {
    Invalid(Invalid),
    Api(ApiError),
}

impl From<Invalid> for Failure {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<ApiError> for Failure {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<plamenu_db::DbError> for Failure {
    fn from(err: plamenu_db::DbError) -> Self {
        Self::Api(err.into())
    }
}

impl From<Failure> for ApiError {
    /// Reproduces the status and wording each refusal carried before it was
    /// typed, so the CLI and any future API caller see no change.
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Invalid(
                invalid @ (Invalid::Cooldown | Invalid::SameAccount | Invalid::NotAnAlias),
            ) => Self::Unprocessable(invalid.to_string()),
            Failure::Invalid(invalid) => Self::BadRequest(invalid.to_string()),
            Failure::Api(err) => err,
        }
    }
}

/// Records `source`'s redirect and queues the relationship replay.
///
/// The redirect is written here rather than by the worker because it is one
/// UPDATE and everything downstream depends on it being visible immediately:
/// clients see the new location, and the outbound `Update(Actor)` that follows
/// a local migration has to carry `movedTo`.
///
/// The replay is not. [`process_move`] loops every local follower, blocker and
/// muter and federates a `Follow` plus an `Undo(Follow)` per follower, so its
/// cost is bounded by how popular the moving account is — which is not a bound
/// at all. Inline, that ran inside the inbox POST delivering the `Move`, where
/// a well-followed remote actor would hold the request open past the sender's
/// timeout and earn a redelivery of the same activity (the 2026-07 bench
/// audit's finding; `BENCH_AUDIT_PLAN.md` in git history).
///
/// Enqueuing is idempotent on `(source, target)`: a `Move` is commonly
/// delivered several times over, and each redelivery must find the queued job
/// rather than add another.
pub async fn enqueue_move(
    state: &AppState,
    source: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    enqueue_move_conn(state, &mut tx, source, target).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn enqueue_move_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    source: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    account::set_moved_to(&mut *conn, source.id, target.uri.as_deref()).await?;
    account_move_job::enqueue(&mut *conn, source.id, target.id).await?;
    Ok(())
}

struct FollowerReplay {
    eligible_ids: Vec<i64>,
    refollow_ids: Vec<i64>,
    refollow_uris: Vec<Option<String>>,
    refollow_pending: bool,
    follower_ids: Vec<i64>,
}

fn prepare_follower_replay(
    domain: &str,
    source: &Account,
    target: &Account,
    followers: &[move_replay::FollowerCarry],
    deliveries: &mut Vec<job::QueuedDelivery>,
) -> FollowerReplay {
    let eligible: Vec<_> = followers.iter().filter(|carry| carry.eligible).collect();
    let eligible_ids: Vec<_> = eligible.iter().map(|carry| carry.follower_id).collect();

    // A remote target gets one `Follow` per follower, signed by that follower.
    // A missing target URI skips the re-follow but never the unfollow below.
    let (refollow_ids, refollow_uris, refollow_pending) = if target.is_local() {
        (
            eligible_ids.clone(),
            vec![None; eligible_ids.len()],
            target.locked,
        )
    } else if let Some(target_uri) = &target.uri {
        let mut uris = Vec::with_capacity(eligible.len());
        for carry in &eligible {
            let marker = id::next();
            let actor_uri = carry.actor_uri.clone().unwrap_or_else(|| {
                plamenu_ap::urls::LocalUserUrls::new(domain, &carry.username).id
            });
            uris.push(Some(activity::follow_uri_for_actor(&actor_uri, marker)));
            deliveries.push(job::QueuedDelivery {
                signer_account_id: carry.follower_id,
                inbox_url: target.inbox_url.clone(),
                activity: activity::follow(domain, &carry.username, marker, target_uri),
            });
        }
        (eligible_ids.clone(), uris, true)
    } else {
        tracing::debug!(
            target = target.id,
            "re-follow on move skipped: no target uri"
        );
        (Vec::new(), Vec::new(), true)
    };

    // Every follower unfollows the source. A remote source is told with an
    // `Undo(Follow)` referencing the stored edge URI.
    if !source.is_local()
        && let Some(source_uri) = &source.uri
    {
        for carry in followers {
            if let Some(edge_uri) = &carry.edge_uri {
                let follow_activity =
                    activity::follow_as_sent(edge_uri, domain, &carry.username, source_uri);
                deliveries.push(job::QueuedDelivery {
                    signer_account_id: carry.follower_id,
                    inbox_url: source.inbox_url.clone(),
                    activity: activity::undo(domain, &carry.username, follow_activity),
                });
            }
        }
    }

    FollowerReplay {
        eligible_ids,
        refollow_ids,
        refollow_uris,
        refollow_pending,
        follower_ids: followers.iter().map(|carry| carry.follower_id).collect(),
    }
}

fn queue_block_deliveries(
    domain: &str,
    target: &Account,
    blockers: &[&move_replay::BlockerCarry],
    block_row_ids: &std::collections::HashMap<i64, i64>,
    deliveries: &mut Vec<job::QueuedDelivery>,
) {
    for carry in blockers {
        // `block_account`'s per-edge order: the Reject severing the target's
        // follow of the blocker, then the Block itself (remote targets only).
        if let (false, Some(edge_uri), Some(target_uri)) =
            (target.is_local(), &carry.target_edge_uri, &target.uri)
        {
            let their_follow =
                activity::follow_as_received(edge_uri, target_uri, domain, &carry.username);
            deliveries.push(job::QueuedDelivery {
                signer_account_id: carry.blocker_id,
                inbox_url: target.inbox_url.clone(),
                activity: activity::reject_follow(
                    domain,
                    &carry.username,
                    id::next(),
                    their_follow,
                ),
            });
        }
        if let (false, Some(target_uri), Some(block_row_id)) = (
            target.is_local(),
            &target.uri,
            block_row_ids.get(&carry.blocker_id),
        ) {
            deliveries.push(job::QueuedDelivery {
                signer_account_id: carry.blocker_id,
                inbox_url: target.inbox_url.clone(),
                activity: activity::block(domain, &carry.username, *block_row_id, target_uri),
            });
        }
    }
}

async fn notify_local_move_followers(state: &AppState, target: &Account, follower_ids: &[i64]) {
    if !target.is_local() || target.locked && target.is_group() {
        return;
    }
    let kind = if target.locked {
        "follow_request"
    } else {
        "follow"
    };
    for &follower_id in follower_ids {
        if let Err(err) =
            notification::create(&state.pool, target.id, follower_id, kind, None).await
        {
            tracing::warn!(
                error = %err,
                follower = follower_id,
                "move re-follow notification failed"
            );
        }
    }
}

/// Re-points local relationships from `source` onto `target` and records
/// `source`'s redirect. Mirrors Mastodon's `MoveWorker`: each accepted local
/// follower of the source follows the target and unfollows the source, and
/// local blocks / mutes of the source are replayed onto the target.
///
/// Called by [`run_due`] on the background worker; callers on a request path
/// want [`enqueue_move`].
///
/// Set-based since the N+1 close-out: the
/// per-edge machine (`actions::follow_account` / `block_account`) is not
/// rewritten — [`move_replay::follower_carries`] / [`blocker_carries`] fold
/// its exact skip rules into one eligibility read per direction, the writes
/// land as set statements, and the per-edge activities (each follower's
/// `Follow`/`Undo` is a distinct payload signed by that follower) ride one
/// batched job insert. Everything commits in one transaction, so a crash
/// leaves the job to be reclaimed whole rather than half-replayed. A *local*
/// target's follow notifications still file per eligible follower after the
/// commit — one recipient times many senders is the axis
/// `notification_policy::evaluate_many` does not batch, and a local-target
/// `Move` is the rare case — matching the old post-commit best-effort shape.
///
/// The re-follow divergence stands: a locked target yields a pending
/// request, unlike Mastodon's local→local fast path, which rewrites the edge
/// outright.
pub async fn process_move(
    state: &AppState,
    source: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    // Record the redirect up front: clients see the new location immediately,
    // and the inbound path treats a repeat Move to the same target as a no-op.
    account::set_moved_to(&state.pool, source.id, target.uri.as_deref()).await?;

    let followers =
        move_replay::follower_carries(&state.pool, source.id, target.id, target.domain.as_deref())
            .await?;
    let blockers = move_replay::blocker_carries(&state.pool, source.id, target.id).await?;

    let domain = &state.config.domain;
    let mut deliveries: Vec<job::QueuedDelivery> = Vec::new();
    let follower_replay =
        prepare_follower_replay(domain, source, target, &followers, &mut deliveries);

    let eligible_blockers: Vec<&move_replay::BlockerCarry> =
        blockers.iter().filter(|c| c.eligible).collect();
    let blocker_ids: Vec<i64> = eligible_blockers.iter().map(|c| c.blocker_id).collect();
    let severed_target_edges: Vec<i64> = eligible_blockers
        .iter()
        .filter(|c| c.target_follows_blocker)
        .map(|c| c.blocker_id)
        .collect();

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    move_replay::insert_follow_edges(
        &mut *tx,
        target.id,
        &follower_replay.refollow_ids,
        &follower_replay.refollow_uris,
        follower_replay.refollow_pending,
    )
    .await?;
    // Carried blocks first: the `Block` activity's URI embeds its row id.
    let block_row_ids = move_replay::insert_blocks(&mut *tx, &blocker_ids, target.id).await?;
    queue_block_deliveries(
        domain,
        target,
        &eligible_blockers,
        &block_row_ids,
        &mut deliveries,
    );
    job::enqueue_batch_tx(&mut *tx, &deliveries).await?;
    follow::delete_in_many(&mut *tx, source.id, &follower_replay.follower_ids).await?;
    follow::delete_out_many(&mut *tx, target.id, &severed_target_edges).await?;
    if source.is_local() {
        notification::clear_kind_from_many(
            &mut *tx,
            source.id,
            &follower_replay.follower_ids,
            "follow",
        )
        .await?;
    }
    notification::clear_from_many(&mut *tx, &blocker_ids, target.id).await?;
    conversation::remove_with_participant_many(&mut *tx, &blocker_ids, target.id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;

    // A local target's follow notifications, after the commit like
    // `notify_after_commit`: best effort, and a group's join requests wait in
    // its moderation queue rather than notifying the group account.
    notify_local_move_followers(state, target, &follower_replay.eligible_ids).await;

    // Carry mutes over with the matching skip rule (`skip_mute_move?`),
    // set-based.
    mute::carry_over(&state.pool, source.id, target.id).await?;

    Ok(())
}

/// Replays claimed per tick. Small on purpose: one job can federate thousands
/// of follows, and this queue has no latency requirement — the redirect the
/// user sees was already recorded synchronously.
const MOVE_BATCH_SIZE: i64 = 2;
/// How long the worker sleeps when the queue is empty.
const MOVE_IDLE_POLL: Duration = Duration::from_secs(10);
/// Replay attempts before a move is given up on. The redirect stands either
/// way; what is lost is the local re-follow, which the follower can redo by
/// hand.
const MOVE_MAX_ATTEMPTS: i32 = 5;
/// Backoff base; attempt `n` waits `MOVE_RETRY_BASE * 2^n` (1 min → ~32 min).
const MOVE_RETRY_BASE: Duration = Duration::from_mins(1);

/// Claims and replays one batch of due moves; returns how many were claimed
/// (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match account_move_job::claim_due(&state.pool, MOVE_BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim account move jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        match replay(state, job).await {
            // Settled, or the source/target vanished under us: nothing a retry
            // could recover.
            Ok(()) => complete_move(state, job.id).await,
            Err(error) => {
                tracing::warn!(
                    error = %error.chain(),
                    source = job.source_account_id,
                    target = job.target_account_id,
                    attempts = job.attempts,
                    "account move replay failed"
                );
                if job.attempts >= MOVE_MAX_ATTEMPTS {
                    complete_move(state, job.id).await;
                } else {
                    let shift = u32::try_from(job.attempts.clamp(0, 5)).unwrap_or(0);
                    let delay = MOVE_RETRY_BASE.saturating_mul(1 << shift);
                    if let Err(error) =
                        account_move_job::reschedule(&state.pool, job.id, delay).await
                    {
                        tracing::error!(%error, "failed to reschedule an account move replay");
                    }
                }
            }
        }
    }
    claimed
}

async fn replay(state: &AppState, job: account_move_job::ClaimedMove) -> Result<(), ApiError> {
    let source = account::find_by_id(&state.pool, job.source_account_id).await?;
    let target = account::find_by_id(&state.pool, job.target_account_id).await?;
    let (Some(source), Some(target)) = (source, target) else {
        return Ok(()); // one of them was deleted; there is nothing to re-point
    };
    process_move(state, &source, &target).await
}

async fn complete_move(state: &AppState, id: i64) {
    if let Err(error) = account_move_job::complete(&state.pool, id).await {
        tracing::error!(%error, "failed to remove a settled account move job");
    }
}

/// Runs the move-replay loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("account move worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, MOVE_IDLE_POLL).await {
                return;
            }
        }
    })
}

/// The outcome of an outbound migration.
#[derive(Debug)]
pub struct MigrateOutcome {
    pub target_uri: String,
    /// Remote follower inboxes the `Move` was queued to.
    pub followers_notified: usize,
}

/// Migrates the local account `username` to `target` (a `user@domain` acct or
/// an actor URI). Verifies the target lists this account in its `alsoKnownAs`,
/// records the redirect, re-points local relationships, then tells followers
/// via `Update(Actor)` (carrying `movedTo`) and `Move`. Mastodon's
/// `MoveService`.
pub async fn migrate_local_account(
    state: &AppState,
    username: &str,
    target: &str,
) -> Result<MigrateOutcome, Failure> {
    let source = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    let source_uri = LocalUserUrls::for_account(
        &state.config.domain,
        &source.username,
        source.uri.as_deref(),
    )
    .id;

    // Mastodon allows one move per 30 days; repeat moves inside the window
    // are refused before any federation happens.
    if let Some(last) = account_migration::latest_at(&state.pool, source.id).await?
        && last + MIGRATION_COOLDOWN > time::OffsetDateTime::now_utc()
    {
        return Err(Invalid::Cooldown.into());
    }

    let fetched = resolve_remote_actor(state, target).await?;
    let target_uri = fetched.id.clone();
    if target_uri == source_uri {
        return Err(Invalid::SameAccount.into());
    }
    // Anti-hijack: the destination must already claim this account as an alias.
    if !fetched
        .also_known_as_uris()
        .iter()
        .any(|alias| alias == &source_uri)
    {
        return Err(Invalid::NotAnAlias.into());
    }
    let target_account = refresh_remote_actor(state, &fetched).await?;

    // Count before the replay unfollows the local followers.
    let followers_count = follow::count_followers(&state.pool, source.id).await?;

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    enqueue_move_conn(state, &mut tx, &source, &target_account).await?;
    account_migration::record(
        &mut *tx,
        source.id,
        &display_acct(target, &target_account),
        target_account.id,
        i64::try_from(followers_count).unwrap_or(i64::MAX),
    )
    .await?;
    // Re-load so the published actor document carries `movedTo`.
    let moved = account::find_by_id(&mut *tx, source.id)
        .await?
        .unwrap_or(source);

    // Followers first learn the redirect (`Update(Actor)`), then re-follow the
    // target on the `Move`. Local followers are re-pointed by the queued
    // replay; this fan-out addresses remote inboxes, which that replay does
    // not touch, so the two are independent and their order does not matter.
    let actor = crate::profile::local_actor_conn(state, &mut tx, &moved).await?;
    let update = plamenu_ap::activity::update_actor(
        &state.config.domain,
        &moved.username,
        serde_json::to_value(actor).map_err(|e| ApiError::Internal(Box::new(e)))?,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    );
    actions::fan_out_conn(state, &mut tx, &moved, &update, &[]).await?;

    let move_activity = plamenu_ap::activity::move_account(
        &state.config.domain,
        &moved.username,
        id::next(),
        &target_uri,
    );
    let followers_notified =
        actions::fan_out_conn(state, &mut tx, &moved, &move_activity, &[]).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    tracing::info!(source = %moved.username, target = %target_uri, "local account migrated");
    Ok(MigrateOutcome {
        target_uri,
        followers_notified,
    })
}

/// The `user@domain` form shown for a move target in the migration history:
/// the acct as typed when the user gave one, otherwise reconstructed from the
/// stored account.
fn display_acct(input: &str, account: &Account) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("https://") {
        match &account.domain {
            Some(domain) => format!("{}@{domain}", account.username),
            None => account.username.clone(),
        }
    } else {
        trimmed.trim_start_matches('@').to_owned()
    }
}

/// Adds `alias` (a `user@domain` acct or an actor URI) to a local account's
/// declared aliases (`alsoKnownAs`), so it can be the *destination* of a move
/// from that other account. Returns the canonical alias URI that was stored.
pub async fn add_local_alias(
    state: &AppState,
    username: &str,
    alias: &str,
) -> Result<String, Failure> {
    let source = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    let uri = resolve_remote_actor(state, alias).await?.id;
    let mut aliases = source.also_known_as.clone();
    if !aliases.iter().any(|existing| existing == &uri) {
        aliases.push(uri.clone());
    }
    account::set_aliases(&state.pool, source.id, &aliases).await?;
    // Keep the display row in step with the wire-format array.
    let trimmed = alias.trim();
    let acct_display = if trimmed.starts_with("https://") {
        uri.clone()
    } else {
        trimmed.trim_start_matches('@').to_owned()
    };
    account_alias::add(&state.pool, source.id, &acct_display, &uri).await?;
    Ok(uri)
}

/// Removes `alias` (by URI) from a local account's declared aliases. Returns
/// whether it was present.
pub async fn remove_local_alias(
    state: &AppState,
    username: &str,
    alias_uri: &str,
) -> Result<bool, Failure> {
    let source = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    let before = source.also_known_as.len();
    let aliases: Vec<String> = source
        .also_known_as
        .into_iter()
        .filter(|existing| existing != alias_uri)
        .collect();
    let removed = aliases.len() != before;
    if removed {
        account::set_aliases(&state.pool, source.id, &aliases).await?;
    }
    // The display row may exist even when the array entry is already gone
    // (or vice versa after a backfill) — always clear it.
    account_alias::remove(&state.pool, source.id, alias_uri).await?;
    Ok(removed)
}

/// The declared aliases of a local account.
pub async fn list_local_aliases(state: &AppState, username: &str) -> Result<Vec<String>, ApiError> {
    let source = account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(source.also_known_as)
}

/// Dereferences a target given as a `user@domain` acct (webfinger first) or a
/// direct `https://` actor URI, returning the canonical actor document.
async fn resolve_remote_actor(
    state: &AppState,
    target: &str,
) -> Result<plamenu_ap::actor::RemoteActor, Failure> {
    let uri = if target.starts_with("https://") {
        target.to_owned()
    } else {
        let acct: Acct = target
            .trim_start_matches('@')
            .parse()
            .map_err(|e: plamenu_ap::acct::AcctError| Invalid::BadHandle(e.to_string()))?;
        if state.config.is_local_domain(acct.domain()) {
            return Err(Invalid::LocalTarget.into());
        }
        if !crate::instance_policy::can_federate_domain(
            &state.pool,
            &state.config.domain,
            acct.domain(),
        )
        .await?
        {
            return Err(Invalid::BlockedDomain.into());
        }
        let resolved =
            state
                .federation
                .resolve_acct(&acct)
                .await
                .map_err(|e| Invalid::Unresolvable {
                    acct: acct.to_string(),
                    detail: e.to_string(),
                })?;
        if !crate::instance_policy::can_federate_domain(
            &state.pool,
            &state.config.domain,
            resolved.acct.domain(),
        )
        .await?
        {
            return Err(Invalid::BlockedDomain.into());
        }
        resolved.actor_uri
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &uri).await? {
        return Err(Invalid::BlockedDomain.into());
    }
    state.federation.fetch_actor(&uri).await.map_err(|e| {
        Invalid::Unfetchable {
            uri: uri.clone(),
            detail: e.to_string(),
        }
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_wording_is_unchanged_by_the_typed_refusal() {
        // The web forms render these refusals from the catalog, but the CLI
        // still prints the sentences it always printed, at the same statuses.
        let rendered = |invalid: Invalid| ApiError::from(Failure::Invalid(invalid));
        assert!(matches!(
            rendered(Invalid::Cooldown),
            ApiError::Unprocessable(message)
                if message == "You can only move your account once every 30 days"
        ));
        assert!(matches!(
            rendered(Invalid::SameAccount),
            ApiError::Unprocessable(message)
                if message == "The target account cannot be the same as the account being moved"
        ));
        assert!(matches!(
            rendered(Invalid::NotAnAlias),
            ApiError::Unprocessable(message)
                if message
                    == "The target account does not list this account as an alias (alsoKnownAs)"
        ));
        assert!(matches!(
            rendered(Invalid::LocalTarget),
            ApiError::BadRequest(message) if message == "the target must be a remote account"
        ));
        assert!(matches!(
            rendered(Invalid::BlockedDomain),
            ApiError::BadRequest(message)
                if message == "the target domain is blocked or not allowed"
        ));
        assert!(matches!(
            rendered(Invalid::Unresolvable {
                acct: "vesna@example.com".to_owned(),
                detail: "no webfinger".to_owned(),
            }),
            ApiError::BadRequest(message)
                if message == "cannot resolve vesna@example.com: no webfinger"
        ));
        assert!(matches!(
            rendered(Invalid::Unfetchable {
                uri: "https://example.com/users/vesna".to_owned(),
                detail: "timed out".to_owned(),
            }),
            ApiError::BadRequest(message)
                if message == "cannot fetch https://example.com/users/vesna: timed out"
        ));
    }
}
