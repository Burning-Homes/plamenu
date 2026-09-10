//! Local group lifecycle: the creation service shared by the web UI
//! and the CLI, the instance creation-policy gate, and the posting/announce
//! machinery — the FEP-1b12 host side: accepted member activity is wrapped
//! verbatim in the group's `Announce` and fanned out to its followers.

use plamenu_ap::acct::Acct;
use plamenu_ap::activity;
use plamenu_db::account::{self, Account, NewLocalAccount};
use plamenu_db::group::{self, Affiliation, Group, MembershipPolicy, NewLocalGroup, PostingPolicy};
use plamenu_db::instance_settings::GroupCreationPolicy;
use plamenu_db::status::{self, Status};
use plamenu_db::{follow, id, mention, notification, pin, username_block};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::auth;
use crate::error::ApiError;

/// Mastodon's local-username length cap; group names share the account
/// namespace, so they share the limit.
const NAME_LENGTH_LIMIT: usize = 30;

/// Per-account ceiling on local groups one member may create. Group creation is
/// open to every account by default and mints an RSA-2048 actor key per group,
/// so without a quota one account could hoard actor keys and rows and drive
/// unbounded key generation. Generous — far above any real
/// member's need — since its only job is to bound abuse; the operator CLI
/// (`enforce_account_quota == false`) is exempt.
pub const MAX_GROUPS_PER_ACCOUNT: i64 = 50;

/// Whether an account may create groups under `policy`. `Approved` equals
/// staff-only until the per-account grant queue ships with the admin
/// console.
#[must_use]
pub fn may_create(policy: GroupCreationPolicy, is_staff: bool) -> bool {
    match policy {
        GroupCreationPolicy::Everyone => true,
        GroupCreationPolicy::Admins | GroupCreationPolicy::Approved => is_staff,
    }
}

/// A group creation request, already gated by [`may_create`] (web) or the
/// operator's authority (CLI).
#[derive(Debug)]
pub struct CreateGroupParams<'a> {
    pub name: &'a str,
    pub display_name: &'a str,
    pub membership_policy: MembershipPolicy,
    /// Who may start a thread in the group.
    pub posting_policy: PostingPolicy,
    /// The creating account — recorded as `created_by` and seeded as owner.
    pub created_by: i64,
    /// Web callers enforce the username blocklist (M34); the CLI, like
    /// `account add`, is the operator speaking and skips it.
    pub enforce_username_blocklist: bool,
    /// Web callers enforce the per-account group quota
    /// ([`MAX_GROUPS_PER_ACCOUNT`]); the operator CLI is exempt.
    pub enforce_account_quota: bool,
}

/// Why a group could not be created: a typed refusal the web form can phrase
/// in the reader's language while the CLI keeps the wording it always printed
/// (the `oauth_app::Invalid` pattern; see `docs/LOCALIZATION.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateInvalid {
    NameBlank,
    NameTooLong,
    NameInvalid,
    NameReserved,
    NameTaken,
    DisplayNameTooLong,
    Quota,
}

impl std::fmt::Display for CreateInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameBlank => f.write_str("Name can't be blank"),
            Self::NameTooLong => write!(
                f,
                "Name is too long (maximum is {NAME_LENGTH_LIMIT} characters)"
            ),
            Self::NameInvalid => {
                f.write_str("Name must contain only letters, numbers and underscores")
            }
            Self::NameReserved => f.write_str("Name is reserved"),
            Self::NameTaken => f.write_str("Name has already been taken"),
            Self::DisplayNameTooLong => {
                f.write_str("Display name is too long (maximum is 30 characters)")
            }
            Self::Quota => write!(
                f,
                "You have reached the limit of {MAX_GROUPS_PER_ACCOUNT} groups"
            ),
        }
    }
}

/// How creation can fail: a typed refusal the caller may want to phrase
/// itself, or an ordinary error raised while carrying it out.
#[derive(Debug)]
pub enum CreateFailure {
    Invalid(CreateInvalid),
    Api(ApiError),
}

impl From<CreateInvalid> for CreateFailure {
    fn from(invalid: CreateInvalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<ApiError> for CreateFailure {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<plamenu_db::DbError> for CreateFailure {
    fn from(err: plamenu_db::DbError) -> Self {
        Self::Api(err.into())
    }
}

impl From<CreateFailure> for ApiError {
    /// Reproduces the wording each refusal carried before it was typed, so
    /// the CLI sees no change.
    fn from(failure: CreateFailure) -> Self {
        match failure {
            CreateFailure::Invalid(invalid) => {
                Self::Unprocessable(format!("Validation failed: {invalid}"))
            }
            CreateFailure::Api(err) => err,
        }
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(format!("Validation failed: {message}"))
}

/// Creates a local group: validation, keypair, the account + sidecar + owner
/// rows, Ed25519 backfill. No `account.created` webhook — that event means a
/// person signed up, and groups aren't people.
pub async fn create_group(
    state: &AppState,
    params: CreateGroupParams<'_>,
) -> Result<(Account, Group), CreateFailure> {
    let name = params.name.trim();
    if name.is_empty() {
        return Err(CreateInvalid::NameBlank.into());
    }
    if name.chars().count() > NAME_LENGTH_LIMIT {
        return Err(CreateInvalid::NameTooLong.into());
    }
    if Acct::new(name, &state.config.domain).is_err() {
        return Err(CreateInvalid::NameInvalid.into());
    }
    if params.enforce_username_blocklist
        // Both blocklist modes stop a group: "needs approval" has no meaning
        // for group names, so it degrades to a plain block.
        && (username_block::matches(&state.pool, name, false).await?
            || username_block::matches(&state.pool, name, true).await?)
    {
        return Err(CreateInvalid::NameReserved.into());
    }
    if account::local_username_reserved(&state.pool, name).await? {
        return Err(CreateInvalid::NameTaken.into());
    }
    if params.display_name.chars().count() > 30 {
        return Err(CreateInvalid::DisplayNameTooLong.into());
    }
    // Per-account quota, checked before any RSA work. The count
    // races concurrent creates from the same account, but the shared
    // credential-crypto gate bounds how many run at once, so the small overshoot
    // a race allows is immaterial.
    if params.enforce_account_quota
        && group::count_created_by(&state.pool, params.created_by).await? >= MAX_GROUPS_PER_ACCOUNT
    {
        return Err(CreateInvalid::Quota.into());
    }
    // RSA-2048 generation runs on the blocking pool behind the shared crypto
    // gate so a burst of creates can't monopolize the runtime.
    let keypair = auth::generate_keypair_gated().await?;
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        CreateFailure::Api(ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        )))
    })?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let (account, group) = group::create_immutable_tx(
        &mut tx,
        NewLocalGroup {
            account: NewLocalAccount {
                username: name,
                display_name: params.display_name.trim(),
                note: "",
                public_key_pem: &keypair.public_pem,
            },
            membership_policy: params.membership_policy,
            posting_policy: params.posting_policy,
            created_by: params.created_by,
        },
        &state.config.domain,
    )
    .await
    .map_err(|err| match err {
        plamenu_db::DbError::UsernameTaken => CreateFailure::from(CreateInvalid::NameTaken),
        other => other.into(),
    })?;
    crate::key_store::provision_account_tx(
        &mut tx,
        keyring,
        &state.config.domain,
        &account,
        &keypair,
        &ed25519,
    )
    .await
    .map_err(|error| CreateFailure::Api(ApiError::Internal(Box::new(error))))?;
    tx.commit()
        .await
        .map_err(plamenu_db::DbError::from)
        .map_err(CreateFailure::from)?;
    // The creator joins their own group, like Lemmy subscribes a community's
    // creator — local edge, no Accept round-trip.
    plamenu_db::follow::create(&state.pool, params.created_by, account.id, None).await?;
    tracing::info!(group = %account.username, created_by = params.created_by, "local group created");
    Ok((account, group))
}

/// The group-related fields of a composed local post, pre-validation.
#[derive(Debug)]
pub struct SubmissionParams<'a> {
    pub group_id: Option<i64>,
    pub title: Option<&'a str>,
    pub external_url: Option<&'a str>,
    pub visibility: &'a str,
    pub has_poll: bool,
    /// The post kind the author picked (E4/long-form): `Event` and `Article` both
    /// carry a title of their own, outside any group.
    pub kind: plamenu_ap::activity::PostKind,
}

/// Lemmy's post-title length cap; shared so titles round-trip unclipped.
const TITLE_LENGTH_LIMIT: usize = 200;

/// Validates a composed post's group fields and resolves its target group
/// accounts (F4B §4). An explicit `group_id` is a top-level submission and
/// must satisfy the posting policy; replies inherit their parent's groups,
/// permission-filtered *silently* — a non-member's reply still posts
/// socially, the group just won't announce it.
pub async fn resolve_compose_targets(
    state: &AppState,
    author: &Account,
    submission: &SubmissionParams<'_>,
    parent: Option<&Status>,
) -> Result<Vec<Account>, ApiError> {
    if let Some(title) = submission.title {
        // An event's title is not optional: it is what a calendar, an invitation
        // and every consumer's list view display, and Mobilizon's own model
        // requires one. A long-form post's title is its headline, and federates
        // as `name` plus a leading heading. So both kinds carry a title outside a
        // group; an ordinary Note still may not.
        let titled_kind = matches!(
            submission.kind,
            plamenu_ap::activity::PostKind::Event | plamenu_ap::activity::PostKind::Article
        );
        if submission.group_id.is_none() && !titled_kind {
            return Err(invalid("Titles are only supported on group posts"));
        }
        if title.chars().count() > TITLE_LENGTH_LIMIT {
            return Err(invalid(&format!(
                "Title is too long (maximum is {TITLE_LENGTH_LIMIT} characters)"
            )));
        }
        if submission.has_poll {
            return Err(invalid("A titled post can't carry a poll"));
        }
    }
    if let Some(url) = submission.external_url {
        if submission.title.is_none() {
            return Err(invalid("Link posts need a title"));
        }
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(invalid("Link is not a valid URL"));
        }
    }
    if let Some(group_id) = submission.group_id {
        if parent.is_some() {
            return Err(invalid("Replies inherit their parent's group"));
        }
        let group_account = account::find_by_id(&state.pool, group_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if submission.visibility != "public" {
            return Err(invalid("Group posts must be public"));
        }
        if let Some(group) = group::find(&state.pool, group_id).await? {
            // A local group we host: the sidecar posting policy decides.
            if !may_submit(state, &group, author.id, true).await? {
                return Err(invalid("You are not allowed to post to this group"));
            }
        } else {
            // A remote community (Lemmy/Mbin/PieFed) has no local policy row: we
            // don't own it, so we defer to the remote's own policy and let it
            // enforce on delivery. We only refuse to post to something we
            // suspended locally; following is NOT required — it only affects
            // whether the post comes back attributed as group content (the
            // community relays its `Announce` to followers). A rejected or
            // un-attributed post degrades gracefully to an ordinary post.
            if !group_account.is_group() {
                return Err(ApiError::NotFound);
            }
            if group_account.suspended() {
                return Err(invalid("This community is suspended on this server"));
            }
        }
        return Ok(vec![group_account]);
    }
    let Some(parent) = parent else {
        return Ok(Vec::new());
    };
    let mut targets = Vec::new();
    for g in group::groups_of_status(&state.pool, parent.id).await? {
        if may_submit(state, &g, author.id, false).await?
            && !thread_locked_for_reply(state, g.account_id, parent).await?
            && let Some(group_account) = account::find_by_id(&state.pool, g.account_id).await?
        {
            targets.push(group_account);
        }
    }
    // A remote community can lock a thread we consume. Unlike a local group
    // — where a locked thread silently drops the group from delivery and a
    // moderator may still reply — we don't own the remote's policy, and a reply
    // it would refuse must never become an orphan local status. Only a *public*
    // reply is ever candidate group content the community receives (a
    // direct/private/local reply never federates to it), so scope the hard
    // refusal to public replies. Refuse up front when the thread's community
    // (found via its boost-row attribution on the root) has locked it.
    if submission.visibility == "public" {
        let root_id = status::thread_root(&state.pool, parent.id).await?;
        if let Some(root) = status::find_by_id(&state.pool, root_id).await? {
            for community in communities_of_status(state, &root).await? {
                // Local groups are attributed by mention rows and were already
                // gated (with `may_submit`) by the loop above.
                if community.is_local() || !community.is_group() {
                    continue;
                }
                // A locked thread must never spawn an orphan reply.
                if thread_locked_for_reply(state, community.id, parent).await? {
                    return Err(invalid("This thread is locked"));
                }
                // The decisive fix for outbound replies into a remote community:
                // a remote Lemmy post is attributed by the community's BOOST row,
                // not a mention, so the mention-based loop above never finds it
                // and the reply used to reach only the parent author's personal
                // inbox — where Lemmy stores it but never runs its announce
                // fan-out, so it stayed invisible on the origin. Add the
                // community as a delivery target: downstream (`actions.rs`) then
                // stamps the reply Note's `audience`/`cc` = community and
                // delivers the `Create` to the community's own inbox, so the
                // origin announces it to followers. We don't own its policy —
                // defer to the remote, refusing only a lock or suspension.
                if community.suspended() || targets.iter().any(|t| t.id == community.id) {
                    continue;
                }
                targets.push(community);
            }
        }
    }
    Ok(targets)
}

/// Whether `sender_id` may submit to the group (posting policy).
/// Outcasts never; owners/moderators always. A top-level post is then decided
/// by the group's posting policy: `Anyone` (any non-outcast), `Members` (an
/// accepted follow), or `Mods` (owner/mods only). Comments only need a clean
/// record whatever the policy — Lemmy's semantics (the policy restricts
/// threads, not replies), and a Mastodon user replying to a boosted group post
/// arrives without membership.
pub async fn may_submit(
    state: &AppState,
    group: &Group,
    sender_id: i64,
    top_level: bool,
) -> Result<bool, ApiError> {
    match group::affiliation_of(&state.pool, group.account_id, sender_id).await? {
        Some(Affiliation::Outcast) => Ok(false),
        Some(Affiliation::Owner | Affiliation::Moderator) => Ok(true),
        None if !top_level => Ok(true),
        None => match group.posting_policy() {
            PostingPolicy::Anyone => Ok(true),
            PostingPolicy::Members => {
                Ok(group::is_member(&state.pool, group.account_id, sender_id).await?)
            }
            PostingPolicy::Mods => Ok(false),
        },
    }
}

/// Whether `status_id` is a group post — attributed to a local group (the
/// mention rows) or boosted by any Group actor, local or remote. Decides
/// vote semantics: a `Dislike` is a downvote here and a legacy
/// Misskey reaction-withdrawal everywhere else, and only group posts render
/// vote buttons.
pub async fn is_group_post(state: &AppState, status_id: i64) -> Result<bool, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    is_group_post_conn(state, &mut conn, status_id).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn is_group_post_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    status_id: i64,
) -> Result<bool, ApiError> {
    Ok(!group::group_attributed_of(&mut *conn, &[status_id])
        .await?
        .is_empty())
}

/// Whether `sender_id` may vote on `status_id`: outcasts of any of
/// the target's local groups are dropped at the gate — like their
/// submissions; everyone else votes (Lemmy accepts votes from any actor with
/// a clean record, membership not required).
pub async fn may_vote(state: &AppState, sender_id: i64, status_id: i64) -> Result<bool, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    may_vote_conn(state, &mut conn, sender_id, status_id).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn may_vote_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    sender_id: i64,
    status_id: i64,
) -> Result<bool, ApiError> {
    for g in group::groups_of_status(&mut *conn, status_id).await? {
        if group::affiliation_of(&mut *conn, g.account_id, sender_id).await?
            == Some(Affiliation::Outcast)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Announces a voter's `Like`/`Dislike` (or an `Undo` of one) through every
/// local group of the target — wrapper-only, exactly how Lemmy relays votes,
/// so follower instances' scores converge. Public targets only: the group
/// never widens an audience. Callers gate outcasts via [`may_vote`].
pub async fn announce_vote(state: &AppState, target: &Status, raw: &Value) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    announce_vote_conn(state, &mut conn, target, raw).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn announce_vote_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    target: &Status,
    raw: &Value,
) -> Result<(), ApiError> {
    if target.visibility != "public" {
        return Ok(());
    }
    for g in group::groups_of_status(&mut *conn, target.id).await? {
        let Some(group_account) = account::find_by_id(&mut *conn, g.account_id).await? else {
            continue;
        };
        announce_wrapped_conn(state, conn, &group_account, raw).await?;
    }
    Ok(())
}

/// Fans a group's FEP-1b12 `Announce` wrapper out to its followers: the
/// accepted activity embedded verbatim, so the author's own integrity proof
/// (when present) keeps authenticating it downstream.
pub async fn announce_wrapped(
    state: &AppState,
    group_account: &Account,
    inner: &Value,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    announce_wrapped_conn(state, &mut conn, group_account, inner).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn announce_wrapped_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
    inner: &Value,
) -> Result<(), ApiError> {
    let wrapper = activity::group_announce(
        &state.config.domain,
        &group_account.username,
        id::next(),
        inner.clone(),
    );
    crate::actions::fan_out_conn(state, conn, group_account, &wrapper, &[]).await?;
    Ok(())
}

/// Announces an activity authored by one of our local actors through a local
/// group. The group wrapper itself is rebased by `fan_out`, but FEP-1b12
/// requires the inner activity to be embedded verbatim, so its author identity
/// must already be canonical before it is nested.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn announce_local_wrapped_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
    local_actor: &Account,
    inner: &Value,
) -> Result<(), ApiError> {
    let canonical = activity::rebase_local_identity(
        inner,
        &state.config.domain,
        &local_actor.username,
        local_actor.uri.as_deref(),
    );
    announce_wrapped_conn(state, conn, group_account, &canonical).await
}

/// Announces an accepted submission to the group's followers: the verbatim
/// wrapper always; a top-level post additionally gets a boost row (what puts
/// it into member timelines and the group page) and the Mastodon-compat bare
/// `Announce(uri)` — Lemmy's exact double-send; FEP-1b12 receivers
/// included) dedup the pair on the boost row. Callers gate on
/// `visibility == "public"`: announcing would widen a non-public audience.
pub async fn announce_submission(
    state: &AppState,
    group_account: &Account,
    stored: &Status,
    inner: &Value,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let boost_id = announce_submission_conn(state, &mut tx, group_account, stored, inner).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if let Some(boost_id) = boost_id {
        crate::streaming::status_created(state, boost_id).await;
    }
    Ok(())
}

/// Group publication shares the authoring transaction and returns a stream id.
pub async fn announce_submission_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
    stored: &Status,
    inner: &Value,
) -> Result<Option<i64>, ApiError> {
    announce_wrapped_conn(state, conn, group_account, inner).await?;
    if stored.in_reply_to_id.is_some() {
        return Ok(None); // comments are announced wrapper-only, as on group ingest
    }
    let boost = status::create_local_reblog_conn(conn, group_account.id, stored.id).await?;
    let author = account::find_by_id(&mut *conn, stored.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let compat = activity::announce(
        &state.config.domain,
        &group_account.username,
        boost.id,
        &crate::entities::status_uri_for_account(&state.config.domain, stored, &author),
        &crate::actions::published_of(&boost)?,
    );
    crate::actions::fan_out_conn(state, conn, group_account, &compat, &[]).await?;
    Ok(Some(boost.id))
}

/// The account rows of every local group `stored` is submitted to — for
/// delete paths, which must capture them before the cascade removes the
/// attributing mention rows.
pub async fn group_accounts_of_status(
    state: &AppState,
    stored: &Status,
) -> Result<Vec<Account>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    group_accounts_of_status_conn(state, &mut conn, stored).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn group_accounts_of_status_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
) -> Result<Vec<Account>, ApiError> {
    let group_ids: Vec<i64> = group::groups_of_status(&mut *conn, stored.id)
        .await?
        .iter()
        .map(|g| g.account_id)
        .collect();
    accounts_by_ids_in_order_conn(state, conn, &group_ids).await
}

/// The account rows for `ids` in one query, restored to the ids' order; ids
/// with no row are silently dropped (matching the per-id `find_by_id` skip
/// the group helpers used).
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn accounts_by_ids_in_order_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    ids: &[i64],
) -> Result<Vec<Account>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows: std::collections::HashMap<i64, Account> = account::find_by_ids(&mut *conn, ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    Ok(ids.iter().filter_map(|id| rows.remove(id)).collect())
}

/// Every community `stored` belongs to, local and remote: local groups
/// carry a silent mention row; a remote community's attribution is instead the
/// boost row it created when it announced the post back. Deduplicated by
/// account id. Used to re-address a group post's `Update`/`Delete` and to
/// stamp the community claim on every representation of the object.
pub async fn communities_of_status(
    state: &AppState,
    stored: &Status,
) -> Result<Vec<Account>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    communities_of_status_conn(state, &mut conn, stored).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn communities_of_status_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
) -> Result<Vec<Account>, ApiError> {
    let mut accounts = group_accounts_of_status_conn(state, conn, stored).await?;
    let extra_ids: Vec<i64> = group::boosting_group_ids(&mut *conn, stored.id)
        .await?
        .into_iter()
        .filter(|group_id| !accounts.iter().any(|a| a.id == *group_id))
        .collect();
    accounts.extend(accounts_by_ids_in_order_conn(state, conn, &extra_ids).await?);
    Ok(accounts)
}

/// The canonical actor URI of an account — its stored `uri` for a remote
/// actor, the local URL layout for a local one. The `audience`/community claim
/// a group post carries on the wire.
#[must_use]
pub fn community_uri(state: &AppState, account: &Account) -> String {
    account.uri.clone().unwrap_or_else(|| {
        plamenu_ap::urls::LocalUserUrls::for_account(
            &state.config.domain,
            &account.username,
            account.uri.as_deref(),
        )
        .id
    })
}

/// The `(uri, inbox)` of every *remote* community `stored` belongs to —
/// where an author's `Update`/`Delete` of a community post must be delivered so
/// the origin re-announces the change. Local groups are excluded (they relay
/// via their own `Announce`, handled separately).
pub async fn remote_community_inboxes(
    state: &AppState,
    stored: &Status,
) -> Result<Vec<(String, String)>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    remote_community_inboxes_conn(state, &mut conn, stored).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn remote_community_inboxes_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
) -> Result<Vec<(String, String)>, ApiError> {
    let mut targets = Vec::new();
    for account in communities_of_status_conn(state, conn, stored).await? {
        if account.is_local() {
            continue;
        }
        if let Some(uri) = account.uri.clone() {
            targets.push((uri, account.inbox_url.clone()));
        }
    }
    Ok(targets)
}

/// A group boost captured before its target's deletion cascades it away —
/// everything needed to retract the compat bare `Announce` afterwards.
pub struct CapturedBoost {
    pub group_account: Account,
    pub boost_id: i64,
    pub published: String,
    pub target_uri: String,
}

/// Captures every local group's boost of `stored` (with the target's URI and
/// the boost's publish time) so [`retract_boost`] can still build the
/// `Undo(Announce)` after the delete cascade removed the rows.
pub async fn capture_boosts(
    state: &AppState,
    stored: &Status,
    author: &Account,
) -> Result<Vec<CapturedBoost>, ApiError> {
    let mut captured = Vec::new();
    for g in group::groups_of_status(&state.pool, stored.id).await? {
        let Some(group_account) = account::find_by_id(&state.pool, g.account_id).await? else {
            continue;
        };
        let Some(boost) = status::find_reblog_by(&state.pool, g.account_id, stored.id).await?
        else {
            continue;
        };
        captured.push(CapturedBoost {
            boost_id: boost.id,
            published: crate::actions::published_of(&boost)?,
            target_uri: crate::entities::status_uri_for_account(
                &state.config.domain,
                stored,
                author,
            ),
            group_account,
        });
    }
    Ok(captured)
}

/// Retracts a group's compat bare `Announce` after its target was deleted:
/// `Undo(Announce)` to the group's followers, so plain-Announce consumers
/// (which can't read the wrapped `Delete`) drop the boost too.
pub async fn retract_boost(state: &AppState, captured: &CapturedBoost) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    retract_boost_conn(state, &mut conn, captured).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn retract_boost_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    captured: &CapturedBoost,
) -> Result<(), ApiError> {
    let announce = activity::announce(
        &state.config.domain,
        &captured.group_account.username,
        captured.boost_id,
        &captured.target_uri,
        &captured.published,
    );
    let undo = activity::undo(
        &state.config.domain,
        &captured.group_account.username,
        announce,
    );
    crate::actions::fan_out_conn(state, conn, &captured.group_account, &undo, &[]).await?;
    Ok(())
}

/// Handles a freshly-ingested remote status' group submissions (F4B §2): for
/// every local group it addresses (the silent audience-mention rows stored at
/// ingest), validate the sender and announce. A rejected submission detaches
/// the group's mention row, so the status never counts as group content; a
/// non-public submission is ingested but never announced — the group must not
/// widen its audience.
pub async fn process_inbound_submission(
    state: &AppState,
    sender: &Account,
    stored: &Status,
    raw_activity: &Value,
) -> Result<(), ApiError> {
    for g in group::groups_of_status(&state.pool, stored.id).await? {
        if !may_submit(state, &g, sender.id, stored.in_reply_to_id.is_none()).await? {
            mention::detach(&state.pool, stored.id, g.account_id).await?;
            tracing::debug!(
                group = g.account_id,
                sender = %sender.username,
                "dropping group submission from non-member, outcast or restricted poster"
            );
            continue;
        }
        // A comment into a locked thread is dropped (Lemmy's `Lock`); the lock
        // lives on the thread root, so walk there.
        if stored.in_reply_to_id.is_some() {
            let root = status::thread_root(&state.pool, stored.id).await?;
            if group::thread_locked(&state.pool, g.account_id, root).await? {
                mention::detach(&state.pool, stored.id, g.account_id).await?;
                tracing::debug!(
                    group = g.account_id,
                    "dropping comment into a locked group thread"
                );
                continue;
            }
        }
        if stored.visibility != "public" {
            continue;
        }
        let Some(group_account) = account::find_by_id(&state.pool, g.account_id).await? else {
            continue;
        };
        announce_submission(state, &group_account, stored, raw_activity).await?;
    }
    Ok(())
}

/// Re-announces an author's `Update`/`Delete` of a group submission to every
/// local group it belongs to — wrapper-only (there is no boost row to touch;
/// deletes retract theirs via [`retract_boost`]). Public submissions only,
/// mirroring the announce gate.
pub async fn reannounce_to_groups(
    state: &AppState,
    stored: &Status,
    raw_activity: &Value,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    reannounce_to_groups_conn(state, &mut conn, stored, raw_activity).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn reannounce_to_groups_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    stored: &Status,
    raw_activity: &Value,
) -> Result<(), ApiError> {
    if stored.visibility != "public" {
        return Ok(());
    }
    for g in group::groups_of_status(&mut *conn, stored.id).await? {
        let Some(group_account) = account::find_by_id(&mut *conn, g.account_id).await? else {
            continue;
        };
        announce_wrapped_conn(state, conn, &group_account, raw_activity).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Membership & moderation
// ---------------------------------------------------------------------------

/// Asserts `actor_id` is an owner or moderator of the group, returning the
/// rank (owner outranks moderator). Outcasts and plain members are refused.
pub async fn require_moderator(
    state: &AppState,
    group_account_id: i64,
    actor_id: i64,
) -> Result<Affiliation, ApiError> {
    match group::affiliation_of(&state.pool, group_account_id, actor_id).await? {
        Some(rank @ (Affiliation::Owner | Affiliation::Moderator)) => Ok(rank),
        _ => Err(ApiError::Forbidden(
            "You are not a moderator of this group".into(),
        )),
    }
}

/// The db effect of a ban, shared by the local ban action and an inbound
/// remote-moderator `Block`: record the outcast row (with an optional expiry
/// for temp bans) and sever the membership follow so the ban takes hold
/// immediately.
pub async fn apply_ban(
    state: &AppState,
    group_account_id: i64,
    target_id: i64,
    expires: Option<OffsetDateTime>,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    apply_ban_conn(state, &mut conn, group_account_id, target_id, expires).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn apply_ban_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account_id: i64,
    target_id: i64,
    expires: Option<OffsetDateTime>,
) -> Result<(), ApiError> {
    group::set_affiliation(
        &mut *conn,
        group_account_id,
        target_id,
        Affiliation::Outcast,
        expires,
    )
    .await?;
    follow::delete(&mut *conn, target_id, group_account_id).await?;
    // A pending join request is withdrawn by the ban too.
    notification::clear_kind_from(&mut *conn, group_account_id, target_id, "follow_request")
        .await?;
    Ok(())
}

/// Lifts a ban: removes the outcast row, but never touches an owner/moderator
/// row (unban is only ever offered for a banned account). Returns whether a
/// ban was actually lifted.
pub async fn apply_unban(
    state: &AppState,
    group_account_id: i64,
    target_id: i64,
) -> Result<bool, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    apply_unban_conn(state, &mut conn, group_account_id, target_id).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn apply_unban_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account_id: i64,
    target_id: i64,
) -> Result<bool, ApiError> {
    if group::affiliation_of(&mut *conn, group_account_id, target_id).await?
        != Some(Affiliation::Outcast)
    {
        return Ok(false);
    }
    Ok(group::remove_affiliation(&mut *conn, group_account_id, target_id).await?)
}

/// Bans `target` from the group (local moderator action): records the outcast,
/// then announces the wrapped `Block` to the group's followers so remote
/// members' servers drop the actor. The banned member still holds their follow
/// when the announce fans out, so their server receives it before the kick.
pub async fn ban_member(
    state: &AppState,
    group_account: &Account,
    mod_actor: &Account,
    target: &Account,
    expires: Option<OffsetDateTime>,
    reason: Option<&str>,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    let group_uri = group_uri(state, group_account);
    let target_uri = actor_uri(state, target);
    let expires_str = expires.and_then(|e| e.format(&Rfc3339).ok());
    let block = activity::group_ban(
        &state.config.domain,
        &mod_actor.username,
        id::next(),
        &target_uri,
        &group_uri,
        false,
        expires_str.as_deref(),
        reason,
    );
    announce_local_wrapped_conn(state, &mut tx, group_account, mod_actor, &block).await?;
    apply_ban_conn(state, &mut tx, group_account.id, target.id, expires).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Unbans `target` (local moderator action): lifts the outcast row and
/// announces the wrapped `Undo(Block)`.
pub async fn unban_member(
    state: &AppState,
    group_account: &Account,
    mod_actor: &Account,
    target: &Account,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    if !apply_unban_conn(state, &mut tx, group_account.id, target.id).await? {
        return Ok(());
    }
    let group_uri = group_uri(state, group_account);
    let target_uri = actor_uri(state, target);
    let block = activity::group_ban(
        &state.config.domain,
        &mod_actor.username,
        id::next(),
        &target_uri,
        &group_uri,
        false,
        None,
        None,
    );
    let undo = activity::group_undo(&state.config.domain, &mod_actor.username, &group_uri, block);
    announce_local_wrapped_conn(state, &mut tx, group_account, mod_actor, &undo).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// The db effect of removing a post/comment from the group: retract the
/// compat bare `Announce` (so plain-Announce consumers drop the boost), delete
/// the group's boost row (it leaves member timelines and the group page), and
/// detach the group attribution so the status is no longer group content (a
/// later author edit is never re-announced). The local status itself survives.
pub async fn apply_remove(
    state: &AppState,
    group_account: &Account,
    status: &Status,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    apply_remove_conn(state, &mut conn, group_account, status).await?;
    conn.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn apply_remove_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
    status: &Status,
) -> Result<(), ApiError> {
    if status.in_reply_to_id.is_none()
        && let Some(boost) = status::find_reblog_by(&mut *conn, group_account.id, status.id).await?
    {
        if let Some(author) = account::find_by_id(&mut *conn, status.account_id).await? {
            let captured = CapturedBoost {
                boost_id: boost.id,
                published: crate::actions::published_of(&boost)?,
                target_uri: crate::entities::status_uri_for_account(
                    &state.config.domain,
                    status,
                    &author,
                ),
                group_account: group_account.clone(),
            };
            retract_boost_conn(state, conn, &captured).await?;
        }
        status::delete_local_conn(&mut *conn, boost.id, group_account.id).await?;
    }
    mention::detach(&mut *conn, status.id, group_account.id).await?;
    Ok(())
}

/// Removes a post/comment from the group (local moderator action, Lemmy's
/// mod-`Delete` carrying a reason): announces the wrapped removal to the
/// group's followers, then applies the local retraction.
pub async fn remove_from_group(
    state: &AppState,
    group_account: &Account,
    mod_actor: &Account,
    status: &Status,
    reason: &str,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    let author = account::find_by_id(&mut *tx, status.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let object_uri = crate::entities::status_uri_for_account(&state.config.domain, status, &author);
    let group_uri = group_uri(state, group_account);
    let remove = activity::group_remove(
        &state.config.domain,
        &mod_actor.username,
        id::next(),
        &object_uri,
        &group_uri,
        reason,
    );
    announce_local_wrapped_conn(state, &mut tx, group_account, mod_actor, &remove).await?;
    apply_remove_conn(state, &mut tx, group_account, status).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Locks or unlocks a thread in the group (local moderator action): records
/// the lock and announces the wrapped `Lock` / `Undo(Lock)`. `root` is the
/// top-level group post.
pub async fn set_thread_lock(
    state: &AppState,
    group_account: &Account,
    mod_actor: &Account,
    root: &Status,
    lock: bool,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    if lock {
        group::lock_thread(&mut *tx, group_account.id, root.id).await?;
    } else {
        group::unlock_thread(&mut *tx, group_account.id, root.id).await?;
    }
    let author = account::find_by_id(&mut *tx, root.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let object_uri = crate::entities::status_uri_for_account(&state.config.domain, root, &author);
    let group_uri = group_uri(state, group_account);
    let lock_activity = activity::group_lock(
        &state.config.domain,
        &mod_actor.username,
        id::next(),
        &object_uri,
        &group_uri,
    );
    let wrapped = if lock {
        lock_activity
    } else {
        activity::group_undo(
            &state.config.domain,
            &mod_actor.username,
            &group_uri,
            lock_activity,
        )
    };
    announce_local_wrapped_conn(state, &mut tx, group_account, mod_actor, &wrapped).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Pins or unpins a group post (local moderator action): records the pin on
/// the group's `featured` collection and announces the wrapped `Add`/`Remove`.
pub async fn set_group_pin(
    state: &AppState,
    group_account: &Account,
    mod_actor: &Account,
    status: &Status,
    pin_it: bool,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    if pin_it {
        pin::create(&mut *tx, group_account.id, status.id).await?;
    } else {
        pin::delete(&mut *tx, group_account.id, status.id).await?;
    }
    let author = account::find_by_id(&mut *tx, status.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let object_uri = crate::entities::status_uri_for_account(&state.config.domain, status, &author);
    let group_uri = group_uri(state, group_account);
    let feature = activity::group_feature(
        &state.config.domain,
        &mod_actor.username,
        id::next(),
        &object_uri,
        &group_uri,
        pin_it,
    );
    announce_local_wrapped_conn(state, &mut tx, group_account, mod_actor, &feature).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Grants or revokes moderator on `target` (owner action): updates the
/// affiliation and announces the wrapped `Add`/`Remove` targeting the group's
/// moderators collection. Never changes the owner row.
pub async fn set_moderator(
    state: &AppState,
    group_account: &Account,
    owner_actor: &Account,
    target: &Account,
    grant: bool,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    if grant {
        group::set_affiliation(
            &mut *tx,
            group_account.id,
            target.id,
            Affiliation::Moderator,
            None,
        )
        .await?;
    } else if group::affiliation_of(&mut *tx, group_account.id, target.id).await?
        == Some(Affiliation::Moderator)
    {
        group::remove_affiliation(&mut *tx, group_account.id, target.id).await?;
    } else {
        return Ok(());
    }
    let target_uri = actor_uri(state, target);
    let group_uri = group_uri(state, group_account);
    let change = activity::group_moderator(
        &state.config.domain,
        &owner_actor.username,
        id::next(),
        &target_uri,
        &group_uri,
        grant,
    );
    announce_local_wrapped_conn(state, &mut tx, group_account, owner_actor, &change).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

// ---- Lifecycle: profile updates, deletion, owner transfer ----------------

/// The group's owner as a full account, or `None` for a malformed group with
/// no owner row (callers treat that as skippable/internal).
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn owner_account_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account_id: i64,
) -> Result<Option<Account>, ApiError> {
    let Some(owner_id) = group::owner(&mut *conn, group_account_id).await? else {
        return Ok(None);
    };
    Ok(account::find_by_id(&mut *conn, owner_id).await?)
}

/// Federates a group profile/settings change with Lemmy's double-send: the
/// plain `Update(Actor)` (so Mastodon followers of the group actor refresh it)
/// *and* the Lemmy-shaped `Announce(Update(Group))` authored by the owner (so
/// subscribing Lemmy communities — which reject a self-authored `Update` via
/// `verify_mod_action` — accept it and refresh their cached community). Call
/// this wherever a group's actor document changes.
pub async fn fan_out_group_profile(
    state: &AppState,
    group_account: &Account,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    fan_out_group_profile_conn(state, &mut conn, group_account).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn fan_out_group_profile_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
) -> Result<(), ApiError> {
    crate::profile::fan_out_actor_update_conn(state, conn, group_account).await?;
    announce_group_update_conn(state, conn, group_account).await
}

/// The Lemmy `UpdateCommunity`-compat half of [`fan_out_group_profile`]: builds
/// the refreshed Group document (its own `@context` stripped so the wire
/// matches Lemmy's — the wrapper already carries one), wraps it in an `Update`
/// authored by the owner, and announces it to the group's followers. A group
/// missing its owner row logs and no-ops; the plain `Update(Actor)` still went
/// out.
pub async fn announce_group_update(
    state: &AppState,
    group_account: &Account,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    announce_group_update_conn(state, &mut conn, group_account).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn announce_group_update_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    group_account: &Account,
) -> Result<(), ApiError> {
    let Some(owner) = owner_account_conn(state, conn, group_account.id).await? else {
        tracing::warn!(group = %group_account.username, "group has no owner; skipping Update(Group)");
        return Ok(());
    };
    let mut doc =
        serde_json::to_value(crate::profile::local_actor_conn(state, conn, group_account).await?)
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if let Some(map) = doc.as_object_mut() {
        map.remove("@context");
    }
    let group_uri = group_uri(state, group_account);
    let update = activity::group_update(
        &state.config.domain,
        &owner.username,
        id::next(),
        &group_uri,
        doc,
    );
    announce_local_wrapped_conn(state, conn, group_account, &owner, &update).await
}

/// The editable group profile/settings, applied together by [`update_settings`]
/// so the group console and the admin console share one write + fan-out path.
/// `note_html` is the already-composed description; `note_source` its markdown.
pub struct GroupSettings<'a> {
    pub display_name: &'a str,
    pub note_html: &'a str,
    pub note_source: &'a str,
    pub policy: MembershipPolicy,
    pub sensitive: bool,
    pub posting_policy: PostingPolicy,
    /// Listing in the local groups directory (`accounts.discoverable`);
    /// federates as `toot:discoverable` like any profile.
    pub discoverable: bool,
    /// The group's avatar, banner and metadata fields. Edited from the group
    /// console's multipart form; the admin console passes the default, which
    /// leaves every one of them untouched.
    pub profile: GroupProfileEdit,
}

/// A group's profile media and metadata. A group is a local account with the
/// same avatar/header/fields columns as a person, but no `users` row and so no
/// session — nothing could ever reach `update_credentials` on its behalf, which
/// is why these travel with the group settings instead.
///
/// `None` everywhere means "leave unchanged", matching
/// [`crate::profile::ProfileChanges`].
#[derive(Debug, Default)]
pub struct GroupProfileEdit {
    /// Raw uploaded image bytes, processed like any profile image.
    pub avatar: Option<Vec<u8>>,
    pub header: Option<Vec<u8>>,
    /// Alt text, published as the image's `summary` (what Mastodon reads).
    pub avatar_description: Option<String>,
    pub header_description: Option<String>,
    /// Raw `(name, value)` pairs; an empty list clears them.
    pub fields: Option<Vec<(String, String)>>,
}

/// Applies a group's settings change and federates it (the double-send).
/// Approval mode rides `manuallyApprovesFollowers` (`locked`), matching the
/// actor contract. Reloads before the fan-out so the serialized actor
/// carries the new values.
#[allow(
    clippy::too_many_lines,
    reason = "group profile mutation with upload rollback and post-commit cleanup"
)]
pub async fn update_settings(
    state: &AppState,
    group_account: &Account,
    settings: GroupSettings<'_>,
) -> Result<(), ApiError> {
    let approval = settings.policy == MembershipPolicy::Approval;
    // Uploads are processed before anything is written, so a rejected image
    // leaves the group's settings as they were.
    let avatar_file = match settings.profile.avatar {
        Some(bytes) => Some(
            crate::profile::store_profile_image(
                state,
                bytes,
                crate::media_processing::AVATAR_MAX_EDGE,
            )
            .await?,
        ),
        None => None,
    };
    let header_file = match settings.profile.header {
        Some(bytes) => Some(
            crate::profile::store_profile_image(
                state,
                bytes,
                crate::media_processing::HEADER_MAX_EDGE,
            )
            .await?,
        ),
        None => None,
    };
    let fields = settings.profile.fields.as_ref().map(|fields| {
        fields
            .iter()
            .filter(|(name, value)| !(name.trim().is_empty() && value.trim().is_empty()))
            .map(|(name, value)| account::FieldPair {
                name: name.trim().to_owned(),
                value: value.trim().to_owned(),
            })
            .collect()
    });
    let update = account::ProfileUpdate {
        display_name: Some(settings.display_name),
        note: Some(settings.note_html),
        note_source: Some(settings.note_source),
        locked: Some(approval),
        discoverable: Some(settings.discoverable),
        fields,
        avatar_file_name: avatar_file.as_ref().map(|(name, _)| name.as_str()),
        header_file_name: header_file.as_ref().map(|(name, _)| name.as_str()),
        avatar_file_size: avatar_file.as_ref().map(|(_, size)| *size),
        header_file_size: header_file.as_ref().map(|(_, size)| *size),
        avatar_description: settings.profile.avatar_description.as_deref(),
        header_description: settings.profile.header_description.as_deref(),
        ..account::ProfileUpdate::default()
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result: Result<Option<Account>, ApiError> = async {
        group::set_membership_policy(&mut *tx, group_account.id, settings.policy).await?;
        group::set_posting_policy(&mut *tx, group_account.id, settings.posting_policy).await?;
        group::set_sensitive(&mut *tx, group_account.id, settings.sensitive).await?;
        let updated = account::update_local_profile_conn(&mut tx, group_account.id, update).await?;
        // A changed field may carry a URL to verify; the worker re-checks every
        // URL field of the account, so one enqueue covers them all.
        if settings.profile.fields.is_some() {
            plamenu_db::link_verification::enqueue(&mut *tx, group_account.id).await?;
        }
        if let Some(refreshed) = account::find_by_id(&mut *tx, group_account.id).await? {
            fan_out_group_profile_conn(state, &mut tx, &refreshed).await?;
        }
        Ok(updated)
    }
    .await;
    let updated = match result {
        Ok(updated) => updated,
        Err(error) => {
            drop(tx);
            crate::profile::enqueue_stray_profile_images(
                state,
                avatar_file.as_ref(),
                header_file.as_ref(),
            )
            .await;
            return Err(error);
        }
    };
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    // A replaced avatar/header has no `media_attachments` row for the orphan
    // sweep to find, and `/media/{file}` serves any valid key without a DB
    // check — so the superseded file is removed now, exactly as for a person's
    // profile.
    if let Some(updated) = &updated {
        for (replaced, old, new) in [
            (
                avatar_file.is_some(),
                group_account.avatar_file_name.as_deref(),
                updated.avatar_file_name.as_deref(),
            ),
            (
                header_file.is_some(),
                group_account.header_file_name.as_deref(),
                updated.header_file_name.as_deref(),
            ),
        ] {
            if replaced
                && let Some(old) = old
                && new != Some(old)
            {
                crate::profile::delete_superseded_profile_image(state, old).await;
            }
        }
    }
    Ok(())
}

/// Deletes a hosted group (owner or instance-admin action). Fans out the
/// Lemmy-shaped `Announce(Delete(Group))` — owner as the acting moderator, so
/// subscribing communities mark their cache deleted — then reuses the account
/// self-deletion machinery, which federates the plain `Delete(Actor)` for
/// Mastodon followers, tombstones the actor (`410 Gone`) and purges its
/// content. Both fan-outs enqueue before the purge, so the group's key still
/// signs them; the suspension the tombstone rides drops the group from every
/// public directory automatically.
pub async fn delete_group(state: &AppState, group_account: &Account) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;

    if let Some(owner) = owner_account_conn(state, &mut tx, group_account.id).await? {
        let group_uri = group_uri(state, group_account);
        let delete = activity::group_delete(
            &state.config.domain,
            &owner.username,
            id::next(),
            &group_uri,
        );
        announce_local_wrapped_conn(state, &mut tx, group_account, &owner, &delete).await?;
    }
    crate::moderation::self_delete_account_conn(state, &mut tx, group_account).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Transfers group ownership to another local member (owner-only from the group
/// console, or an instance admin from the admin console). The previous owner is
/// demoted to moderator — they keep their mod powers, matching the confirmed
/// design — and the new owner is promoted; if the new owner wasn't already a
/// moderator, the wrapped `Add` to the moderators collection is federated (an
/// owner is always a moderator on the wire). Refreshes the actor so the
/// FEP-5219 affiliations collection (which publishes the owner as `admin`)
/// re-federates. `new_owner` must be a local, accepted member.
pub async fn transfer_owner(
    state: &AppState,
    group_account: &Account,
    new_owner: &Account,
) -> Result<(), ApiError> {
    let Some(old_owner_id) = group::owner(&state.pool, group_account.id).await? else {
        return Err(ApiError::Internal("group has no owner".into()));
    };
    if old_owner_id == new_owner.id {
        return Ok(()); // already the owner — nothing to do
    }
    if !new_owner.is_local() {
        return Err(invalid(
            "ownership can only be transferred to a local member",
        ));
    }
    if !group::is_member(&state.pool, group_account.id, new_owner.id).await? {
        return Err(invalid("the new owner must be a member of the group"));
    }
    let already_mod = group::affiliation_of(&state.pool, group_account.id, new_owner.id).await?
        == Some(Affiliation::Moderator);
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Demote the current owner to moderator, then promote the new one. The PK
    // is (group, account), so both are plain upserts.
    group::set_affiliation(
        &mut *tx,
        group_account.id,
        old_owner_id,
        Affiliation::Moderator,
        None,
    )
    .await?;
    group::set_affiliation(
        &mut *tx,
        group_account.id,
        new_owner.id,
        Affiliation::Owner,
        None,
    )
    .await?;
    if !already_mod {
        // The demoted previous owner is still a moderator, so it signs the Add
        // (a mod granting mod, Lemmy's `AddMod` shape). Fall back to the new
        // owner as actor if the old owner vanished mid-transfer.
        let actor = account::find_by_id(&mut *tx, old_owner_id)
            .await?
            .unwrap_or_else(|| new_owner.clone());
        let target_uri = actor_uri(state, new_owner);
        let group_uri = group_uri(state, group_account);
        let add = activity::group_moderator(
            &state.config.domain,
            &actor.username,
            id::next(),
            &target_uri,
            &group_uri,
            true,
        );
        announce_local_wrapped_conn(state, &mut tx, group_account, &actor, &add).await?;
    }
    announce_group_update_conn(state, &mut tx, group_account).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Whether a reply into `group_account_id` must be dropped because its thread
/// is locked. Only comments can violate a lock; a top-level post has none. The
/// lock lives on the thread root, so we walk `parent` up to it.
pub async fn thread_locked_for_reply(
    state: &AppState,
    group_account_id: i64,
    parent: &Status,
) -> Result<bool, ApiError> {
    let root = status::thread_root(&state.pool, parent.id).await?;
    Ok(group::thread_locked(&state.pool, group_account_id, root).await?)
}

/// A group actor's `ActivityPub` id.
fn group_uri(state: &AppState, group_account: &Account) -> String {
    plamenu_ap::urls::LocalUserUrls::for_account(
        &state.config.domain,
        &group_account.username,
        group_account.uri.as_deref(),
    )
    .id
}

/// An actor's `ActivityPub` id — its stored `uri` for a remote account, the
/// minted local url otherwise.
fn actor_uri(state: &AppState, account: &Account) -> String {
    account.uri.clone().unwrap_or_else(|| {
        plamenu_ap::urls::LocalUserUrls::for_account(
            &state.config.domain,
            &account.username,
            account.uri.as_deref(),
        )
        .id
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typed create refusals must keep printing the sentences the CLI
    /// (and any future API caller) always saw.
    #[test]
    fn cli_wording_is_unchanged_by_the_typed_refusal() {
        for (refusal, wording) in [
            (CreateInvalid::NameBlank, "Name can't be blank"),
            (
                CreateInvalid::NameTooLong,
                "Name is too long (maximum is 30 characters)",
            ),
            (
                CreateInvalid::NameInvalid,
                "Name must contain only letters, numbers and underscores",
            ),
            (CreateInvalid::NameReserved, "Name is reserved"),
            (CreateInvalid::NameTaken, "Name has already been taken"),
            (
                CreateInvalid::DisplayNameTooLong,
                "Display name is too long (maximum is 30 characters)",
            ),
            (
                CreateInvalid::Quota,
                "You have reached the limit of 50 groups",
            ),
        ] {
            let ApiError::Unprocessable(message) = ApiError::from(CreateFailure::from(refusal))
            else {
                panic!("create refusals map onto Unprocessable");
            };
            assert_eq!(message, format!("Validation failed: {wording}"));
        }
    }
}
