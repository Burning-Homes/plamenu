//! Admin account moderation views (Mastodon's `Admin::AccountFilter` +
//! `REST::Admin::AccountSerializer`). An [`AdminAccountView`] pairs the public
//! [`Account`] with the moderation overlay an admin sees: the local user's
//! email/confirmation/approval/disabled state and assigned role.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;
use crate::account::{self, Account};
use crate::role::Role;

/// An account as seen through the admin moderation lens. The overlay fields are
/// `None`/`false` for remote accounts (which have no local `users` row).
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Mastodon's independent admin account flags are a compatibility surface"
)]
pub struct AdminAccountView {
    pub account: Account,
    /// A remotely-owned identity registered as an account on this instance.
    pub portable: bool,
    /// Whether a local `users` row backs this account.
    pub has_user: bool,
    pub email: Option<String>,
    pub confirmed_at: Option<OffsetDateTime>,
    pub approved: bool,
    pub disabled: bool,
    pub locale: Option<String>,
    /// The address observed on the original registration request. This is
    /// distinct from [`Self::ips`], which also contains later successful
    /// login addresses and may outlive the signup address's retention window.
    pub sign_up_ip: Option<String>,
    pub invite_request_text: Option<String>,
    /// The OAuth application that submitted the registration, when that app
    /// still exists.
    pub sign_up_application: Option<String>,
    pub sign_up_invite: Option<AdminSignupInvite>,
    pub confirmation_sent_at: Option<OffsetDateTime>,
    pub time_zone: Option<String>,
    /// Proof that the configured minimum-age gate passed. The submitted birth
    /// date itself is deliberately never stored.
    pub age_verified_at: Option<OffsetDateTime>,
    pub ips: Vec<AdminAccountIp>,
    pub role: Option<Role>,
}

/// The invite attached to a registration, with enough attribution for the
/// account moderation page to link back to the inviter.
#[derive(Debug, Clone)]
pub struct AdminSignupInvite {
    pub code: String,
    pub inviter_account_id: Option<i64>,
    pub inviter_username: Option<String>,
}

/// A distinct IP address used by a local user, with its latest observed use.
/// This mirrors Mastodon's `user_ips` database view.
#[derive(Debug, Clone)]
pub struct AdminAccountIp {
    pub ip: String,
    pub used_at: OffsetDateTime,
}

/// Filters for [`list`] — Mastodon's `Admin::AccountFilter` keys, already
/// normalized (the v1/v2 controllers translate their param spellings into
/// these). `username`/`display_name`/`email` are `ILIKE` patterns supplied by
/// the caller (Mastodon matches case-insensitively).
#[derive(Debug, Default)]
pub struct AdminAccountFilter {
    /// `"local"` or `"remote"`; `None` for either.
    pub origin: Option<String>,
    /// `active`/`pending`/`disabled`/`silenced`/`suspended`/`sensitized`.
    pub status: Option<String>,
    pub by_domain: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    /// Restrict to accounts whose user holds one of these roles; empty = any.
    pub role_ids: Vec<i64>,
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: i64,
}

/// Lists accounts matching `filter`, newest id first (oldest first when
/// `min_id` is set, like Mastodon's keyset pagination). Overlay data is loaded
/// alongside so the serializer needs no further queries.
pub async fn list(
    pool: &PgPool,
    filter: &AdminAccountFilter,
) -> Result<Vec<AdminAccountView>, DbError> {
    // Mastodon paginates by account id; `min_id` walks forward (ascending),
    // every other cursor walks back (descending).
    let ascending = filter.min_id.is_some();
    let ids: Vec<i64> = sqlx::query_scalar!(
        r#"
        SELECT a.id
        FROM accounts a
        LEFT JOIN users u ON u.account_id = a.id
        WHERE NOT a.is_internal
          AND ($1::text IS NULL
                OR ($1 = 'local'  AND (a.domain IS NULL OR a.portable))
                OR ($1 = 'remote' AND a.domain IS NOT NULL AND NOT a.portable))
          AND ($2::text IS NULL
                OR ($2 = 'active'     AND a.suspended_at IS NULL)
                OR ($2 = 'pending'    AND u.approved = false)
                OR ($2 = 'disabled'   AND u.disabled = true)
                OR ($2 = 'silenced'   AND a.silenced_at IS NOT NULL)
                OR ($2 = 'suspended'  AND a.suspended_at IS NOT NULL)
                OR ($2 = 'sensitized' AND a.sensitized_at IS NOT NULL))
          AND ($3::text IS NULL OR lower(a.domain) = lower($3))
          AND ($4::text IS NULL OR a.username ILIKE $4)
          AND ($5::text IS NULL OR a.display_name ILIKE $5)
          AND ($6::text IS NULL OR u.email ILIKE $6)
          AND (cardinality($7::bigint[]) = 0 OR u.role_id = ANY($7))
          AND ($8::bigint IS NULL OR a.id < $8)
          AND ($9::bigint IS NULL OR a.id > $9)
          AND ($10::bigint IS NULL OR a.id > $10)
        ORDER BY a.id * (CASE WHEN $11 THEN 1 ELSE -1 END)
        LIMIT $12
        "#,
        filter.origin,
        filter.status,
        filter.by_domain,
        filter.username,
        filter.display_name,
        filter.email,
        &filter.role_ids,
        filter.max_id,
        filter.since_id,
        filter.min_id,
        ascending,
        filter.limit,
    )
    .fetch_all(pool)
    .await?;

    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // `find_by_ids` returns rows in arbitrary order; reassemble in cursor order.
    let mut accounts = account::find_by_ids(pool, &ids).await?;
    let mut overlays = overlay_for(pool, &ids).await?;
    let mut views = Vec::with_capacity(ids.len());
    for account_id in &ids {
        let Some(pos) = accounts.iter().position(|a| a.id == *account_id) else {
            continue;
        };
        let account = accounts.swap_remove(pos);
        let overlay = overlays.remove(account_id).unwrap_or_default();
        views.push(overlay.into_view(account));
    }
    Ok(views)
}

/// A single account's admin view, or `None` when the id is unknown.
pub async fn show(pool: &PgPool, account_id: i64) -> Result<Option<AdminAccountView>, DbError> {
    let Some(account) = account::find_by_id(pool, account_id).await? else {
        return Ok(None);
    };
    let overlay = overlay_for(pool, &[account_id])
        .await?
        .remove(&account_id)
        .unwrap_or_default();
    Ok(Some(overlay.into_view(account)))
}

/// The per-account overlay (user + role columns), before being paired with the
/// public account.
#[derive(Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "query overlay mirrors independent persisted account and user flags"
)]
struct Overlay {
    portable: bool,
    has_user: bool,
    email: Option<String>,
    confirmed_at: Option<OffsetDateTime>,
    approved: bool,
    disabled: bool,
    locale: Option<String>,
    sign_up_ip: Option<String>,
    invite_request_text: Option<String>,
    sign_up_application: Option<String>,
    sign_up_invite: Option<AdminSignupInvite>,
    confirmation_sent_at: Option<OffsetDateTime>,
    time_zone: Option<String>,
    age_verified_at: Option<OffsetDateTime>,
    ips: Vec<AdminAccountIp>,
    role: Option<Role>,
}

impl Overlay {
    fn into_view(self, account: Account) -> AdminAccountView {
        AdminAccountView {
            account,
            portable: self.portable,
            has_user: self.has_user,
            email: self.email,
            confirmed_at: self.confirmed_at,
            approved: self.approved,
            disabled: self.disabled,
            locale: self.locale,
            sign_up_ip: self.sign_up_ip,
            invite_request_text: self.invite_request_text,
            sign_up_application: self.sign_up_application,
            sign_up_invite: self.sign_up_invite,
            confirmation_sent_at: self.confirmation_sent_at,
            time_zone: self.time_zone,
            age_verified_at: self.age_verified_at,
            ips: self.ips,
            role: self.role,
        }
    }
}

/// Batch-loads the user/role overlay for a set of account ids.
async fn overlay_for(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Overlay>, DbError> {
    let mut ips = ips_for(pool, account_ids).await?;
    let rows = sqlx::query!(
        r#"
        SELECT a.id AS "account_id!",
               a.portable     AS "portable!",
               u.id            AS "user_id?",
               u.email         AS "email?",
               u.confirmed_at  AS "confirmed_at?",
               u.approved      AS "approved?",
               u.disabled      AS "disabled?",
               u.locale        AS "locale?",
               u.sign_up_ip AS "sign_up_ip?", u.invite_request_text AS "invite_request_text?",
               signup_app.name AS "sign_up_application?", signup_invite.code AS "invite_code?",
               inviter_account.id AS "inviter_account_id?", inviter_account.username AS "inviter_username?",
               u.confirmation_sent_at AS "confirmation_sent_at?", u.time_zone AS "time_zone?", u.age_verified_at AS "age_verified_at?",
               r.id            AS "role_id?",
               r.name          AS "role_name?",
               r.color         AS "role_color?",
               r.position      AS "role_position?",
               r.permissions   AS "role_permissions?",
               r.highlighted   AS "role_highlighted?",
               r.created_at    AS "role_created_at?",
               r.updated_at    AS "role_updated_at?"
        FROM accounts a
        LEFT JOIN users u ON u.account_id = a.id
        LEFT JOIN user_roles r ON r.id = u.role_id
        LEFT JOIN oauth_apps signup_app ON signup_app.id = u.created_by_application_id
        LEFT JOIN invites signup_invite ON signup_invite.id = u.invite_id
        LEFT JOIN users inviter_user ON inviter_user.id = signup_invite.user_id
        LEFT JOIN accounts inviter_account ON inviter_account.id = inviter_user.account_id
        WHERE a.id = ANY($1)
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;

    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let sign_up_invite = signup_invite(
            row.invite_code,
            row.inviter_account_id,
            row.inviter_username,
        );
        let role = match (
            row.role_id,
            row.role_name,
            row.role_color,
            row.role_position,
            row.role_permissions,
            row.role_highlighted,
            row.role_created_at,
            row.role_updated_at,
        ) {
            (
                Some(id),
                Some(name),
                Some(color),
                Some(position),
                Some(permissions),
                Some(highlighted),
                Some(created_at),
                Some(updated_at),
            ) => Some(Role {
                id,
                name,
                color,
                position,
                permissions,
                highlighted,
                created_at,
                updated_at,
            }),
            _ => None,
        };
        map.insert(
            row.account_id,
            Overlay {
                portable: row.portable,
                has_user: row.user_id.is_some(),
                email: row.email,
                confirmed_at: row.confirmed_at,
                approved: row.approved.unwrap_or(false),
                disabled: row.disabled.unwrap_or(false),
                locale: row.locale,
                sign_up_ip: row.sign_up_ip,
                invite_request_text: row.invite_request_text,
                sign_up_application: row.sign_up_application,
                sign_up_invite,
                confirmation_sent_at: row.confirmation_sent_at,
                time_zone: row.time_zone,
                age_verified_at: row.age_verified_at,
                ips: ips.remove(&row.account_id).unwrap_or_default(),
                role,
            },
        );
    }
    Ok(map)
}

fn signup_invite(
    code: Option<String>,
    inviter_account_id: Option<i64>,
    inviter_username: Option<String>,
) -> Option<AdminSignupInvite> {
    code.map(|code| AdminSignupInvite {
        code,
        inviter_account_id,
        inviter_username,
    })
}

async fn ips_for(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<AdminAccountIp>>, DbError> {
    let rows = sqlx::query!(
        r#"
        WITH user_ips AS (
            SELECT u.account_id,
                   u.sign_up_ip AS ip,
                   u.created_at AS used_at
            FROM users u
            WHERE u.account_id = ANY($1)
              AND u.sign_up_ip IS NOT NULL
            UNION ALL
            SELECT u.account_id,
                   la.ip,
                   la.created_at AS used_at
            FROM users u
            JOIN login_activities la ON la.user_id = u.id
            WHERE u.account_id = ANY($1)
              AND la.success
              AND la.ip IS NOT NULL
        ),
        deduped AS (
            SELECT account_id, ip, max(used_at) AS used_at
            FROM user_ips
            GROUP BY account_id, ip
        )
        SELECT account_id AS "account_id!",
               ip AS "ip!",
               used_at AS "used_at!"
        FROM deduped
        ORDER BY account_id, used_at DESC
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;

    let mut map: std::collections::HashMap<i64, Vec<AdminAccountIp>> =
        std::collections::HashMap::new();
    for row in rows {
        map.entry(row.account_id).or_default().push(AdminAccountIp {
            ip: row.ip,
            used_at: row.used_at,
        });
    }
    Ok(map)
}
