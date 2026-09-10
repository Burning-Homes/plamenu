//! Instance-level policy records for the admin moderation surface:
//! federation domain blocks/allows plus e-mail/IP/canonical-email access
//! blocks. These mirror Mastodon's tables closely at the REST boundary.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone)]
pub struct Page {
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: i64,
}

impl Page {
    #[must_use]
    fn ascending(&self) -> bool {
        self.min_id.is_some()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DomainBlock {
    pub id: i64,
    pub domain: String,
    pub severity: String,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub struct NewDomainBlock<'a> {
    pub domain: &'a str,
    pub severity: &'a str,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: Option<&'a str>,
    pub public_comment: Option<&'a str>,
    pub obfuscate: bool,
}

#[derive(Debug, Default)]
pub struct DomainBlockUpdate<'a> {
    pub severity: Option<&'a str>,
    pub reject_media: Option<bool>,
    pub reject_reports: Option<bool>,
    pub private_comment: Option<&'a str>,
    pub public_comment: Option<&'a str>,
    pub obfuscate: Option<bool>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DomainAllow {
    pub id: i64,
    pub domain: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EmailDomainBlock {
    pub id: i64,
    pub domain: String,
    pub parent_id: Option<i64>,
    pub allow_with_approval: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IpBlock {
    pub id: i64,
    pub ip: String,
    pub severity: String,
    pub comment: String,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Default)]
pub struct IpBlockUpdate<'a> {
    pub ip: Option<&'a str>,
    pub severity: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub update_expires_at: bool,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CanonicalEmailBlock {
    pub id: i64,
    pub canonical_email_hash: String,
    pub reference_account_id: Option<i64>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One row of the known-instances directory: a remote domain we have seen
/// accounts from, with its account count and any federation-policy verdict.
#[derive(Debug, Clone)]
pub struct KnownInstance {
    pub domain: String,
    pub accounts_count: i64,
    pub published: OffsetDateTime,
    /// The domain block's severity, when one exists (`silence`/`suspend`/`noop`).
    pub block_severity: Option<String>,
    pub allowed: bool,
}

/// Filters for [`known_instances`] — the admin directory's search and policy
/// narrowing, plus its keyset cursor.
#[derive(Debug, Default)]
pub struct KnownInstanceFilter<'a> {
    /// Keyset cursor: domains strictly after this one, alphabetically.
    pub after_domain: Option<&'a str>,
    /// An `ILIKE` pattern the caller has already wrapped/escaped
    /// (`%query%`).
    pub domain_query: Option<&'a str>,
    /// `suspended` / `limited` / `blocked` (any block) / `allowed` /
    /// `none` (no block and no allow); `None` for every instance.
    pub policy: Option<&'a str>,
    pub limit: i64,
}

/// The remote domains known to this server (every domain an account was seen
/// from), alphabetical, keyset-paginated by domain — the data behind
/// Mastodon's `admin/instances` directory.
pub async fn known_instances(
    pool: &PgPool,
    filter: &KnownInstanceFilter<'_>,
) -> Result<Vec<KnownInstance>, DbError> {
    let rows = sqlx::query_as!(
        KnownInstance,
        r#"
        SELECT a.domain AS "domain!", COUNT(*) AS "accounts_count!",
               MIN(a.created_at) AS "published!",
               db.severity AS "block_severity?",
               (da.id IS NOT NULL) AS "allowed!"
        FROM accounts a
        LEFT JOIN domain_blocks db ON db.domain = a.domain
        LEFT JOIN domain_allows da ON da.domain = a.domain
        WHERE a.domain IS NOT NULL AND NOT a.portable
          AND ($1::text IS NULL OR a.domain > $1)
          AND ($2::text IS NULL OR a.domain ILIKE $2)
          AND ($3::text IS NULL
                OR ($3 = 'suspended' AND db.severity = 'suspend')
                OR ($3 = 'limited'   AND db.severity = 'silence')
                OR ($3 = 'blocked'   AND db.id IS NOT NULL)
                OR ($3 = 'allowed'   AND da.id IS NOT NULL)
                OR ($3 = 'none'      AND db.id IS NULL AND da.id IS NULL))
        GROUP BY a.domain, db.severity, da.id
        ORDER BY a.domain
        LIMIT $4
        "#,
        filter.after_domain,
        filter.domain_query,
        filter.policy,
        filter.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The per-domain figures behind the admin instance detail page. All counts
/// are computed live — this renders once per admin page view, not in any hot
/// path.
#[derive(Debug, Clone)]
pub struct InstanceStats {
    /// Accounts we know from the domain.
    pub accounts_count: i64,
    /// Statuses stored from those accounts.
    pub statuses_count: i64,
    /// Accepted follows from local users toward the domain's accounts.
    pub follows_out: i64,
    /// Accepted follows from the domain's accounts toward local users.
    pub follows_in: i64,
    /// Reports filed against the domain's accounts.
    pub reports_count: i64,
    /// When we last stored a status from the domain, if ever.
    pub last_status_at: Option<OffsetDateTime>,
}

/// Aggregates [`InstanceStats`] for one remote domain.
pub async fn instance_stats(pool: &PgPool, domain: &str) -> Result<InstanceStats, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT count(*) FROM accounts WHERE domain = $1 AND NOT portable)
                AS "accounts_count!",
            (SELECT count(*) FROM statuses s -- STUBKEEP: operator aggregate; stubs are rows until pruned
              JOIN accounts a ON a.id = s.account_id
              WHERE a.domain = $1 AND NOT a.portable) AS "statuses_count!",
            (SELECT count(*) FROM follows f
              JOIN accounts t ON t.id = f.target_account_id
              JOIN accounts src ON src.id = f.account_id
              WHERE t.domain = $1 AND NOT t.portable
                AND (src.domain IS NULL OR src.portable) AND NOT f.pending)
                AS "follows_out!",
            (SELECT count(*) FROM follows f
              JOIN accounts src ON src.id = f.account_id
              JOIN accounts t ON t.id = f.target_account_id
              WHERE src.domain = $1 AND NOT src.portable
                AND (t.domain IS NULL OR t.portable) AND NOT f.pending)
                AS "follows_in!",
            (SELECT count(*) FROM reports r
              JOIN accounts t ON t.id = r.target_account_id
              WHERE t.domain = $1 AND NOT t.portable) AS "reports_count!",
            (SELECT max(s.created_at) FROM statuses s
              JOIN accounts a ON a.id = s.account_id
              WHERE a.domain = $1 AND NOT a.portable) AS "last_status_at?"
        "#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(InstanceStats {
        accounts_count: row.accounts_count,
        statuses_count: row.statuses_count,
        follows_out: row.follows_out,
        follows_in: row.follows_in,
        reports_count: row.reports_count,
        last_status_at: row.last_status_at,
    })
}

/// The policy rows attached to one domain: its block and/or allow, if any.
pub async fn domain_policy(
    pool: &PgPool,
    domain: &str,
) -> Result<(Option<DomainBlock>, Option<DomainAllow>), DbError> {
    let block = find_domain_block_by_domain(pool, domain).await?;
    let allow = sqlx::query_as!(
        DomainAllow,
        r#"
        SELECT id, domain, created_at, updated_at
        FROM domain_allows
        WHERE lower(domain) = lower($1)
        "#,
        domain,
    )
    .fetch_optional(pool)
    .await?;
    Ok((block, allow))
}

pub async fn list_domain_blocks(pool: &PgPool, page: &Page) -> Result<Vec<DomainBlock>, DbError> {
    let rows = sqlx::query_as!(
        DomainBlock,
        r#"
        SELECT id, domain, severity, reject_media, reject_reports,
               private_comment, public_comment, obfuscate, created_at,
               updated_at
        FROM domain_blocks
        WHERE ($1::bigint IS NULL OR id < $1)
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id * (CASE WHEN $4 THEN 1 ELSE -1 END)
        LIMIT $5
        "#,
        page.max_id,
        page.since_id,
        page.min_id,
        page.ascending(),
        page.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_domain_block(
    pool: &PgPool,
    block_id: i64,
) -> Result<Option<DomainBlock>, DbError> {
    let row = sqlx::query_as!(
        DomainBlock,
        r#"
        SELECT id, domain, severity, reject_media, reject_reports,
               private_comment, public_comment, obfuscate, created_at,
               updated_at
        FROM domain_blocks
        WHERE id = $1
        "#,
        block_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_domain_block_by_domain(
    pool: &PgPool,
    domain: &str,
) -> Result<Option<DomainBlock>, DbError> {
    let row = sqlx::query_as!(
        DomainBlock,
        r#"
        SELECT id, domain, severity, reject_media, reject_reports,
               private_comment, public_comment, obfuscate, created_at,
               updated_at
        FROM domain_blocks
        WHERE domain = $1
        "#,
        domain,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn domain_allows_federation(pool: &PgPool, domain: &str) -> Result<bool, DbError> {
    let allowed = sqlx::query_scalar!(
        r#"
        SELECT instance_domain_allowed($1) AS "allowed!"
        "#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(allowed)
}

pub async fn account_domain_allows_federation(
    pool: &PgPool,
    account_id: i64,
) -> Result<bool, DbError> {
    let allowed = sqlx::query_scalar!(
        r#"
        SELECT (suspended_at IS NULL
                AND (portable OR COALESCE(instance_domain_allowed(domain), false))) AS "allowed!"
        FROM accounts
        WHERE id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(allowed.unwrap_or(false))
}

/// Whether `domain` has a block with `reject_media`: its media is never
/// downloaded or cached, only referenced at its origin.
pub async fn domain_rejects_media(pool: &PgPool, domain: &str) -> Result<bool, DbError> {
    let rejects = sqlx::query_scalar!(
        r#"
        SELECT instance_domain_rejects_media($1) AS "rejects!"
        "#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(rejects)
}

/// [`domain_rejects_media`] for a stored account, by its domain column (a
/// local account never rejects).
pub async fn account_domain_rejects_media(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let rejects = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(instance_domain_rejects_media(domain), false) AS "rejects!"
        FROM accounts
        WHERE id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(rejects.unwrap_or(false))
}

/// Whether `domain` carries a `silence` domain block — its accounts are
/// limited (hidden from the shared discovery feeds and from anonymous profile
/// views) just like a per-account silence.
pub async fn is_domain_silenced(pool: &PgPool, domain: &str) -> Result<bool, DbError> {
    let silenced = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM domain_blocks
            WHERE domain = $1 AND severity = 'silence'
        ) AS "silenced!"
        "#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(silenced)
}

/// Every domain under a `silence` block, for batched rendering (the account
/// list serializer prefetches this once instead of querying per row).
pub async fn silenced_domains(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let domains =
        sqlx::query_scalar!("SELECT domain FROM domain_blocks WHERE severity = 'silence'",)
            .fetch_all(pool)
            .await?;
    Ok(domains)
}

/// Whether `domain` has a block with `reject_reports`: its inbound `Flag`
/// activities are dropped without filing a report.
pub async fn domain_rejects_reports(pool: &PgPool, domain: &str) -> Result<bool, DbError> {
    let rejects = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM domain_blocks
            WHERE domain = $1 AND reject_reports
        ) AS "rejects!"
        "#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(rejects)
}

pub async fn create_domain_block(
    pool: &PgPool,
    new: NewDomainBlock<'_>,
) -> Result<DomainBlock, DbError> {
    let row = sqlx::query_as!(
        DomainBlock,
        r#"
        INSERT INTO domain_blocks
            (id, domain, severity, reject_media, reject_reports,
             private_comment, public_comment, obfuscate)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id, domain, severity, reject_media, reject_reports,
                  private_comment, public_comment, obfuscate, created_at,
                  updated_at
        "#,
        id::next(),
        new.domain,
        new.severity,
        new.reject_media,
        new.reject_reports,
        new.private_comment,
        new.public_comment,
        new.obfuscate,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update_domain_block(
    pool: &PgPool,
    block_id: i64,
    update: DomainBlockUpdate<'_>,
) -> Result<Option<DomainBlock>, DbError> {
    let row = sqlx::query_as!(
        DomainBlock,
        r#"
        UPDATE domain_blocks
        SET severity = COALESCE($2, severity),
            reject_media = COALESCE($3, reject_media),
            reject_reports = COALESCE($4, reject_reports),
            private_comment = COALESCE($5, private_comment),
            public_comment = COALESCE($6, public_comment),
            obfuscate = COALESCE($7, obfuscate),
            updated_at = now()
        WHERE id = $1
        RETURNING id, domain, severity, reject_media, reject_reports,
                  private_comment, public_comment, obfuscate, created_at,
                  updated_at
        "#,
        block_id,
        update.severity,
        update.reject_media,
        update.reject_reports,
        update.private_comment,
        update.public_comment,
        update.obfuscate,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete_domain_block(pool: &PgPool, block_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM domain_blocks WHERE id = $1", block_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_domain_allows(pool: &PgPool, page: &Page) -> Result<Vec<DomainAllow>, DbError> {
    let rows = sqlx::query_as!(
        DomainAllow,
        r#"
        SELECT id, domain, created_at, updated_at
        FROM domain_allows
        WHERE ($1::bigint IS NULL OR id < $1)
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id * (CASE WHEN $4 THEN 1 ELSE -1 END)
        LIMIT $5
        "#,
        page.max_id,
        page.since_id,
        page.min_id,
        page.ascending(),
        page.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_domain_allow(
    pool: &PgPool,
    allow_id: i64,
) -> Result<Option<DomainAllow>, DbError> {
    let row = sqlx::query_as!(
        DomainAllow,
        r#"
        SELECT id, domain, created_at, updated_at
        FROM domain_allows
        WHERE id = $1
        "#,
        allow_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_domain_allow(pool: &PgPool, domain: &str) -> Result<DomainAllow, DbError> {
    let row = sqlx::query_as!(
        DomainAllow,
        r#"
        INSERT INTO domain_allows (id, domain)
        VALUES ($1, $2)
        ON CONFLICT (domain) DO UPDATE SET domain = EXCLUDED.domain
        RETURNING id, domain, created_at, updated_at
        "#,
        id::next(),
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn delete_domain_allow(pool: &PgPool, allow_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM domain_allows WHERE id = $1", allow_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_email_domain_blocks(
    pool: &PgPool,
    page: &Page,
) -> Result<Vec<EmailDomainBlock>, DbError> {
    let rows = sqlx::query_as!(
        EmailDomainBlock,
        r#"
        SELECT id, domain, parent_id, allow_with_approval, created_at, updated_at
        FROM email_domain_blocks
        WHERE ($1::bigint IS NULL OR id < $1)
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id * (CASE WHEN $4 THEN 1 ELSE -1 END)
        LIMIT $5
        "#,
        page.max_id,
        page.since_id,
        page.min_id,
        page.ascending(),
        page.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_email_domain_block(
    pool: &PgPool,
    block_id: i64,
) -> Result<Option<EmailDomainBlock>, DbError> {
    let row = sqlx::query_as!(
        EmailDomainBlock,
        r#"
        SELECT id, domain, parent_id, allow_with_approval, created_at, updated_at
        FROM email_domain_blocks
        WHERE id = $1
        "#,
        block_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_email_domain_block_by_domain(
    pool: &PgPool,
    domain: &str,
) -> Result<Option<EmailDomainBlock>, DbError> {
    let row = sqlx::query_as!(
        EmailDomainBlock,
        r#"
        SELECT id, domain, parent_id, allow_with_approval, created_at, updated_at
        FROM email_domain_blocks
        WHERE domain = $1
        "#,
        domain,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_email_domain_block(
    pool: &PgPool,
    domain: &str,
    allow_with_approval: bool,
) -> Result<EmailDomainBlock, DbError> {
    let row = sqlx::query_as!(
        EmailDomainBlock,
        r#"
        INSERT INTO email_domain_blocks (id, domain, allow_with_approval)
        VALUES ($1, $2, $3)
        RETURNING id, domain, parent_id, allow_with_approval, created_at,
                  updated_at
        "#,
        id::next(),
        domain,
        allow_with_approval,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn delete_email_domain_block(pool: &PgPool, block_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM email_domain_blocks WHERE id = $1", block_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_ip_blocks(pool: &PgPool, page: &Page) -> Result<Vec<IpBlock>, DbError> {
    let rows = sqlx::query_as!(
        IpBlock,
        r#"
        SELECT id, ip, severity, comment, expires_at, created_at, updated_at
        FROM ip_blocks
        WHERE ($1::bigint IS NULL OR id < $1)
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id * (CASE WHEN $4 THEN 1 ELSE -1 END)
        LIMIT $5
        "#,
        page.max_id,
        page.since_id,
        page.min_id,
        page.ascending(),
        page.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_ip_block(pool: &PgPool, block_id: i64) -> Result<Option<IpBlock>, DbError> {
    let row = sqlx::query_as!(
        IpBlock,
        r#"
        SELECT id, ip, severity, comment, expires_at, created_at, updated_at
        FROM ip_blocks
        WHERE id = $1
        "#,
        block_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_ip_block_by_ip(pool: &PgPool, ip: &str) -> Result<Option<IpBlock>, DbError> {
    let row = sqlx::query_as!(
        IpBlock,
        r#"
        SELECT id, ip, severity, comment, expires_at, created_at, updated_at
        FROM ip_blocks
        WHERE ip = $1
        "#,
        ip,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn active_ip_blocks(pool: &PgPool) -> Result<Vec<IpBlock>, DbError> {
    let rows = sqlx::query_as!(
        IpBlock,
        r#"
        SELECT id, ip, severity, comment, expires_at, created_at, updated_at
        FROM ip_blocks
        WHERE expires_at IS NULL OR expires_at > now()
        ORDER BY id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn create_ip_block(
    pool: &PgPool,
    ip: &str,
    severity: &str,
    comment: &str,
    expires_at: Option<OffsetDateTime>,
) -> Result<IpBlock, DbError> {
    let row = sqlx::query_as!(
        IpBlock,
        r#"
        INSERT INTO ip_blocks (id, ip, severity, comment, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, ip, severity, comment, expires_at, created_at, updated_at
        "#,
        id::next(),
        ip,
        severity,
        comment,
        expires_at,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update_ip_block(
    pool: &PgPool,
    block_id: i64,
    update: IpBlockUpdate<'_>,
) -> Result<Option<IpBlock>, DbError> {
    let row = sqlx::query_as!(
        IpBlock,
        r#"
        UPDATE ip_blocks
        SET ip = COALESCE($2, ip),
            severity = COALESCE($3, severity),
            comment = COALESCE($4, comment),
            expires_at = CASE WHEN $5 THEN $6 ELSE expires_at END,
            updated_at = now()
        WHERE id = $1
        RETURNING id, ip, severity, comment, expires_at, created_at, updated_at
        "#,
        block_id,
        update.ip,
        update.severity,
        update.comment,
        update.update_expires_at,
        update.expires_at,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete_ip_block(pool: &PgPool, block_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM ip_blocks WHERE id = $1", block_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_canonical_email_blocks(
    pool: &PgPool,
    page: &Page,
) -> Result<Vec<CanonicalEmailBlock>, DbError> {
    let rows = sqlx::query_as!(
        CanonicalEmailBlock,
        r#"
        SELECT id, canonical_email_hash, reference_account_id, created_at, updated_at
        FROM canonical_email_blocks
        WHERE ($1::bigint IS NULL OR id < $1)
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id * (CASE WHEN $4 THEN 1 ELSE -1 END)
        LIMIT $5
        "#,
        page.max_id,
        page.since_id,
        page.min_id,
        page.ascending(),
        page.limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_canonical_email_block(
    pool: &PgPool,
    block_id: i64,
) -> Result<Option<CanonicalEmailBlock>, DbError> {
    let row = sqlx::query_as!(
        CanonicalEmailBlock,
        r#"
        SELECT id, canonical_email_hash, reference_account_id, created_at, updated_at
        FROM canonical_email_blocks
        WHERE id = $1
        "#,
        block_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_canonical_email_block_by_hash(
    pool: &PgPool,
    canonical_email_hash: &str,
) -> Result<Option<CanonicalEmailBlock>, DbError> {
    let row = sqlx::query_as!(
        CanonicalEmailBlock,
        r#"
        SELECT id, canonical_email_hash, reference_account_id, created_at, updated_at
        FROM canonical_email_blocks
        WHERE canonical_email_hash = $1
        "#,
        canonical_email_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_canonical_email_block(
    pool: &PgPool,
    canonical_email_hash: &str,
    reference_account_id: Option<i64>,
) -> Result<CanonicalEmailBlock, DbError> {
    let row = sqlx::query_as!(
        CanonicalEmailBlock,
        r#"
        INSERT INTO canonical_email_blocks
            (id, canonical_email_hash, reference_account_id)
        VALUES ($1, $2, $3)
        RETURNING id, canonical_email_hash, reference_account_id, created_at,
                  updated_at
        "#,
        id::next(),
        canonical_email_hash,
        reference_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn matching_canonical_email_blocks(
    pool: &PgPool,
    canonical_email_hash: &str,
) -> Result<Vec<CanonicalEmailBlock>, DbError> {
    let rows = sqlx::query_as!(
        CanonicalEmailBlock,
        r#"
        SELECT id, canonical_email_hash, reference_account_id, created_at, updated_at
        FROM canonical_email_blocks
        WHERE canonical_email_hash = $1
        ORDER BY id DESC
        "#,
        canonical_email_hash,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_canonical_email_block(pool: &PgPool, block_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM canonical_email_blocks WHERE id = $1", block_id,)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn domain_block_crud_and_pagination(pool: PgPool) {
        let first = create_domain_block(
            &pool,
            NewDomainBlock {
                domain: "example.com",
                severity: "silence",
                reject_media: false,
                reject_reports: false,
                private_comment: Some("private"),
                public_comment: Some("public"),
                obfuscate: false,
            },
        )
        .await
        .unwrap();
        let second = create_domain_block(
            &pool,
            NewDomainBlock {
                domain: "bad.example",
                severity: "suspend",
                reject_media: true,
                reject_reports: true,
                private_comment: None,
                public_comment: None,
                obfuscate: true,
            },
        )
        .await
        .unwrap();

        let page = list_domain_blocks(
            &pool,
            &Page {
                max_id: None,
                since_id: None,
                min_id: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(page[0].id, second.id);
        assert_eq!(page[1].id, first.id);

        let updated = update_domain_block(
            &pool,
            first.id,
            DomainBlockUpdate {
                severity: Some("noop"),
                reject_media: Some(true),
                ..DomainBlockUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.severity, "noop");
        assert!(updated.reject_media);

        assert!(delete_domain_block(&pool, first.id).await.unwrap());
        assert!(find_domain_block(&pool, first.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn allow_and_access_blocks_roundtrip(pool: PgPool) {
        let allow = create_domain_allow(&pool, "friend.example").await.unwrap();
        assert_eq!(
            find_domain_allow(&pool, allow.id)
                .await
                .unwrap()
                .unwrap()
                .domain,
            "friend.example"
        );
        assert!(
            domain_allows_federation(&pool, "friend.example")
                .await
                .unwrap()
        );
        assert!(
            !domain_allows_federation(&pool, "stranger.example")
                .await
                .unwrap()
        );

        let email = create_email_domain_block(&pool, "spam.example", true)
            .await
            .unwrap();
        assert!(email.allow_with_approval);

        let ip = create_ip_block(&pool, "192.0.2.0/24", "no_access", "bad net", None)
            .await
            .unwrap();
        assert_eq!(ip.severity, "no_access");
        let expired = create_ip_block(
            &pool,
            "198.51.100.0/24",
            "no_access",
            "old net",
            Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
        )
        .await
        .unwrap();
        let active = active_ip_blocks(&pool).await.unwrap();
        assert!(active.iter().any(|block| block.id == ip.id));
        assert!(!active.iter().any(|block| block.id == expired.id));

        let canonical = create_canonical_email_block(&pool, "abc123", None)
            .await
            .unwrap();
        let matches = matching_canonical_email_blocks(&pool, "abc123")
            .await
            .unwrap();
        assert_eq!(matches[0].id, canonical.id);
    }
}
