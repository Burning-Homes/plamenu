//! `OAuth2` storage: apps, single-use authorization grants, access tokens.
//! Secrets never touch the database in the clear — callers pass hashes.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::{DbError, id};

/// Authorization codes live this long (OAuth 2.1 recommends short lifetimes).
pub const GRANT_TTL: Duration = Duration::minutes(10);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct App {
    pub id: i64,
    pub name: String,
    pub website: Option<String>,
    pub client_id: String,
    pub client_secret_hash: String,
    pub redirect_uris: Vec<String>,
    pub scopes: String,
    /// The user who created the app on the web "Development" page; `None` for
    /// apps registered over `POST /api/v1/apps` and the first-party web app.
    pub owner_user_id: Option<i64>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug)]
pub struct NewApp<'a> {
    pub name: &'a str,
    pub website: Option<&'a str>,
    pub client_id: &'a str,
    pub client_secret_hash: &'a str,
    pub redirect_uris: &'a [String],
    pub scopes: &'a str,
}

pub async fn create_app(pool: &PgPool, new: NewApp<'_>) -> Result<App, DbError> {
    create_app_with_owner(pool, new, None).await
}

/// Like [`create_app`], stamping the creating user — the web Development page's
/// registration path. Owned apps show up in [`list_owned_apps`].
pub async fn create_owned_app(
    pool: &PgPool,
    new: NewApp<'_>,
    owner_user_id: i64,
) -> Result<App, DbError> {
    create_app_with_owner(pool, new, Some(owner_user_id)).await
}

async fn create_app_with_owner(
    pool: &PgPool,
    new: NewApp<'_>,
    owner_user_id: Option<i64>,
) -> Result<App, DbError> {
    let app = sqlx::query_as!(
        App,
        r#"
        INSERT INTO oauth_apps (id, name, website, client_id, client_secret_hash,
                                redirect_uris, scopes, owner_user_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id, name, website, client_id, client_secret_hash,
                  redirect_uris, scopes, owner_user_id, created_at
        "#,
        id::next(),
        new.name,
        new.website,
        new.client_id,
        new.client_secret_hash,
        new.redirect_uris,
        new.scopes,
        owner_user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(app)
}

pub async fn find_app_by_client_id(pool: &PgPool, client_id: &str) -> Result<Option<App>, DbError> {
    let app = sqlx::query_as!(
        App,
        r#"
        SELECT id, name, website, client_id, client_secret_hash,
               redirect_uris, scopes, owner_user_id, created_at
        FROM oauth_apps
        WHERE client_id = $1
        "#,
        client_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(app)
}

pub async fn find_app_by_id(pool: &PgPool, app_id: i64) -> Result<Option<App>, DbError> {
    let app = sqlx::query_as!(
        App,
        r#"
        SELECT id, name, website, client_id, client_secret_hash,
               redirect_uris, scopes, owner_user_id, created_at
        FROM oauth_apps
        WHERE id = $1
        "#,
        app_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(app)
}

/// [`find_app_by_id`] over a set of ids in one query — the status render
/// batch resolves every distinct `application_id` on a page with one round
/// trip. Unknown ids are simply absent.
pub async fn find_apps_by_ids(pool: &PgPool, app_ids: &[i64]) -> Result<Vec<App>, DbError> {
    if app_ids.is_empty() {
        return Ok(Vec::new());
    }
    let apps = sqlx::query_as!(
        App,
        r#"
        SELECT id, name, website, client_id, client_secret_hash,
               redirect_uris, scopes, owner_user_id, created_at
        FROM oauth_apps
        WHERE id = ANY($1)
        "#,
        app_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(apps)
}

/// The applications a user created on the Development page, newest first
/// (Mastodon's `current_user.applications.order(id: :desc)`).
pub async fn list_owned_apps(pool: &PgPool, owner_user_id: i64) -> Result<Vec<App>, DbError> {
    let apps = sqlx::query_as!(
        App,
        r#"
        SELECT id, name, website, client_id, client_secret_hash,
               redirect_uris, scopes, owner_user_id, created_at
        FROM oauth_apps
        WHERE owner_user_id = $1
        ORDER BY id DESC
        "#,
        owner_user_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(apps)
}

/// One owned app by id — `None` when the id is unknown *or* belongs to someone
/// else, so a guessed id reads as a plain 404 (Mastodon's owner-scoped `find`).
pub async fn find_owned_app(
    pool: &PgPool,
    owner_user_id: i64,
    app_id: i64,
) -> Result<Option<App>, DbError> {
    let app = sqlx::query_as!(
        App,
        r#"
        SELECT id, name, website, client_id, client_secret_hash,
               redirect_uris, scopes, owner_user_id, created_at
        FROM oauth_apps
        WHERE id = $1 AND owner_user_id = $2
        "#,
        app_id,
        owner_user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(app)
}

/// Saves the Development page's editable attributes on an owned app.
pub async fn update_owned_app(
    pool: &PgPool,
    app_id: i64,
    name: &str,
    website: Option<&str>,
    redirect_uris: &[String],
    scopes: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE oauth_apps
        SET name = $2, website = $3, redirect_uris = $4, scopes = $5
        WHERE id = $1
        "#,
        app_id,
        name,
        website,
        redirect_uris,
        scopes,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes an application outright; grants and tokens go with it (`ON DELETE
/// CASCADE`), while statuses posted through it keep a NULL application.
pub async fn delete_app(pool: &PgPool, app_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM oauth_apps WHERE id = $1", app_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Grant {
    pub id: i64,
    pub app_id: i64,
    pub user_id: i64,
    pub redirect_uri: String,
    pub scopes: String,
    pub pkce_challenge: Option<String>,
    pub expires_at: OffsetDateTime,
}

pub struct NewGrant<'a> {
    pub code_hash: &'a str,
    pub app_id: i64,
    pub user_id: i64,
    pub redirect_uri: &'a str,
    pub scopes: &'a str,
    pub pkce_challenge: Option<&'a str>,
}

pub async fn create_grant(pool: &PgPool, new: NewGrant<'_>) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO oauth_grants (id, code_hash, app_id, user_id, redirect_uri,
                                  scopes, pkce_challenge, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        id::next(),
        new.code_hash,
        new.app_id,
        new.user_id,
        new.redirect_uri,
        new.scopes,
        new.pkce_challenge,
        OffsetDateTime::now_utc() + GRANT_TTL,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Consumes (deletes) a grant by code hash; expired or unknown codes yield
/// `None`. Single-use by construction.
pub async fn take_grant(pool: &PgPool, code_hash: &str) -> Result<Option<Grant>, DbError> {
    let grant = sqlx::query_as!(
        Grant,
        r#"
        DELETE FROM oauth_grants
        WHERE code_hash = $1 AND expires_at > now()
        RETURNING id, app_id, user_id, redirect_uri, scopes, pkce_challenge, expires_at
        "#,
        code_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(grant)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Token {
    pub id: i64,
    pub app_id: i64,
    pub user_id: Option<i64>,
    pub scopes: String,
    pub created_at: OffsetDateTime,
    /// The most recent request this token authenticated. NULL until the token
    /// is used (and it is only advanced past the [`TOUCH_INTERVAL`] throttle).
    pub last_used_at: Option<OffsetDateTime>,
}

/// The browser/client provenance captured once when a token is minted, used
/// by the active-sessions and authorized-apps pages.
#[derive(Debug, Default, Clone)]
pub struct SessionMeta<'a> {
    pub user_agent: Option<&'a str>,
    pub ip: Option<&'a str>,
}

/// Don't advance `last_used_at` on every authenticated request — a signed-in
/// tab hits many endpoints a minute. One write per token per this window keeps
/// the sessions list fresh enough without a row write on the hot path.
pub const TOUCH_INTERVAL: Duration = Duration::minutes(5);

pub async fn create_token(
    pool: &PgPool,
    token_hash: &str,
    app_id: i64,
    user_id: Option<i64>,
    scopes: &str,
) -> Result<Token, DbError> {
    create_token_with_meta(
        pool,
        token_hash,
        app_id,
        user_id,
        scopes,
        SessionMeta::default(),
    )
    .await
}

/// Like [`create_token`], recording the client that minted the token so it can
/// be shown (and revoked) on the security-settings pages.
pub async fn create_token_with_meta(
    pool: &PgPool,
    token_hash: &str,
    app_id: i64,
    user_id: Option<i64>,
    scopes: &str,
    meta: SessionMeta<'_>,
) -> Result<Token, DbError> {
    let token = sqlx::query_as!(
        Token,
        r#"
        INSERT INTO oauth_tokens (id, token_hash, app_id, user_id, scopes,
                                  user_agent, last_used_ip)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, app_id, user_id, scopes, created_at, last_used_at
        "#,
        id::next(),
        token_hash,
        app_id,
        user_id,
        scopes,
        meta.user_agent,
        meta.ip,
    )
    .fetch_one(pool)
    .await?;
    Ok(token)
}

/// Mints an app-level (`client_credentials`, no resource owner) access token
/// while holding the app to a bounded number of live such tokens. In one
/// transaction it deletes the app's oldest live user-less tokens so that at
/// most `cap - 1` remain, then inserts the new one — a hard per-app ceiling of
/// `cap` live app tokens, regardless of how fast an anonymous caller mints them.
/// User tokens (`user_id IS NOT NULL`, i.e. browser/API
/// sessions) are never touched by this pruning. Deleting rather than revoking
/// keeps the table from filling with dead rows the attacker has no reason to
/// let `vacuum_oauth` reap.
pub async fn create_app_token_capped(
    pool: &PgPool,
    token_hash: &str,
    app_id: i64,
    scopes: &str,
    meta: SessionMeta<'_>,
    cap: i64,
) -> Result<Token, DbError> {
    let mut tx = pool.begin().await?;
    // Keep the newest `cap - 1` live app tokens; evict the rest to make room
    // for the one about to be inserted. `cap >= 1` is a caller invariant.
    sqlx::query!(
        r#"
        DELETE FROM oauth_tokens
        WHERE id IN (
            SELECT id FROM oauth_tokens
            WHERE app_id = $1 AND user_id IS NULL AND revoked_at IS NULL
            ORDER BY id DESC
            OFFSET $2
        )
        "#,
        app_id,
        (cap - 1).max(0),
    )
    .execute(&mut *tx)
    .await?;
    let token = sqlx::query_as!(
        Token,
        r#"
        INSERT INTO oauth_tokens (id, token_hash, app_id, user_id, scopes,
                                  user_agent, last_used_ip)
        VALUES ($1, $2, $3, NULL, $4, $5, $6)
        RETURNING id, app_id, user_id, scopes, created_at, last_used_at
        "#,
        id::next(),
        token_hash,
        app_id,
        scopes,
        meta.user_agent,
        meta.ip,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// The number of live (non-revoked) app-level tokens an app holds. Test/
/// introspection helper for the [`create_app_token_capped`] ceiling.
pub async fn count_live_app_tokens(pool: &PgPool, app_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM oauth_tokens
        WHERE app_id = $1 AND user_id IS NULL AND revoked_at IS NULL
        "#,
        app_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Finds a live (non-revoked) token by hash.
pub async fn find_active_token(pool: &PgPool, token_hash: &str) -> Result<Option<Token>, DbError> {
    let token = sqlx::query_as!(
        Token,
        r#"
        SELECT id, app_id, user_id, scopes, created_at, last_used_at
        FROM oauth_tokens
        WHERE token_hash = $1 AND revoked_at IS NULL
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(token)
}

/// The newest live token a user holds for one app — the Development page's
/// "your access token" row (metadata only; the secret itself is never stored).
pub async fn find_user_app_token(
    pool: &PgPool,
    user_id: i64,
    app_id: i64,
) -> Result<Option<Token>, DbError> {
    let token = sqlx::query_as!(
        Token,
        r#"
        SELECT id, app_id, user_id, scopes, created_at, last_used_at
        FROM oauth_tokens
        WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        user_id,
        app_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(token)
}

/// Advances `last_used_at`/`last_used_ip` for a token that just authenticated a
/// request. The `WHERE` guard (kept in step with [`TOUCH_INTERVAL`]) makes a
/// fresh token a no-op write; callers gate on [`Token::last_used_at`] first to
/// skip even this round-trip on the hot path.
pub async fn touch_token(pool: &PgPool, token_id: i64, ip: Option<&str>) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE oauth_tokens
        SET last_used_at = now(),
            last_used_ip = COALESCE($2, last_used_ip)
        WHERE id = $1
          AND (last_used_at IS NULL OR last_used_at < now() - interval '5 minutes')
        "#,
        token_id,
        ip,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Revokes a token (idempotent); the hash must belong to `app_id` — RFC 7009
/// forbids revoking other clients' tokens.
pub async fn revoke_token(pool: &PgPool, token_hash: &str, app_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE oauth_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND app_id = $2 AND revoked_at IS NULL",
        token_hash,
        app_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Revokes every live token of a user across all applications — a password
/// reset signs the account out everywhere, sessions and API clients
/// alike. Returns how many tokens were revoked.
pub async fn revoke_all_for_user(pool: &PgPool, user_id: i64) -> Result<u64, DbError> {
    revoke_all_for_user_conn(pool, user_id).await
}

/// [`revoke_all_for_user`] against an open transaction (or any executor), so a
/// credential change and its sign-out-everywhere commit as one unit rather than
/// as two independent writes where the revocation can fail after the password
/// has already changed and leave a copied token live.
pub(crate) async fn revoke_all_for_user_conn<'e, E>(
    executor: E,
    user_id: i64,
) -> Result<u64, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let result = sqlx::query!(
        "UPDATE oauth_tokens SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL",
        user_id,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// One row on the authorized-apps page: an application the user has at least
/// one live token for, folded across those tokens.
#[derive(Debug, Clone)]
pub struct AuthorizedApp {
    pub id: i64,
    pub name: String,
    pub website: Option<String>,
    pub scopes: String,
    /// The oldest live token for this app — when the user first authorized it.
    pub authorized_at: OffsetDateTime,
    /// The newest use across the app's live tokens, if any has been used.
    pub last_used_at: Option<OffsetDateTime>,
}

/// Third-party applications the user has granted a live token, newest-authorized
/// first. `exclude_app_id` drops the first-party web app (Mastodon's superapp,
/// which is never listed as a revocable authorization).
pub async fn list_authorized_apps(
    pool: &PgPool,
    user_id: i64,
    exclude_app_id: i64,
) -> Result<Vec<AuthorizedApp>, DbError> {
    let apps = sqlx::query_as!(
        AuthorizedApp,
        r#"
        SELECT app.id AS "id!",
               app.name AS "name!",
               app.website,
               app.scopes AS "scopes!",
               min(t.created_at) AS "authorized_at!",
               max(t.last_used_at) AS "last_used_at"
        FROM oauth_tokens t
        JOIN oauth_apps app ON app.id = t.app_id
        WHERE t.user_id = $1 AND t.revoked_at IS NULL AND t.app_id <> $2
        GROUP BY app.id, app.name, app.website, app.scopes
        ORDER BY min(t.created_at) DESC
        "#,
        user_id,
        exclude_app_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(apps)
}

/// Revokes every live token a user holds for one application — the
/// authorized-apps "revoke" button. Returns how many were revoked.
pub async fn revoke_app_for_user(pool: &PgPool, user_id: i64, app_id: i64) -> Result<u64, DbError> {
    let result = sqlx::query!(
        "UPDATE oauth_tokens SET revoked_at = now()
         WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL",
        user_id,
        app_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// One row on the active-sessions page: a live first-party web token.
#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub id: i64,
    pub user_agent: Option<String>,
    pub last_used_ip: Option<String>,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
}

/// Live browser sessions for a user: the non-revoked tokens under the
/// first-party web app, newest first.
pub async fn list_active_sessions(
    pool: &PgPool,
    user_id: i64,
    web_app_id: i64,
) -> Result<Vec<ActiveSession>, DbError> {
    let sessions = sqlx::query_as!(
        ActiveSession,
        r#"
        SELECT id, user_agent, last_used_ip, created_at, last_used_at
        FROM oauth_tokens
        WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL
        ORDER BY created_at DESC
        "#,
        user_id,
        web_app_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(sessions)
}

/// Revokes one browser session by token id, scoped to the owning user and the
/// first-party web app so a stray id can't reach an API client's token.
/// Returns how many rows matched (0 when the id is unknown or already gone).
pub async fn revoke_session(
    pool: &PgPool,
    user_id: i64,
    web_app_id: i64,
    token_id: i64,
) -> Result<u64, DbError> {
    let result = sqlx::query!(
        "UPDATE oauth_tokens SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND app_id = $3 AND revoked_at IS NULL",
        token_id,
        user_id,
        web_app_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::user;

    async fn fixture(pool: &PgPool) -> (App, i64) {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let user = user::create(pool, account.id, Some("alice@example.com"), "h")
            .await
            .unwrap();
        let app = create_app(
            pool,
            NewApp {
                name: "test app",
                website: None,
                client_id: "client123",
                client_secret_hash: "secret_hash",
                redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
                scopes: "read write",
            },
        )
        .await
        .unwrap();
        (app, user.id)
    }

    #[sqlx::test]
    async fn app_roundtrip(pool: PgPool) {
        let (app, _) = fixture(&pool).await;
        let found = find_app_by_client_id(&pool, "client123")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, app.id);
        assert_eq!(found.redirect_uris, ["urn:ietf:wg:oauth:2.0:oob"]);
        assert!(
            find_app_by_client_id(&pool, "nope")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn grants_are_single_use(pool: PgPool) {
        let (app, user_id) = fixture(&pool).await;
        create_grant(
            &pool,
            NewGrant {
                code_hash: "codehash",
                app_id: app.id,
                user_id,
                redirect_uri: "urn:ietf:wg:oauth:2.0:oob",
                scopes: "read",
                pkce_challenge: Some("challenge"),
            },
        )
        .await
        .unwrap();

        let grant = take_grant(&pool, "codehash").await.unwrap().unwrap();
        assert_eq!(grant.user_id, user_id);
        assert_eq!(grant.pkce_challenge.as_deref(), Some("challenge"));
        // Second exchange fails: the code is gone.
        assert!(take_grant(&pool, "codehash").await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn owned_app_lifecycle(pool: PgPool) {
        let (unowned, user_id) = fixture(&pool).await;
        let owned = create_owned_app(
            &pool,
            NewApp {
                name: "my bot",
                website: Some("https://bot.example"),
                client_id: "botclient",
                client_secret_hash: "hash",
                redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
                scopes: "read write",
            },
            user_id,
        )
        .await
        .unwrap();
        assert_eq!(owned.owner_user_id, Some(user_id));

        // Only the owned app is listed; the API-registered one is not.
        let listed = list_owned_apps(&pool, user_id).await.unwrap();
        assert_eq!(listed.iter().map(|a| a.id).collect::<Vec<_>>(), [owned.id]);
        // Owner-scoped lookup: someone else's id reads as absent.
        assert!(
            find_owned_app(&pool, user_id, owned.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            find_owned_app(&pool, user_id, unowned.id)
                .await
                .unwrap()
                .is_none()
        );

        update_owned_app(
            &pool,
            owned.id,
            "renamed bot",
            None,
            &["https://bot.example/callback".to_owned()],
            "read",
        )
        .await
        .unwrap();
        let updated = find_owned_app(&pool, user_id, owned.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "renamed bot");
        assert_eq!(updated.website, None);
        assert_eq!(updated.redirect_uris, ["https://bot.example/callback"]);
        assert_eq!(updated.scopes, "read");

        // Deleting the app takes its tokens with it.
        create_token(&pool, "ownertok", owned.id, Some(user_id), "read")
            .await
            .unwrap();
        assert!(
            find_user_app_token(&pool, user_id, owned.id)
                .await
                .unwrap()
                .is_some()
        );
        delete_app(&pool, owned.id).await.unwrap();
        assert!(list_owned_apps(&pool, user_id).await.unwrap().is_empty());
        assert!(
            find_active_token(&pool, "ownertok")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn token_lifecycle_and_revocation(pool: PgPool) {
        let (app, user_id) = fixture(&pool).await;
        create_token(&pool, "tokenhash", app.id, Some(user_id), "read write")
            .await
            .unwrap();
        let token = find_active_token(&pool, "tokenhash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token.user_id, Some(user_id));
        assert_eq!(token.scopes, "read write");

        // A different app cannot revoke it.
        revoke_token(&pool, "tokenhash", app.id + 1).await.unwrap();
        assert!(
            find_active_token(&pool, "tokenhash")
                .await
                .unwrap()
                .is_some()
        );
        // The owning app can.
        revoke_token(&pool, "tokenhash", app.id).await.unwrap();
        assert!(
            find_active_token(&pool, "tokenhash")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Minting app-level tokens through the capped path holds one app to a hard
    /// ceiling of live tokens: the oldest are evicted so the count never grows
    /// past the cap, however many are minted. A user session for
    /// the same app is untouched by the eviction.
    #[sqlx::test]
    async fn app_token_minting_is_capped_per_app(pool: PgPool) {
        let (app, user_id) = fixture(&pool).await;

        // A real user session for this app must survive app-token pruning.
        create_token(&pool, "user-session", app.id, Some(user_id), "read")
            .await
            .unwrap();

        // Mint well past the cap of 3; the oldest app tokens are evicted.
        let cap = 3;
        let mut hashes = Vec::new();
        for i in 0..10 {
            let hash = format!("app-token-{i}");
            create_app_token_capped(&pool, &hash, app.id, "read", SessionMeta::default(), cap)
                .await
                .unwrap();
            hashes.push(hash);
        }

        // Never more than the cap of live app tokens.
        assert_eq!(count_live_app_tokens(&pool, app.id).await.unwrap(), cap);

        // The survivors are exactly the newest `cap` mints; older ones are gone.
        for old in &hashes[..hashes.len() - usize::try_from(cap).unwrap()] {
            assert!(find_active_token(&pool, old).await.unwrap().is_none());
        }
        for recent in &hashes[hashes.len() - usize::try_from(cap).unwrap()..] {
            let token = find_active_token(&pool, recent).await.unwrap().unwrap();
            assert_eq!(token.user_id, None);
        }

        // The user session is still live — app-token pruning never touches it.
        assert!(
            find_active_token(&pool, "user-session")
                .await
                .unwrap()
                .is_some()
        );
    }
}
