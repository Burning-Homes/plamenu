//! Audit-log writing for admin/moderator mutations (Mastodon's `log_action`
//! controller concern). Every state-changing admin verb — web dashboard or
//! admin REST API — appends a line describing who did what to what.
//!
//! The action and target-type vocabulary mirrors Mastodon's
//! (`suspend`/`Account`, `resolve`/`Report`, `create`/`DomainBlock`, …) so a
//! Mastodon admin reads the log without a glossary.

use plamenu_db::account::{self, Account};
use plamenu_db::admin_action_log::{self, NewActionLog};
use plamenu_db::{DbError, PgPool};

/// What an admin action was applied to: the log line's type/id pair plus the
/// display fields snapshotted at write time.
pub struct Target {
    kind: &'static str,
    id: i64,
    human: String,
    permalink: Option<String>,
}

impl Target {
    #[must_use]
    pub fn webxdc(session: &plamenu_db::webxdc::Session) -> Self {
        Self {
            kind: "WebxdcSession",
            id: session.id,
            human: session.name.clone(),
            permalink: Some(format!("/admin/webxdc/{}", session.id)),
        }
    }

    #[must_use]
    pub fn webxdc_app(app: &plamenu_db::webxdc::LibraryApp) -> Self {
        Self {
            kind: "WebxdcApp",
            id: app.id,
            human: app.name.clone(),
            permalink: Some("/admin/webxdc/apps".to_owned()),
        }
    }

    #[must_use]
    pub fn webxdc_catalog(source: &plamenu_db::webxdc::CatalogSource) -> Self {
        Self {
            kind: "WebxdcCatalog",
            id: source.id,
            human: source.name.clone(),
            permalink: Some("/admin/webxdc/apps".to_owned()),
        }
    }

    /// A moderated account (`suspend`, `silence`, `unsuspend`, …).
    #[must_use]
    pub fn account(account: &Account) -> Self {
        Self {
            kind: "Account",
            id: account.id,
            human: handle(account),
            permalink: Some(format!("/admin/accounts/{}", account.id)),
        }
    }

    /// A login-level action on a local user (`enable`, `approve`,
    /// `disable_2fa`, …) — identified by its account handle, like Mastodon's
    /// `User#to_log_human_identifier`.
    #[must_use]
    pub fn user(account: &Account) -> Self {
        Self {
            kind: "User",
            ..Self::account(account)
        }
    }

    /// A strike recorded without a state change (`none` action type).
    #[must_use]
    pub fn warning(id: i64, target: &Account) -> Self {
        Self {
            kind: "AccountWarning",
            id,
            human: handle(target),
            permalink: Some(format!("/admin/accounts/{}", target.id)),
        }
    }

    #[must_use]
    pub fn report(id: i64) -> Self {
        Self {
            kind: "Report",
            id,
            human: format!("#{id}"),
            permalink: Some(format!("/admin/reports/{id}")),
        }
    }

    #[must_use]
    pub fn domain_block(id: i64, domain: &str) -> Self {
        Self::policy("DomainBlock", id, domain)
    }

    #[must_use]
    pub fn domain_allow(id: i64, domain: &str) -> Self {
        Self::policy("DomainAllow", id, domain)
    }

    #[must_use]
    pub fn email_domain_block(id: i64, domain: &str) -> Self {
        Self::policy("EmailDomainBlock", id, domain)
    }

    #[must_use]
    pub fn ip_block(id: i64, ip: &str) -> Self {
        Self::policy("IpBlock", id, ip)
    }

    #[must_use]
    pub fn canonical_email_block(id: i64) -> Self {
        Self::policy("CanonicalEmailBlock", id, "")
    }

    #[must_use]
    pub fn relay(id: i64, inbox_url: &str) -> Self {
        Self {
            kind: "Relay",
            id,
            human: inbox_url.to_owned(),
            permalink: Some("/admin/relays".to_owned()),
        }
    }

    #[must_use]
    pub fn rule(id: i64, text: &str) -> Self {
        Self {
            kind: "Rule",
            id,
            human: text.to_owned(),
            permalink: Some("/admin/rules".to_owned()),
        }
    }

    #[must_use]
    pub fn announcement(id: i64, text: &str) -> Self {
        Self {
            kind: "Announcement",
            id,
            human: text.to_owned(),
            permalink: Some("/admin/announcements".to_owned()),
        }
    }

    #[must_use]
    pub fn custom_emoji(id: i64, shortcode: &str) -> Self {
        Self {
            kind: "CustomEmoji",
            id,
            human: format!(":{shortcode}:"),
            permalink: Some("/admin/custom-emojis".to_owned()),
        }
    }

    #[must_use]
    pub fn webhook(id: i64, url: &str) -> Self {
        let origin = url::Url::parse(url).map_or_else(
            |_| format!("webhook #{id}"),
            |parsed| parsed.origin().ascii_serialization(),
        );
        Self {
            kind: "Webhook",
            id,
            human: origin,
            permalink: Some("/admin/webhooks".to_owned()),
        }
    }

    /// A strike appeal, identified by the appellant's handle like Mastodon's
    /// `Appeal#to_log_human_identifier`.
    #[must_use]
    pub fn appeal(id: i64, appellant: &Account) -> Self {
        Self {
            kind: "Appeal",
            id,
            human: handle(appellant),
            permalink: Some("/admin/appeals".to_owned()),
        }
    }

    #[must_use]
    pub fn role(id: i64, name: &str) -> Self {
        Self {
            kind: "UserRole",
            id,
            human: name.to_owned(),
            permalink: Some(format!("/admin/roles/{id}")),
        }
    }

    /// A local group acted on from the instance groups console
    /// (`suspend`/`delete`/`transfer_owner`/`update`), identified by its
    /// handle. Plamenu extension — Mastodon has no group target type.
    #[must_use]
    pub fn group(account: &Account) -> Self {
        Self {
            kind: "Group",
            id: account.id,
            human: handle(account),
            permalink: Some(format!("/admin/groups/{}", account.id)),
        }
    }

    /// A Lemmy-compatible hard purge of a root post or reply. The status may
    /// be gone by the time the append-only log is read, so the identifier and
    /// permalink are deliberately snapshotted.
    #[must_use]
    pub fn status(id: i64, comment: bool) -> Self {
        Self {
            kind: if comment { "Comment" } else { "Status" },
            id,
            human: format!("#{id}"),
            permalink: Some(format!("/api/v1/statuses/{id}")),
        }
    }

    #[must_use]
    pub fn username_block(id: i64, username: &str) -> Self {
        Self {
            kind: "UsernameBlock",
            id,
            human: username.to_owned(),
            permalink: Some("/admin/username-blocks".to_owned()),
        }
    }

    /// A published Terms of Service version, identified by its effective date
    /// (`None` = effective immediately on publication).
    #[must_use]
    pub fn terms_of_service(id: i64, effective_date: Option<&str>) -> Self {
        Self {
            kind: "TermsOfService",
            id,
            human: effective_date.unwrap_or("on publication").to_owned(),
            permalink: Some("/admin/terms-of-service".to_owned()),
        }
    }

    fn policy(kind: &'static str, id: i64, human: &str) -> Self {
        Self {
            kind,
            id,
            human: human.to_owned(),
            permalink: Some("/admin/instance-policy".to_owned()),
        }
    }
}

/// Appends "`moderator` did `action` to `target`" to the audit log. Call
/// after the mutation succeeds, never before.
pub async fn record(
    pool: &PgPool,
    moderator_account_id: i64,
    action: &str,
    target: &Target,
) -> Result<(), DbError> {
    admin_action_log::record(pool, new_line(moderator_account_id, action, target)).await
}

/// Atomically hard-deletes `account_id` and records the moderator's action, so
/// the admin `destroy`/`reject` delete and its audit line commit or roll back
/// together: an account is never removed without a
/// durable record of who removed it, and no such record survives a rolled-back
/// deletion. Returns whether a row was deleted. Call the plain [`record`] for
/// non-destructive verbs. The acting moderator is never the deleted account
/// (self-destroy is refused before this), so their actor snapshot resolves
/// inside the transaction.
pub async fn record_account_deletion(
    pool: &PgPool,
    account_id: i64,
    moderator_account_id: i64,
    action: &str,
    target: &Target,
) -> Result<bool, DbError> {
    account::delete_by_id_with_audit(
        pool,
        account_id,
        new_line(moderator_account_id, action, target),
    )
    .await
}

/// Revalidates and rejects one account from a materialized bulk selection,
/// marking its selection row in the same transaction as deletion and audit.
/// Row locks make the pending/local and role-rank checks authoritative at the
/// instant of deletion, not merely when the confirmation page rendered.
pub async fn record_bulk_pending_rejection(
    pool: &PgPool,
    selection_token: &str,
    moderator_account_id: i64,
    moderator_position: i32,
    target: &Target,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let eligibility = sqlx::query_as::<_, (bool, Option<i32>)>(
        "SELECT u.approved, r.position
         FROM accounts a
         JOIN users u ON u.account_id = a.id
         LEFT JOIN user_roles r ON r.id = u.role_id
         WHERE a.id = $1 AND a.domain IS NULL AND NOT a.portable
         FOR UPDATE OF a, u",
    )
    .bind(target.id)
    .fetch_optional(&mut *tx)
    .await?;
    let eligible = eligibility.is_some_and(|(approved, position)| {
        !approved
            && target.id != moderator_account_id
            && position.is_none_or(|position| position < moderator_position)
    });

    let deleted = if eligible {
        let deleted = account::delete_by_id_tx(&mut tx, target.id).await?;
        if deleted {
            admin_action_log::record_tx(&mut tx, new_line(moderator_account_id, "reject", target))
                .await?;
        }
        deleted
    } else {
        false
    };
    let outcome = if deleted { "rejected" } else { "skipped" };
    sqlx::query(
        "UPDATE admin_account_bulk_selections
         SET outcome = $4
         WHERE token = $1 AND moderator_account_id = $2
           AND account_id = $3 AND outcome IS NULL",
    )
    .bind(selection_token)
    .bind(moderator_account_id)
    .bind(target.id)
    .bind(outcome)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(deleted)
}

/// Builds the audit line for a mutation whose DB function records it inside the
/// same transaction (e.g. [`plamenu_db::user::set_credentials_and_revoke`] for
/// the admin set-password path), so the action and its trail
/// commit or roll back together.
pub(crate) fn new_line<'a>(
    moderator_account_id: i64,
    action: &'a str,
    target: &'a Target,
) -> NewActionLog<'a> {
    NewActionLog {
        account_id: moderator_account_id,
        action,
        target_type: target.kind,
        target_id: target.id,
        human_identifier: &target.human,
        permalink: target.permalink.as_deref(),
    }
}

/// The display handle: `@user` for local accounts, `@user@domain` for remote.
fn handle(account: &Account) -> String {
    match &account.domain {
        Some(domain) => format!("@{}@{}", account.username, domain),
        None => format!("@{}", account.username),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_audit_identifier_keeps_only_the_origin() {
        let target = Target::webhook(
            42,
            "https://hooks.example/private/token-sentinel?credential=sentinel",
        );
        assert_eq!(target.human, "https://hooks.example");
        assert!(!target.human.contains("sentinel"));
    }
}
