//! Periodic data-retention and vacuum operations — Mastodon's
//! user-cleanup, IP-cleanup and vacuum schedulers, minus the pieces
//! Plamenu has no equivalent for
//! (Redis feeds, Elasticsearch imports). Each function is one self-contained,
//! idempotent statement returning the number of rows it touched, so the worker
//! ([`crate::maintenance`](../../../server/src/maintenance.rs)) can run and log
//! them independently.

use sqlx::PgPool;

use crate::DbError;

/// Deletes local accounts whose e-mail was never confirmed and whose
/// confirmation e-mail went out more than `max_age_days` ago — Mastodon's
/// user-cleanup scheduler does the same (7 days). CLI-created
/// accounts are backfilled `confirmed_at = created_at` (migration 0052) and a
/// self-signup with no e-mail is confirmed immediately, so only genuinely
/// stalled e-mail sign-ups match. Deleting the `accounts` row cascades the
/// `users` row (`users.account_id … ON DELETE CASCADE`) and every other
/// account-owned record.
pub async fn delete_stale_unconfirmed(pool: &PgPool, max_age_days: i32) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        DELETE FROM accounts
        WHERE id IN (
            SELECT account_id FROM users
            WHERE confirmed_at IS NULL
              AND confirmation_sent_at IS NOT NULL
              AND confirmation_sent_at < now() - make_interval(days => $1)
        )
        "#,
        max_age_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Deletes sign-in log rows older than `retention_days` — the aged half of
/// Mastodon's IP-cleanup scheduler, which destroys old sign-in records
/// outright.
/// The daily-unique `active_users` metric only reads the trailing window, so
/// pruning ancient rows never moves it.
pub async fn prune_login_activities(pool: &PgPool, retention_days: i32) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        DELETE FROM login_activities
        WHERE created_at < now() - make_interval(days => $1)
        "#,
        retention_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Forgets the recorded IP addresses of accounts and tokens dormant longer than
/// `retention_days`, keeping the rows — Mastodon's IP-cleanup scheduler
/// nulling of `users.sign_up_ip` and `oauth_access_tokens.last_used_ip` past
/// the retention window. Returns the total number of IP columns cleared.
pub async fn scrub_stale_ips(pool: &PgPool, retention_days: i32) -> Result<u64, DbError> {
    let sign_up = sqlx::query!(
        r#"
        UPDATE users SET sign_up_ip = NULL
        WHERE sign_up_ip IS NOT NULL
          AND current_sign_in_at IS NOT NULL
          AND current_sign_in_at < now() - make_interval(days => $1)
        "#,
        retention_days,
    )
    .execute(pool)
    .await?;
    let token_ip = sqlx::query!(
        r#"
        UPDATE oauth_tokens SET last_used_ip = NULL
        WHERE last_used_ip IS NOT NULL
          AND last_used_at IS NOT NULL
          AND last_used_at < now() - make_interval(days => $1)
        "#,
        retention_days,
    )
    .execute(pool)
    .await?;
    Ok(sign_up.rows_affected() + token_ip.rows_affected())
}

/// Deletes OAuth tokens revoked more than a day ago and access grants that have
/// expired — Mastodon's `Vacuum::AccessTokensVacuum` (doorkeeper's
/// `cleanup_stale` for revoked/expired tokens and used/expired grants). A grant
/// is a short-lived authorization code; once past `expires_at` it is dead
/// weight. Returns the total rows removed.
pub async fn vacuum_oauth(pool: &PgPool) -> Result<u64, DbError> {
    let tokens = sqlx::query!(
        r#"
        DELETE FROM oauth_tokens
        WHERE revoked_at IS NOT NULL
          AND revoked_at < now() - make_interval(days => 1)
        "#,
    )
    .execute(pool)
    .await?;
    // App-level (`client_credentials`, no resource owner) tokens never expire on
    // their own and an anonymous minter has no reason to revoke them, so without
    // a retention rule they accumulate forever. Reap those unused
    // for 30 days — measured from last use, falling back to creation. User
    // sessions (`user_id IS NOT NULL`) are deliberately excluded: their lifetime
    // is governed elsewhere, not by this sweep.
    let stale_app_tokens = sqlx::query!(
        r#"
        DELETE FROM oauth_tokens
        WHERE user_id IS NULL
          AND revoked_at IS NULL
          AND coalesce(last_used_at, created_at) < now() - make_interval(days => 30)
        "#,
    )
    .execute(pool)
    .await?;
    let grants = sqlx::query!(
        r#"
        DELETE FROM oauth_grants
        WHERE expires_at < now()
        "#,
    )
    .execute(pool)
    .await?;
    Ok(tokens.rows_affected() + stale_app_tokens.rows_affected() + grants.rows_affected())
}

/// Deletes link-preview cards no status references any more, once they are
/// older than `retention_days` — Mastodon's `Vacuum::PreviewCardsVacuum`
/// (orphaned cards past the media-cache retention). A card is shared by every
/// status linking its URL through `preview_cards_statuses`; when the last such
/// status is gone the card is dead. `retention_days = 0` (retention disabled)
/// keeps every card forever, matching the media-cache convention.
pub async fn vacuum_orphan_preview_cards(
    pool: &PgPool,
    retention_days: i32,
) -> Result<u64, DbError> {
    if retention_days == 0 {
        return Ok(0);
    }
    let result = sqlx::query!(
        r#"
        DELETE FROM preview_cards
        WHERE updated_at < now() - make_interval(days => $1)
          AND NOT EXISTS (
              SELECT 1 FROM preview_cards_statuses
              WHERE preview_card_id = preview_cards.id
          )
        "#,
        retention_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Whether any local user who can work the moderation queue (holds
/// `MANAGE_REPORTS`, or is an `ADMINISTRATOR`) has signed in since `cutoff` —
/// the freshness check behind Mastodon's auto-close-registrations scheduler.
pub async fn active_moderator_since(
    pool: &PgPool,
    manage_reports: i64,
    administrator: i64,
    cutoff: time::OffsetDateTime,
) -> Result<bool, DbError> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM users u
            JOIN user_roles r ON r.id = u.role_id
            WHERE u.current_sign_in_at IS NOT NULL
              AND u.current_sign_in_at >= $3
              AND (
                  r.permissions & $1 = $1
                  OR r.permissions & $2 <> 0
              )
        ) AS "exists!"
        "#,
        manage_reports,
        administrator,
        cutoff,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::role::permission;
    use crate::status::{self, NewLocalStatus};
    use crate::{id, oauth, role, user};

    async fn seed_account(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn deletes_only_stale_unconfirmed_accounts(pool: PgPool) {
        // A: unconfirmed, e-mail sent 8 days ago → swept.
        let stale = seed_account(&pool, "stale").await;
        let stale_user = user::create(&pool, stale, Some("stale@x"), "h")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET confirmed_at = NULL, confirmation_sent_at = now() - make_interval(days => 8) WHERE id = $1",
            stale_user.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        // B: unconfirmed but only a day old → kept.
        let recent = seed_account(&pool, "recent").await;
        let recent_user = user::create(&pool, recent, Some("recent@x"), "h")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET confirmed_at = NULL, confirmation_sent_at = now() - make_interval(days => 1) WHERE id = $1",
            recent_user.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        // C: confirmed (the default) → kept regardless of age.
        let confirmed = seed_account(&pool, "confirmed").await;
        user::create(&pool, confirmed, Some("ok@x"), "h")
            .await
            .unwrap();

        let swept = delete_stale_unconfirmed(&pool, 7).await.unwrap();
        assert_eq!(swept, 1);
        assert!(account::find_by_id(&pool, stale).await.unwrap().is_none());
        assert!(account::find_by_id(&pool, recent).await.unwrap().is_some());
        assert!(
            account::find_by_id(&pool, confirmed)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    async fn prunes_old_login_activities_and_scrubs_ips(pool: PgPool) {
        let acct = seed_account(&pool, "alice").await;
        let u = user::create(&pool, acct, Some("a@x"), "h").await.unwrap();
        // Two sign-in log rows: one ancient, one recent.
        for (label, age) in [("old", 400), ("new", 5)] {
            sqlx::query!(
                "INSERT INTO login_activities (id, user_id, created_at) VALUES ($1, $2, now() - make_interval(days => $3))",
                id::next(),
                u.id,
                age,
            )
            .execute(&pool)
            .await
            .unwrap();
            let _ = label;
        }
        let pruned = prune_login_activities(&pool, 365).await.unwrap();
        assert_eq!(pruned, 1, "only the >1y row is dropped");

        // IP scrub: a dormant account keeps its row but loses the recorded IP.
        sqlx::query!(
            "UPDATE users SET sign_up_ip = '203.0.113.1', current_sign_in_at = now() - make_interval(days => 400) WHERE id = $1",
            u.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        let app = oauth::create_app(
            &pool,
            oauth::NewApp {
                name: "app",
                website: None,
                client_id: "cid",
                client_secret_hash: "sec",
                redirect_uris: &["urn:x".to_owned()],
                scopes: "read",
            },
        )
        .await
        .unwrap();
        oauth::create_token_with_meta(
            &pool,
            "tok",
            app.id,
            Some(u.id),
            "read",
            oauth::SessionMeta {
                user_agent: None,
                ip: Some("203.0.113.9"),
            },
        )
        .await
        .unwrap();
        sqlx::query!("UPDATE oauth_tokens SET last_used_at = now() - make_interval(days => 400)")
            .execute(&pool)
            .await
            .unwrap();

        let scrubbed = scrub_stale_ips(&pool, 365).await.unwrap();
        assert_eq!(
            scrubbed, 2,
            "sign_up_ip and token last_used_ip both cleared"
        );
        let ip: Option<String> =
            sqlx::query_scalar!("SELECT sign_up_ip FROM users WHERE id = $1", u.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ip.is_none());
    }

    #[sqlx::test]
    async fn vacuums_spent_oauth(pool: PgPool) {
        let acct = seed_account(&pool, "alice").await;
        let u = user::create(&pool, acct, Some("a@x"), "h").await.unwrap();
        let app = oauth::create_app(
            &pool,
            oauth::NewApp {
                name: "app",
                website: None,
                client_id: "cid",
                client_secret_hash: "sec",
                redirect_uris: &["urn:x".to_owned()],
                scopes: "read",
            },
        )
        .await
        .unwrap();
        // An expired grant and a token revoked two days ago → both vacuumed.
        sqlx::query!(
            "INSERT INTO oauth_grants (id, code_hash, app_id, user_id, redirect_uri, scopes, expires_at) \
             VALUES ($1, 'c', $2, $3, 'urn:x', 'read', now() - make_interval(days => 1))",
            id::next(),
            app.id,
            u.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        oauth::create_token(&pool, "tok", app.id, Some(u.id), "read")
            .await
            .unwrap();
        sqlx::query!("UPDATE oauth_tokens SET revoked_at = now() - make_interval(days => 2)")
            .execute(&pool)
            .await
            .unwrap();

        let removed = vacuum_oauth(&pool).await.unwrap();
        assert_eq!(removed, 2);
    }

    /// App-level `client_credentials` tokens unused for 30 days are reaped so
    /// an anonymous minter can't grow the table without bound; a fresh app
    /// token and a long-idle *user* session are both left alone.
    #[sqlx::test]
    async fn vacuums_stale_app_level_tokens_only(pool: PgPool) {
        let acct = seed_account(&pool, "alice").await;
        let u = user::create(&pool, acct, Some("a@x"), "h").await.unwrap();
        let app = oauth::create_app(
            &pool,
            oauth::NewApp {
                name: "app",
                website: None,
                client_id: "cid",
                client_secret_hash: "sec",
                redirect_uris: &["urn:x".to_owned()],
                scopes: "read",
            },
        )
        .await
        .unwrap();

        // An app-level token last used 40 days ago → reaped.
        oauth::create_token(&pool, "stale-app", app.id, None, "read")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE oauth_tokens SET created_at = now() - make_interval(days => 40), \
             last_used_at = now() - make_interval(days => 40) WHERE token_hash = 'stale-app'"
        )
        .execute(&pool)
        .await
        .unwrap();

        // A fresh app-level token → kept.
        oauth::create_token(&pool, "fresh-app", app.id, None, "read")
            .await
            .unwrap();

        // A user session idle for 40 days → kept (its lifetime is not this
        // sweep's concern).
        oauth::create_token(&pool, "old-user", app.id, Some(u.id), "read")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE oauth_tokens SET created_at = now() - make_interval(days => 40), \
             last_used_at = now() - make_interval(days => 40) WHERE token_hash = 'old-user'"
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed = vacuum_oauth(&pool).await.unwrap();
        assert_eq!(removed, 1);
        assert!(
            oauth::find_active_token(&pool, "stale-app")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            oauth::find_active_token(&pool, "fresh-app")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            oauth::find_active_token(&pool, "old-user")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    async fn vacuums_only_old_orphan_preview_cards(pool: PgPool) {
        let acct = seed_account(&pool, "alice").await;
        // Old orphan → removed.
        sqlx::query!(
            "INSERT INTO preview_cards (id, url, updated_at) VALUES ($1, 'https://a/1', now() - make_interval(days => 40))",
            id::next(),
        )
        .execute(&pool)
        .await
        .unwrap();
        // Recent orphan → kept (too fresh).
        sqlx::query!(
            "INSERT INTO preview_cards (id, url, updated_at) VALUES ($1, 'https://a/2', now())",
            id::next(),
        )
        .execute(&pool)
        .await
        .unwrap();
        // Old but still referenced by a status → kept.
        let referenced = sqlx::query_scalar!(
            "INSERT INTO preview_cards (id, url, updated_at) VALUES ($1, 'https://a/3', now() - make_interval(days => 40)) RETURNING id",
            id::next(),
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let st = status::create_local(
            &pool,
            NewLocalStatus::new(acct, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO preview_cards_statuses (preview_card_id, status_id, url) VALUES ($1, $2, 'https://a/3')",
            referenced,
            st.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed = vacuum_orphan_preview_cards(&pool, 14).await.unwrap();
        assert_eq!(removed, 1);
        // Retention disabled keeps everything.
        assert_eq!(vacuum_orphan_preview_cards(&pool, 0).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn detects_active_moderator(pool: PgPool) {
        let acct = seed_account(&pool, "mod").await;
        let u = user::create(&pool, acct, Some("m@x"), "h").await.unwrap();
        let mod_role = role::create(&pool, "Mod", "", 10, permission::MANAGE_REPORTS, false)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET role_id = $1, current_sign_in_at = now() WHERE id = $2",
            mod_role.id,
            u.id
        )
        .execute(&pool)
        .await
        .unwrap();
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(8);
        assert!(
            active_moderator_since(
                &pool,
                permission::MANAGE_REPORTS,
                permission::ADMINISTRATOR,
                cutoff,
            )
            .await
            .unwrap()
        );

        // Backdate the sign-in past the window → no longer active.
        sqlx::query!(
            "UPDATE users SET current_sign_in_at = now() - make_interval(days => 10) WHERE id = $1",
            u.id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            !active_moderator_since(
                &pool,
                permission::MANAGE_REPORTS,
                permission::ADMINISTRATOR,
                cutoff,
            )
            .await
            .unwrap()
        );
    }
}
