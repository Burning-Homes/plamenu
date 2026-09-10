//! Local groups: a group is an `accounts` row with
//! `actor_type = 'Group'`, no `users` row, and a `groups` sidecar row here.
//! Plain membership is an accepted `follows` row (subscription == membership,
//! Lemmy's model); `group_affiliations` stores only elevated roles and bans,
//! per FEP-5219.

use std::collections::HashSet;

use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;

use crate::account::{Account, NewLocalAccount};
use crate::{DbError, id};

/// Who becomes a member on follow: instantly (`open`) or after a moderator
/// approves the request (`approval`, riding the locked-account machinery).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MembershipPolicy {
    #[default]
    Open,
    Approval,
}

impl MembershipPolicy {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "approval" => Self::Approval,
            _ => Self::Open,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Approval => "approval",
        }
    }
}

/// Who may start a top-level thread in a local group. `Mods` is the only
/// value Lemmy's `postingRestrictedToMods` can express; `Anyone` and `Members`
/// both federate as `postingRestrictedToMods = false` (Lemmy has no
/// must-subscribe-to-post concept), the `Members` restriction being ours to
/// enforce at the inbox. Comments are always open to non-outcasts, whatever the
/// policy — matching Lemmy and the prior behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PostingPolicy {
    /// Any non-outcast may post — no membership required.
    Anyone,
    /// Accepted followers (members) only. The prior default behavior.
    #[default]
    Members,
    /// Owner/moderators only (the prior `posting_restricted_to_mods`).
    Mods,
}

impl PostingPolicy {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "anyone" => Self::Anyone,
            "mods" => Self::Mods,
            _ => Self::Members,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anyone => "anyone",
            Self::Members => "members",
            Self::Mods => "mods",
        }
    }

    /// The federated `postingRestrictedToMods` flag — true only for `Mods`.
    #[must_use]
    pub fn restricted_to_mods(self) -> bool {
        matches!(self, Self::Mods)
    }
}

/// The FEP-5219 affiliation ladder. `Owner` federates as `admin`; plain
/// members are never stored (an accepted follow is the membership).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affiliation {
    Owner,
    Moderator,
    /// Banned from the group; beats a follow row everywhere.
    Outcast,
}

impl Affiliation {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "moderator" => Some(Self::Moderator),
            "outcast" => Some(Self::Outcast),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Moderator => "moderator",
            Self::Outcast => "outcast",
        }
    }
}

/// The `groups` sidecar row of a local Group account.
#[derive(Debug, Clone)]
pub struct Group {
    pub account_id: i64,
    pub membership_policy: String,
    pub posting_policy: String,
    pub sensitive: bool,
    pub created_by: Option<i64>,
    pub created_at: OffsetDateTime,
}

impl Group {
    #[must_use]
    pub fn membership_policy(&self) -> MembershipPolicy {
        MembershipPolicy::parse(&self.membership_policy)
    }

    #[must_use]
    pub fn posting_policy(&self) -> PostingPolicy {
        PostingPolicy::parse(&self.posting_policy)
    }

    /// The federated `postingRestrictedToMods` flag, derived from the policy.
    #[must_use]
    pub fn posting_restricted_to_mods(&self) -> bool {
        self.posting_policy().restricted_to_mods()
    }
}

/// Everything needed to create a local group.
#[derive(Debug)]
pub struct NewLocalGroup<'a> {
    pub account: NewLocalAccount<'a>,
    pub membership_policy: MembershipPolicy,
    pub posting_policy: PostingPolicy,
    /// The creating account: recorded on the sidecar row and seeded as the
    /// group's `owner` affiliation.
    pub created_by: i64,
}

/// Creates a local group: the Group-typed account (locked when membership
/// needs approval, so `manuallyApprovesFollowers` and the pending-follow
/// machinery fall out), its sidecar row and the creator's `owner`
/// affiliation, atomically. Fails with [`DbError::UsernameTaken`] when the
/// name is already used locally (groups share the account namespace).
pub async fn create(pool: &PgPool, new: NewLocalGroup<'_>) -> Result<(Account, Group), DbError> {
    insert_group(pool, new, id::next(), None).await
}

/// Creates a local Group with the same handle-independent actor-ID scheme as
/// newly registered Person accounts.
pub async fn create_immutable(
    pool: &PgPool,
    new: NewLocalGroup<'_>,
    domain: &str,
) -> Result<(Account, Group), DbError> {
    let account_id = id::next();
    let actor_uri = crate::account::numeric_local_actor_uri(domain, account_id);
    insert_group(pool, new, account_id, Some(&actor_uri)).await
}

/// Transactional form used when encrypted normalized keys must commit with
/// the group account and its sidecars.
pub async fn create_immutable_tx(
    conn: &mut PgConnection,
    new: NewLocalGroup<'_>,
    domain: &str,
) -> Result<(Account, Group), DbError> {
    let account_id = id::next();
    let actor_uri = crate::account::numeric_local_actor_uri(domain, account_id);
    insert_group_tx(conn, new, account_id, Some(&actor_uri)).await
}

async fn insert_group(
    pool: &PgPool,
    new: NewLocalGroup<'_>,
    account_id: i64,
    actor_uri: Option<&str>,
) -> Result<(Account, Group), DbError> {
    let mut tx = pool.begin().await?;
    let result = insert_group_tx(&mut tx, new, account_id, actor_uri).await?;
    tx.commit().await?;
    Ok(result)
}

async fn insert_group_tx(
    tx: &mut PgConnection,
    new: NewLocalGroup<'_>,
    account_id: i64,
    actor_uri: Option<&str>,
) -> Result<(Account, Group), DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        INSERT INTO accounts (id, username, display_name, note, public_key,
                              actor_type, locked, uri)
        VALUES ($1, $2, $3, $4, $5, 'Group', $6, $7)
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
        new.account.username,
        new.account.display_name,
        new.account.note,
        new.account.public_key_pem,
        new.membership_policy == MembershipPolicy::Approval,
        actor_uri,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::UsernameTaken,
        _ => DbError::Sqlx(err),
    })?;
    let group = sqlx::query_as!(
        Group,
        r"
        INSERT INTO groups (account_id, membership_policy, posting_policy, created_by)
        VALUES ($1, $2, $3, $4)
        RETURNING account_id, membership_policy, posting_policy, sensitive,
                  created_by, created_at
        ",
        account.id,
        new.membership_policy.as_str(),
        new.posting_policy.as_str(),
        new.created_by,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO group_affiliations (group_account_id, account_id, affiliation)
         VALUES ($1, $2, 'owner')",
        account.id,
        new.created_by,
    )
    .execute(&mut *tx)
    .await?;
    Ok((account, group))
}

/// The sidecar row of a local group, `None` when `account_id` isn't one.
pub async fn find<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Option<Group>, DbError> {
    let group = sqlx::query_as!(
        Group,
        r#"
        SELECT account_id, membership_policy, posting_policy, sensitive,
               created_by, created_at
        FROM groups
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(group)
}

/// How many local groups `account_id` has created (`groups.created_by`). Backs
/// the per-account group-creation quota: default group creation
/// is open to every account and mints an RSA actor key per group, so an
/// unbounded creator could hoard keys/rows and monopolize crypto threads.
pub async fn count_created_by(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM groups WHERE created_by = $1"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Number of communities hosted by this instance. Kept distinct from the
/// generic local-account counter because Group actors have no `users` row.
pub async fn count_local(pool: &PgPool) -> Result<i64, DbError> {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM groups")
        .fetch_one(pool)
        .await
        .map_err(DbError::from)
}

/// Whether `account_id` is a member of the group. Plain membership is an
/// accepted follow — FEP-5219's rule for public groups, exactly Lemmy's
/// subscription model; no membership rows exist.
pub async fn is_member(
    pool: &PgPool,
    group_account_id: i64,
    account_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one" FROM follows
        WHERE account_id = $2 AND target_account_id = $1 AND NOT pending
        "#,
        group_account_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// The local groups a status is submitted to. Attribution rides the mention
/// rows: inbound submissions get a silent audience mention at ingest, local
/// group posts attach one on compose — so posts *and* comments resolve here,
/// while boost rows exist only for announced top-level posts.
pub async fn groups_of_status<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<Vec<Group>, DbError> {
    let groups = sqlx::query_as!(
        Group,
        r#"
        SELECT g.account_id, g.membership_policy, g.posting_policy,
               g.sensitive, g.created_by, g.created_at
        FROM groups g
        JOIN status_mentions sm ON sm.account_id = g.account_id
        WHERE sm.status_id = $1
        ORDER BY g.account_id
        "#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(groups)
}

/// The `(status_id, group_account_id)` attributions for `status_ids`.
///
/// A local group is recorded as a silent mention; a remote group is recorded
/// by its Announce/boost row (remote consumption deliberately has no local
/// `groups` sidecar or mention row). Returning the account as well as the
/// status lets every renderer preserve the community context instead of
/// reducing it to the old yes/no `group_post` flag.
pub async fn group_attributions_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query_as::<_, (i64, i64)>(
        r"
        SELECT sm.status_id, sm.account_id
        FROM status_mentions sm
        JOIN groups g ON g.account_id = sm.account_id
        WHERE sm.status_id = ANY($1)
        UNION
        SELECT b.reblog_of_id, b.account_id
        FROM statuses b -- STUBKEEP: a boost wrapper never stubs (nothing replies to it)
        JOIN accounts a ON a.id = b.account_id
        WHERE b.reblog_of_id = ANY($1) AND a.actor_type = 'Group'
        ORDER BY 1, 2
        ",
    )
    .bind(status_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every community of a batch of statuses as `(status_id, group_account_id)`,
/// in the order the per-status pair [`groups_of_status`] + [`boosting_group_ids`]
/// yields them: the local groups a status was submitted to first (by account
/// id), then the remote communities that announced it back and are not already
/// named. The batched form of `crate::groups::communities_of_status`, for the
/// wire builders that stamp the community claim on every representation of an
/// object.
///
/// Unlike [`group_attributions_of`], which answers only *whether* a status is
/// attributed, this preserves the local-first ordering — the first entry is the
/// community a `Note` claims as its `audience`.
pub async fn community_attributions_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    if status_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT status_id AS "status_id!", account_id AS "account_id!", mentioned AS "mentioned!"
        FROM (
            SELECT sm.status_id, sm.account_id, TRUE AS mentioned
            FROM status_mentions sm
            JOIN groups g ON g.account_id = sm.account_id
            WHERE sm.status_id = ANY($1)
            UNION
            SELECT b.reblog_of_id, b.account_id, FALSE
            FROM statuses b -- STUBKEEP: a boost wrapper never stubs (nothing replies to it)
            JOIN accounts a ON a.id = b.account_id
            WHERE b.reblog_of_id = ANY($1) AND a.actor_type = 'Group'
        ) attributions
        ORDER BY status_id, mentioned DESC, account_id
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    // A local group matches both arms (a `groups` row *and* `actor_type`), so
    // the UNION can carry it twice with different `mentioned` flags. The sort
    // puts the mention row first; keep that one, like the per-status pair's
    // "skip ids already collected".
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    Ok(rows
        .into_iter()
        .map(|row| (row.status_id, row.account_id))
        .filter(|pair| seen.insert(*pair))
        .collect())
}

/// Which of `status_ids` are group posts. This compatibility projection keeps
/// vote and authorization callers cheap while [`group_attributions_of`] is the
/// richer source used by status rendering.
pub async fn group_attributed_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let mut ids: Vec<i64> = group_attributions_of(pool, status_ids)
        .await?
        .into_iter()
        .map(|(status_id, _)| status_id)
        .collect();
    ids.dedup();
    Ok(ids)
}

/// Group accounts (local and remote) that boosted `status_id` — the vote
/// fan-in targets: a local member's `Like`/`Dislike` of a group post
/// delivers to each remote group's inbox and announces through each local
/// one, exactly as Lemmy clients address the community.
pub async fn boosting_group_ids<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT a.id AS "id!"
        FROM statuses b -- STUBKEEP: a boost wrapper never stubs (nothing replies to it)
        JOIN accounts a ON a.id = b.account_id
        WHERE b.reblog_of_id = $1 AND a.actor_type = 'Group'
        "#,
        status_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The `Top` sort's time window (Lemmy's `t` parameter): posts published
/// within it, ranked by score.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TopWindow {
    Day,
    #[default]
    Week,
    Month,
    All,
}

/// Ordering for a community-scoped compatibility feed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineSort {
    Active,
    Hot,
    #[default]
    New,
    Old,
    Top(TopWindow),
}

impl TimelineSort {
    const fn key(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Hot => "hot",
            Self::New => "new",
            Self::Old => "old",
            Self::Top(_) => "top",
        }
    }

    fn window_seconds(self) -> Option<f64> {
        match self {
            Self::Top(window) => window.seconds(),
            _ => None,
        }
    }
}

/// Root posts attributed to one local or remote community. Attribution can be
/// either Plamenu's silent group mention (local submissions) or a Group-owned
/// boost (remote `Announce`). Querying the attribution itself avoids losing an
/// older community post merely because it fell outside a global-feed window.
#[allow(
    clippy::too_many_lines,
    reason = "the compile-checked query and its planner-shape comments must stay together"
)]
pub async fn attributed_timeline(
    pool: &PgPool,
    group_account_id: i64,
    viewer: Option<i64>,
    sort: TimelineSort,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::status::Status>, DbError> {
    let statuses = sqlx::query_as!(
        crate::status::Status,
        r#"
        WITH attributed AS MATERIALIZED (
            SELECT sm.status_id
            FROM status_mentions sm
            WHERE sm.account_id = $1
            UNION
            SELECT b.reblog_of_id
            FROM statuses b -- STUBKEEP: group Announce wrappers never stub
            WHERE b.account_id = $1 AND b.reblog_of_id IS NOT NULL
        ), candidates AS MATERIALIZED (
            -- The lateral boundaries keep the small attribution set driving
            -- primary-key lookups. Without them PostgreSQL may hash-join it
            -- against all statuses/accounts, which turns a 2,000-post group
            -- page into two full-table scans. Account memoization still
            -- deduplicates repeated authors inside the materialized CTE.
            SELECT s.*, author.domain AS author_domain,
                   author.portable AS author_portable,
                   author.suspended_at AS author_suspended_at
            FROM attributed gp
            CROSS JOIN LATERAL (
                SELECT * FROM statuses s0 WHERE s0.id = gp.status_id OFFSET 0
            ) s
            CROSS JOIN LATERAL (
                SELECT a.domain, a.portable, a.suspended_at
                FROM accounts a WHERE a.id = s.account_id OFFSET 0
            ) author
        )
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url,
               s.quote_approval_policy, s.application_id, s.title, s.object_type,
               s.external_url
        FROM candidates s
        LEFT JOIN LATERAL (
            SELECT
                (SELECT count(*) FROM favourites f WHERE f.status_id = s.id)
                  - (SELECT count(*) FROM status_dislikes d WHERE d.status_id = s.id) AS score,
                greatest(
                    s.created_at,
                    coalesce(
                        (SELECT max(reply.created_at) FROM statuses reply
                         WHERE reply.in_reply_to_id = s.id AND reply.deleted_at IS NULL),
                        s.created_at
                    )
                ) AS active_at
            WHERE $3 IN ('top', 'hot', 'active')
        ) rank ON true
        WHERE s.reblog_of_id IS NULL
          AND s.in_reply_to_id IS NULL
          AND s.in_reply_to_uri IS NULL
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.visibility = 'public'
          AND s.author_suspended_at IS NULL
          -- Inline the instance-domain policy as fixed hashed sets. Calling
          -- instance_domain_allowed once per candidate inflates the estimated
          -- plan above PostgreSQL's JIT threshold even when the tables are
          -- empty, adding compilation work to every page request.
          AND (s.author_portable OR s.author_domain IS NULL OR (
                s.author_domain NOT IN (
                    SELECT db.domain FROM domain_blocks db
                    WHERE db.severity = 'suspend')
                AND (NOT EXISTS (SELECT 1 FROM domain_allows)
                     OR s.author_domain IN (SELECT da.domain FROM domain_allows da))))
          -- Authenticated viewers are local accounts, so an author's
          -- account-domain block against the viewer's NULL domain can never
          -- match. Inline the remaining relationship arms so PostgreSQL
          -- builds each sparse set once instead of invoking account_hidden
          -- for every attributed candidate.
          AND ($2::bigint IS NULL OR $2 = s.account_id OR (
                s.account_id NOT IN (
                    SELECT b.target_account_id FROM blocks b WHERE b.account_id = $2)
                AND s.account_id NOT IN (
                    SELECT b.account_id FROM blocks b WHERE b.target_account_id = $2)
                AND s.account_id NOT IN (
                    SELECT m.target_account_id FROM mutes m
                    WHERE m.account_id = $2
                      AND (m.expires_at IS NULL OR m.expires_at > now()))
                AND (s.author_portable OR s.author_domain IS NULL OR s.author_domain NOT IN (
                    SELECT adb.domain FROM account_domain_blocks adb
                    WHERE adb.account_id = $2))))
          AND ($4::float8 IS NULL OR s.created_at >= now() - make_interval(secs => $4))
        ORDER BY
          CASE WHEN $3 = 'old' THEN s.created_at END ASC,
          CASE WHEN $3 = 'top' THEN rank.score END DESC,
          CASE WHEN $3 = 'hot' THEN
            ln(greatest(1.0, 3.0 + rank.score)::float8)
              / power((extract(epoch FROM now() - s.created_at) / 3600.0)::float8 + 2.0, 1.8)
          END DESC,
          CASE WHEN $3 = 'active' THEN rank.active_at END DESC,
          CASE WHEN $3 = 'new' THEN s.created_at END DESC,
          s.id DESC
        LIMIT $5 OFFSET $6
        "#,
        group_account_id,
        viewer,
        sort.key(),
        sort.window_seconds(),
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

impl TopWindow {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "day" => Self::Day,
            "month" => Self::Month,
            "all" => Self::All,
            _ => Self::Week,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::All => "all",
        }
    }

    /// The window length in seconds; `None` means unbounded (`all`).
    #[must_use]
    pub fn seconds(self) -> Option<f64> {
        match self {
            Self::Day => Some(86_400.0),
            Self::Week => Some(604_800.0),
            Self::Month => Some(2_592_000.0),
            Self::All => None,
        }
    }
}

/// The group timeline's boost rows ranked by target score
/// (`favourites - dislikes`, Lemmy's Top) over posts published within the
/// window, ties newest-first. Offset-paginated — rank orders don't keyset.
pub async fn timeline_top(
    pool: &PgPool,
    group_account_id: i64,
    window: TopWindow,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::status::Status>, DbError> {
    let statuses = sqlx::query_as!(
        crate::status::Status,
        r#"
        SELECT b.id, b.uri, b.account_id, b.content, b.created_at, b.updated_at,
               b.visibility, b.in_reply_to_id, b.reblog_of_id, b.edited_at,
               b.spoiler_text, b.sensitive, b.language, b.url, b.quote_approval_policy,
               b.application_id, b.title, b.object_type, b.external_url
        FROM statuses b
        JOIN statuses t ON t.id = b.reblog_of_id AND t.deleted_at IS NULL -- STUBFILTER: drop announces of deleted posts
        CROSS JOIN LATERAL (
            SELECT (SELECT count(*) FROM favourites f WHERE f.status_id = t.id)
                 - (SELECT count(*) FROM status_dislikes d WHERE d.status_id = t.id) AS score
        ) v
        WHERE b.account_id = $1
          AND ($2::float8 IS NULL OR t.created_at >= now() - make_interval(secs => $2))
        ORDER BY v.score DESC, b.id DESC
        LIMIT $3 OFFSET $4
        "#,
        group_account_id,
        window.seconds(),
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// The group timeline's boost rows ranked hot: Lemmy's hot rank,
/// `log(max(1, 3 + score)) / (age_hours + 2)^1.8` — score buys rank,
/// age decays it polynomially; everything at score ≤ -2 collapses to zero
/// and falls back to recency. Computed live (no rank table) — the group
/// benches guard the cost.
pub async fn timeline_hot(
    pool: &PgPool,
    group_account_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::status::Status>, DbError> {
    let statuses = sqlx::query_as!(
        crate::status::Status,
        r#"
        SELECT b.id, b.uri, b.account_id, b.content, b.created_at, b.updated_at,
               b.visibility, b.in_reply_to_id, b.reblog_of_id, b.edited_at,
               b.spoiler_text, b.sensitive, b.language, b.url, b.quote_approval_policy,
               b.application_id, b.title, b.object_type, b.external_url
        FROM statuses b
        JOIN statuses t ON t.id = b.reblog_of_id AND t.deleted_at IS NULL -- STUBFILTER: drop announces of deleted posts
        CROSS JOIN LATERAL (
            SELECT (SELECT count(*) FROM favourites f WHERE f.status_id = t.id)
                 - (SELECT count(*) FROM status_dislikes d WHERE d.status_id = t.id) AS score
        ) v
        WHERE b.account_id = $1
        ORDER BY ln(greatest(1.0, 3.0 + v.score)::float8)
                 / power((extract(epoch FROM now() - t.created_at) / 3600.0)::float8 + 2.0, 1.8)
                 DESC,
                 b.id DESC
        LIMIT $2 OFFSET $3
        "#,
        group_account_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(statuses)
}

/// One elevated affiliation (owner or moderator) of a group.
#[derive(Debug)]
pub struct AffiliationEntry {
    pub account_id: i64,
    pub affiliation: String,
}

/// The group's owner and moderators — the accounts published in the
/// FEP-1b12 moderators and FEP-5219 affiliations collections. Owner first,
/// then moderators by appointment time.
pub async fn elevated(
    pool: &PgPool,
    group_account_id: i64,
) -> Result<Vec<AffiliationEntry>, DbError> {
    let entries = sqlx::query_as!(
        AffiliationEntry,
        r#"
        SELECT account_id, affiliation
        FROM group_affiliations
        WHERE group_account_id = $1 AND affiliation IN ('owner', 'moderator')
        ORDER BY affiliation = 'owner' DESC, created_at, account_id
        "#,
        group_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// The group's owner account id (the single `owner` affiliation row seeded at
/// creation). Returns `None` only for a malformed group with no owner —
/// callers treat that as an internal error.
pub async fn owner<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
) -> Result<Option<i64>, DbError> {
    let row = sqlx::query_scalar!(
        r#"
        SELECT account_id
        FROM group_affiliations
        WHERE group_account_id = $1 AND affiliation = 'owner'
        LIMIT 1
        "#,
        group_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// A banned account with its ban expiry (`None` = indefinite) — the group
/// moderation "bans" list.
pub struct Ban {
    pub account_id: i64,
    pub expires_at: Option<OffsetDateTime>,
}

/// The group's current bans (outcast rows, including not-yet-expired temp
/// bans), newest first.
pub async fn outcasts(pool: &PgPool, group_account_id: i64) -> Result<Vec<Ban>, DbError> {
    let rows = sqlx::query_as!(
        Ban,
        r#"
        SELECT account_id, expires_at
        FROM group_affiliations
        WHERE group_account_id = $1 AND affiliation = 'outcast'
          AND (expires_at IS NULL OR expires_at > now())
        ORDER BY created_at DESC
        "#,
        group_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// `account_id`'s current affiliation with the group, ignoring expired bans.
pub async fn affiliation_of<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    account_id: i64,
) -> Result<Option<Affiliation>, DbError> {
    let row = sqlx::query_scalar!(
        r#"
        SELECT affiliation
        FROM group_affiliations
        WHERE group_account_id = $1 AND account_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        group_account_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.as_deref().and_then(Affiliation::parse))
}

/// [`affiliation_of`] against many groups in one query, keyed by group
/// account id — the thread page resolves the viewer's role in every
/// community of the focus post with one round trip. Groups where the account
/// holds no live affiliation are absent.
pub async fn affiliations_of(
    pool: &PgPool,
    group_account_ids: &[i64],
    account_id: i64,
) -> Result<std::collections::HashMap<i64, Affiliation>, DbError> {
    if group_account_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT group_account_id, affiliation
        FROM group_affiliations
        WHERE group_account_id = ANY($1) AND account_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        group_account_ids,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Affiliation::parse(&row.affiliation).map(|a| (row.group_account_id, a)))
        .collect())
}

/// Sets the community's posting policy — who may start a thread. The
/// `Mods` value is what publishes `postingRestrictedToMods` on the actor; the
/// group-settings page pairs it with the rest of the community flags.
pub async fn set_posting_policy<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    policy: PostingPolicy,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE groups SET posting_policy = $2 WHERE account_id = $1",
        group_account_id,
        policy.as_str(),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets the community-wide sensitive flag (Lemmy `sensitive`, published on the
/// actor). Paired with the mods-only flag on the group-settings page.
pub async fn set_sensitive<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    sensitive: bool,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE groups SET sensitive = $2 WHERE account_id = $1",
        group_account_id,
        sensitive,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Changes the membership policy (`open`/`approval`). The caller must also flip
/// `accounts.locked` to match (`manuallyApprovesFollowers`), keeping the actor
/// document and the follow path consistent.
pub async fn set_membership_policy<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    policy: MembershipPolicy,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE groups SET membership_policy = $2 WHERE account_id = $1",
        group_account_id,
        policy.as_str(),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Grants (or changes) `account_id`'s affiliation. `expires_at` only makes
/// sense for `Outcast` (temporary bans).
pub async fn set_affiliation<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    account_id: i64,
    affiliation: Affiliation,
    expires_at: Option<OffsetDateTime>,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO group_affiliations (group_account_id, account_id, affiliation, expires_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (group_account_id, account_id)
         DO UPDATE SET affiliation = EXCLUDED.affiliation, expires_at = EXCLUDED.expires_at",
        group_account_id,
        account_id,
        affiliation.as_str(),
        expires_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Drops `account_id`'s affiliation (unban, demote). Returns whether a row
/// existed.
pub async fn remove_affiliation<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    account_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM group_affiliations WHERE group_account_id = $1 AND account_id = $2",
        group_account_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Local groups, newest first — the web directory listing, offset-paged.
/// `include_unlisted` also returns groups that opted out of the directory
/// (`discoverable = false`) — the operator CLI wants all of them, the public
/// directory does not.
pub async fn local_group_ids(
    pool: &PgPool,
    include_unlisted: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT g.account_id
        FROM groups g
        JOIN accounts a ON a.id = g.account_id
        WHERE a.suspended_at IS NULL
          AND ($1 OR a.discoverable IS NOT FALSE)
        ORDER BY g.created_at DESC
        LIMIT $2 OFFSET $3
        "#,
        include_unlisted,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Known local and remote communities for the Lemmy `All` directory. Remote
/// Group actors do not have a `groups` sidecar and commonly omit Mastodon's
/// `discoverable` extension, so the local-only directory query cannot represent
/// them. Instance policy and suspension still apply.
pub async fn public_group_ids(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT a.id
        FROM accounts a
        WHERE a.actor_type = 'Group'
          AND NOT a.is_internal
          AND a.suspended_at IS NULL
          AND a.deleted_at IS NULL
          AND (a.portable OR instance_domain_allowed(a.domain))
          AND ((a.domain IS NOT NULL AND NOT a.portable)
               OR a.discoverable IS NOT FALSE)
        ORDER BY a.created_at DESC, a.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// A row of the instance groups admin console: enough to render the list
/// without a per-row round trip. Unlike the public directory this includes
/// suspended and deleted groups, so staff can find and act on them.
pub struct AdminGroupRow {
    pub account_id: i64,
    pub username: String,
    pub display_name: String,
    pub membership_policy: String,
    pub suspended: bool,
    pub deleted: bool,
    pub member_count: i64,
    pub created_at: OffsetDateTime,
}

/// Local groups for the admin console: optional case-insensitive `search` over
/// username/display name, keyset-paginated on the account id (a snowflake, so
/// id order is creation order), newest first. Includes suspended/deleted groups
/// with their state flags.
pub async fn admin_list(
    pool: &PgPool,
    search: Option<&str>,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<AdminGroupRow>, DbError> {
    let pattern = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));
    let rows = sqlx::query_as!(
        AdminGroupRow,
        r#"
        SELECT
            g.account_id AS "account_id!",
            a.username,
            a.display_name,
            g.membership_policy,
            (a.suspended_at IS NOT NULL) AS "suspended!",
            (a.deleted_at IS NOT NULL) AS "deleted!",
            (
                SELECT COUNT(*) FROM follows f
                WHERE f.target_account_id = g.account_id AND NOT f.pending
            ) AS "member_count!",
            g.created_at
        FROM groups g
        JOIN accounts a ON a.id = g.account_id
        WHERE ($1::text IS NULL
               OR a.username ILIKE $1 OR a.display_name ILIKE $1)
          AND ($2::bigint IS NULL OR g.account_id < $2)
        ORDER BY g.account_id DESC
        LIMIT $3
        "#,
        pattern.as_deref(),
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Group accounts (local or remote) `account_id` is a member of — accepted
/// follows toward `actor_type = 'Group'` actors, newest first.
pub async fn joined_group_ids(pool: &PgPool, account_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.target_account_id
        FROM follows f
        JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND NOT f.pending
          AND a.actor_type = 'Group' AND a.suspended_at IS NULL
        ORDER BY f.id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Local and remote groups where `account_id` is an owner or moderator.
pub async fn moderated_group_ids(pool: &PgPool, account_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT ga.group_account_id
        FROM group_affiliations ga
        JOIN accounts a ON a.id = ga.group_account_id
        WHERE ga.account_id = $1
          AND ga.affiliation IN ('owner', 'moderator')
          AND (ga.expires_at IS NULL OR ga.expires_at > now())
          AND a.suspended_at IS NULL
        ORDER BY ga.created_at DESC, ga.group_account_id
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// The group's members — accepted followers — as account ids, newest first,
/// keyset-paginated on the follow row id. The group moderation "members & bans"
/// page. Outcasts can still hold a follow row until the ban severs it, so this
/// is the raw subscriber list; the caller annotates affiliations.
pub async fn members(
    pool: &PgPool,
    group_account_id: i64,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT f.account_id
        FROM follows f
        WHERE f.target_account_id = $1 AND NOT f.pending
          AND ($2::bigint IS NULL OR f.id < $2)
        ORDER BY f.id DESC
        LIMIT $3
        "#,
        group_account_id,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// The group's member count (accepted followers) — the header stat.
pub async fn member_count(pool: &PgPool, group_account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM follows
           WHERE target_account_id = $1 AND NOT pending"#,
        group_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Lemmy's community aggregate, derived from the same group attribution
/// (`status_mentions`) and membership (`follows`) rows Plamenu uses to render
/// community timelines and authorize posting.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CommunityAggregate {
    pub subscribers: i64,
    pub subscribers_local: i64,
    pub posts: i64,
    pub comments: i64,
    pub active_day: i64,
    pub active_week: i64,
    pub active_month: i64,
    pub active_half_year: i64,
}

pub async fn community_aggregate(
    pool: &PgPool,
    group_account_id: i64,
) -> Result<CommunityAggregate, DbError> {
    sqlx::query_as::<_, CommunityAggregate>(
        r"
        WITH attributed AS (
            SELECT DISTINCT s.id, s.account_id, s.in_reply_to_id, s.created_at
            FROM statuses s
            JOIN status_mentions sm ON sm.status_id = s.id
            WHERE sm.account_id = $1 AND s.reblog_of_id IS NULL
              AND s.deleted_at IS NULL -- STUBFILTER
        )
        SELECT
            (SELECT count(*) FROM follows WHERE target_account_id = $1 AND NOT pending) AS subscribers,
            (SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.account_id
             WHERE f.target_account_id = $1 AND NOT f.pending AND a.domain IS NULL) AS subscribers_local,
            count(*) FILTER (WHERE in_reply_to_id IS NULL) AS posts,
            count(*) FILTER (WHERE in_reply_to_id IS NOT NULL) AS comments,
            count(DISTINCT account_id) FILTER (WHERE created_at >= now() - interval '1 day') AS active_day,
            count(DISTINCT account_id) FILTER (WHERE created_at >= now() - interval '7 days') AS active_week,
            count(DISTINCT account_id) FILTER (WHERE created_at >= now() - interval '30 days') AS active_month,
            count(DISTINCT account_id) FILTER (WHERE created_at >= now() - interval '182 days') AS active_half_year
        FROM attributed
        ",
    )
    .bind(group_account_id)
    .fetch_one(pool)
    .await
    .map_err(DbError::from)
}

/// Locks a thread in the group (Lemmy `Lock`): no new comments. `status_id` is
/// the thread root (top-level group post). Idempotent.
pub async fn lock_thread<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO group_locked_posts (group_account_id, status_id) VALUES ($1, $2)
         ON CONFLICT (group_account_id, status_id) DO NOTHING",
        group_account_id,
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Unlocks a thread (`Undo(Lock)`). Returns whether a lock existed.
pub async fn unlock_thread<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    group_account_id: i64,
    status_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM group_locked_posts WHERE group_account_id = $1 AND status_id = $2",
        group_account_id,
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Whether `status_id` (a thread root) is locked in the group.
pub async fn thread_locked(
    pool: &PgPool,
    group_account_id: i64,
    status_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM group_locked_posts
           WHERE group_account_id = $1 AND status_id = $2"#,
        group_account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Whether any of `group_account_ids` locks thread root `status_id` — the
/// group-scoped batched form of [`thread_locked`], one query however many
/// communities attribute the thread. Deliberately not [`locked_of`]: scoping
/// to the given groups means a stale lock row left after a post was removed
/// from a group stops counting.
pub async fn thread_locked_in_any(
    pool: &PgPool,
    group_account_ids: &[i64],
    status_id: i64,
) -> Result<bool, DbError> {
    if group_account_ids.is_empty() {
        return Ok(false);
    }
    let locked = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM group_locked_posts
            WHERE group_account_id = ANY($1) AND status_id = $2
        ) AS "locked!"
        "#,
        group_account_ids,
        status_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(locked)
}

/// Which of `status_ids` are locked in *any* group — for rendering the lock
/// indicator on a group page's post list without a per-row query.
pub async fn locked_of(pool: &PgPool, status_ids: &[i64]) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT DISTINCT status_id AS "id!" FROM group_locked_posts
           WHERE status_id = ANY($1)"#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn owner(pool: &PgPool) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn hiking(pool: &PgPool, owner_id: i64, policy: MembershipPolicy) -> (Account, Group) {
        create(
            pool,
            NewLocalGroup {
                account: NewLocalAccount {
                    username: "hiking",
                    display_name: "Hiking",
                    note: "",
                    public_key_pem: "pub",
                },
                membership_policy: policy,
                posting_policy: PostingPolicy::Members,
                created_by: owner_id,
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn create_seeds_group_account_sidecar_and_owner(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (account, group) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        assert!(account.is_group());
        assert!(account.is_local());
        assert!(!account.locked);
        assert_eq!(group.membership_policy(), MembershipPolicy::Open);
        assert_eq!(group.created_by, Some(owner_id));
        assert_eq!(
            affiliation_of(&pool, account.id, owner_id).await.unwrap(),
            Some(Affiliation::Owner)
        );
    }

    #[sqlx::test]
    async fn status_attributions_name_local_mentions_and_group_announces(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        let mentioned = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(owner_id, "<p>local submission</p>", "public", None),
        )
        .await
        .unwrap();
        crate::mention::attach(&pool, mentioned.id, group_account.id, true)
            .await
            .unwrap();

        let announced = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(
                owner_id,
                "<p>remote-shaped submission</p>",
                "public",
                None,
            ),
        )
        .await
        .unwrap();
        crate::status::create_local_reblog(&pool, group_account.id, announced.id)
            .await
            .unwrap();

        assert_eq!(
            group_attributions_of(&pool, &[mentioned.id, announced.id])
                .await
                .unwrap(),
            vec![
                (mentioned.id, group_account.id),
                (announced.id, group_account.id)
            ]
        );
        assert_eq!(
            group_attributed_of(&pool, &[mentioned.id, announced.id])
                .await
                .unwrap(),
            vec![mentioned.id, announced.id]
        );
    }

    #[sqlx::test]
    async fn count_created_by_counts_only_this_creators_groups(pool: PgPool) {
        let alice = owner(&pool).await;
        let bob = account::create_local(
            &pool,
            NewLocalAccount {
                username: "bob",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;

        assert_eq!(count_created_by(&pool, alice).await.unwrap(), 0);

        // alice creates two groups, bob one — the count is scoped per creator so
        // it can back the per-account quota.
        for name in ["hiking", "cooking"] {
            create(
                &pool,
                NewLocalGroup {
                    account: NewLocalAccount {
                        username: name,
                        display_name: "",
                        note: "",
                        public_key_pem: "pub",
                    },
                    membership_policy: MembershipPolicy::Open,
                    posting_policy: PostingPolicy::Members,
                    created_by: alice,
                },
            )
            .await
            .unwrap();
        }
        create(
            &pool,
            NewLocalGroup {
                account: NewLocalAccount {
                    username: "cycling",
                    display_name: "",
                    note: "",
                    public_key_pem: "pub",
                },
                membership_policy: MembershipPolicy::Open,
                posting_policy: PostingPolicy::Members,
                created_by: bob,
            },
        )
        .await
        .unwrap();

        assert_eq!(count_created_by(&pool, alice).await.unwrap(), 2);
        assert_eq!(count_created_by(&pool, bob).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn approval_policy_locks_the_account(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (account, group) = hiking(&pool, owner_id, MembershipPolicy::Approval).await;
        assert!(account.locked);
        assert_eq!(group.membership_policy(), MembershipPolicy::Approval);
    }

    #[sqlx::test]
    async fn group_names_share_the_account_namespace(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let clash = create(
            &pool,
            NewLocalGroup {
                account: NewLocalAccount {
                    // Case-insensitive clash with the owner's username.
                    username: "Alice",
                    display_name: "",
                    note: "",
                    public_key_pem: "pub",
                },
                membership_policy: MembershipPolicy::Open,
                posting_policy: PostingPolicy::Members,
                created_by: owner_id,
            },
        )
        .await;
        assert!(matches!(clash, Err(DbError::UsernameTaken)));
        // The failed transaction leaves no orphan Group behind: the name
        // still resolves to the person who owned it all along.
        let resolved = account::find_local_by_username(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.id, owner_id);
        assert!(!resolved.is_group());
    }

    #[sqlx::test]
    async fn elevated_lists_owner_first_and_skips_outcasts(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        let moderator = account::create_local(
            &pool,
            NewLocalAccount {
                username: "bob",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let banned = account::create_local(
            &pool,
            NewLocalAccount {
                username: "mallory",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        set_affiliation(
            &pool,
            group_account.id,
            moderator.id,
            Affiliation::Moderator,
            None,
        )
        .await
        .unwrap();
        set_affiliation(
            &pool,
            group_account.id,
            banned.id,
            Affiliation::Outcast,
            None,
        )
        .await
        .unwrap();
        let entries = elevated(&pool, group_account.id).await.unwrap();
        let listed: Vec<(i64, &str)> = entries
            .iter()
            .map(|e| (e.account_id, e.affiliation.as_str()))
            .collect();
        assert_eq!(
            listed,
            vec![(owner_id, "owner"), (moderator.id, "moderator")]
        );
    }

    #[sqlx::test]
    async fn expired_bans_no_longer_apply(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        let banned = account::create_local(
            &pool,
            NewLocalAccount {
                username: "mallory",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let yesterday = OffsetDateTime::now_utc() - time::Duration::days(1);
        set_affiliation(
            &pool,
            group_account.id,
            banned.id,
            Affiliation::Outcast,
            Some(yesterday),
        )
        .await
        .unwrap();
        assert_eq!(
            affiliation_of(&pool, group_account.id, banned.id)
                .await
                .unwrap(),
            None
        );
        let tomorrow = OffsetDateTime::now_utc() + time::Duration::days(1);
        set_affiliation(
            &pool,
            group_account.id,
            banned.id,
            Affiliation::Outcast,
            Some(tomorrow),
        )
        .await
        .unwrap();
        assert_eq!(
            affiliation_of(&pool, group_account.id, banned.id)
                .await
                .unwrap(),
            Some(Affiliation::Outcast)
        );
        assert!(
            remove_affiliation(&pool, group_account.id, banned.id)
                .await
                .unwrap()
        );
        assert_eq!(
            affiliation_of(&pool, group_account.id, banned.id)
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn directory_and_joined_listings(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        assert_eq!(
            local_group_ids(&pool, false, 50, 0).await.unwrap(),
            vec![group_account.id]
        );
        // Not a member yet.
        assert!(joined_group_ids(&pool, owner_id).await.unwrap().is_empty());
        crate::follow::create(&pool, owner_id, group_account.id, None)
            .await
            .unwrap();
        assert_eq!(
            joined_group_ids(&pool, owner_id).await.unwrap(),
            vec![group_account.id]
        );
    }

    #[sqlx::test]
    async fn thread_lock_lifecycle(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        let post = crate::status::create_local(
            &pool,
            crate::status::NewLocalStatus::new(owner_id, "<p>topic</p>", "public", None),
        )
        .await
        .unwrap();

        assert!(
            !thread_locked(&pool, group_account.id, post.id)
                .await
                .unwrap()
        );
        assert!(locked_of(&pool, &[post.id]).await.unwrap().is_empty());

        lock_thread(&pool, group_account.id, post.id).await.unwrap();
        // Idempotent.
        lock_thread(&pool, group_account.id, post.id).await.unwrap();
        assert!(
            thread_locked(&pool, group_account.id, post.id)
                .await
                .unwrap()
        );
        assert_eq!(locked_of(&pool, &[post.id]).await.unwrap(), vec![post.id]);
        // The lock is per group.
        assert!(!thread_locked(&pool, owner_id, post.id).await.unwrap());

        assert!(
            unlock_thread(&pool, group_account.id, post.id)
                .await
                .unwrap()
        );
        assert!(
            !unlock_thread(&pool, group_account.id, post.id)
                .await
                .unwrap()
        );
        assert!(
            !thread_locked(&pool, group_account.id, post.id)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn members_and_bans_listing(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        let carl = account::create_local(
            &pool,
            NewLocalAccount {
                username: "carl",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        // A pending follow isn't a member; an accepted one is.
        crate::follow::create_request(&pool, carl.id, group_account.id, None)
            .await
            .unwrap();
        assert_eq!(member_count(&pool, group_account.id).await.unwrap(), 0);
        crate::follow::create(&pool, carl.id, group_account.id, None)
            .await
            .unwrap();
        assert_eq!(member_count(&pool, group_account.id).await.unwrap(), 1);
        assert_eq!(
            members(&pool, group_account.id, None, 10).await.unwrap(),
            vec![carl.id]
        );
        assert!(outcasts(&pool, group_account.id).await.unwrap().is_empty());

        set_affiliation(&pool, group_account.id, carl.id, Affiliation::Outcast, None)
            .await
            .unwrap();
        let bans = outcasts(&pool, group_account.id).await.unwrap();
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].account_id, carl.id);
        assert!(bans[0].expires_at.is_none());
    }

    #[sqlx::test]
    async fn settings_setters(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, group) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        assert_eq!(group.posting_policy(), PostingPolicy::Members);
        assert!(!group.posting_restricted_to_mods());
        assert!(!group.sensitive);

        set_posting_policy(&pool, group_account.id, PostingPolicy::Mods)
            .await
            .unwrap();
        set_sensitive(&pool, group_account.id, true).await.unwrap();
        set_membership_policy(&pool, group_account.id, MembershipPolicy::Approval)
            .await
            .unwrap();
        let updated = find(&pool, group_account.id).await.unwrap().unwrap();
        assert_eq!(updated.posting_policy(), PostingPolicy::Mods);
        assert!(updated.posting_restricted_to_mods());
        assert!(updated.sensitive);
        assert_eq!(updated.membership_policy(), MembershipPolicy::Approval);
    }

    #[sqlx::test]
    async fn owner_lookup_returns_the_owner_row(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (group_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        assert_eq!(
            super::owner(&pool, group_account.id).await.unwrap(),
            Some(owner_id)
        );
        // A non-group account has no owner row.
        assert_eq!(super::owner(&pool, owner_id).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn admin_list_searches_flags_state_and_counts_members(pool: PgPool) {
        let owner_id = owner(&pool).await;
        let (hiking_account, _) = hiking(&pool, owner_id, MembershipPolicy::Open).await;
        // A second group so search actually narrows the set.
        let (cooking_account, _) = create(
            &pool,
            NewLocalGroup {
                account: NewLocalAccount {
                    username: "cooking",
                    display_name: "Cooking",
                    note: "",
                    public_key_pem: "pub",
                },
                membership_policy: MembershipPolicy::Approval,
                posting_policy: PostingPolicy::Members,
                created_by: owner_id,
            },
        )
        .await
        .unwrap();
        // A member of the hiking group (the owner's own auto-join is not seeded
        // by the db `create`, so add one explicitly).
        crate::follow::create(&pool, owner_id, hiking_account.id, None)
            .await
            .unwrap();
        // Suspend cooking: it stays listed here (unlike the public directory).
        crate::account::suspend(&pool, cooking_account.id, "local")
            .await
            .unwrap();

        let all = admin_list(&pool, None, None, 50).await.unwrap();
        assert_eq!(all.len(), 2, "both groups, including the suspended one");

        let hiking_row = all.iter().find(|r| r.username == "hiking").unwrap();
        assert_eq!(hiking_row.member_count, 1);
        assert!(!hiking_row.suspended);
        assert!(!hiking_row.deleted);

        let cooking_row = all.iter().find(|r| r.username == "cooking").unwrap();
        assert!(cooking_row.suspended);
        assert_eq!(cooking_row.membership_policy, "approval");

        // Case-insensitive search over username / display name.
        let hits = admin_list(&pool, Some("HIK"), None, 50).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].username, "hiking");

        // Keyset pagination on the account id (newest first).
        let first = admin_list(&pool, None, None, 1).await.unwrap();
        assert_eq!(first.len(), 1);
        let next = admin_list(&pool, None, Some(first[0].account_id), 50)
            .await
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_ne!(next[0].account_id, first[0].account_id);
    }
}
