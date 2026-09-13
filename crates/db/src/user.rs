//! Authentication identities (users) behind local accounts.

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// Failed password attempts before a login is temporarily locked.
///
/// This is intentionally conservative for the first lockout pass: it
/// blocks automated guessing without turning a couple of typo retries into
/// account recovery work.
pub const LOGIN_LOCK_MAX_ATTEMPTS: i32 = 10;

/// How long a failed-attempt lock remains active.
pub const LOGIN_LOCK_WINDOW_SECONDS: i32 = 60 * 60;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub account_id: i64,
    /// Optional: an account functions fully without an address. When set it
    /// works as a login identifier (alongside the username) and enables
    /// password reset and notification mail.
    pub email: Option<String>,
    /// argon2id PHC string.
    pub password_hash: String,
    /// Moderation login gate (`Admin::AccountAction` `disable`): a disabled
    /// user cannot sign in and their existing tokens stop resolving.
    pub disabled: bool,
    /// Registration gate: false while awaiting admin
    /// approval in `approved` registrations mode.
    pub approved: bool,
    /// E-mail confirmation timestamp; NULL = unconfirmed.
    pub confirmed_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    /// The encrypted TOTP secret. Set during 2FA enrollment, before
    /// `otp_required_for_login` flips (Mastodon keeps the provisional secret
    /// the same way while the user confirms their authenticator).
    pub otp_secret: Option<String>,
    /// Whether sign-in demands a second factor.
    pub otp_required_for_login: bool,
    /// The last TOTP timestep accepted, so a code cannot be replayed inside
    /// its drift window (devise-two-factor's `consumed_timestep`).
    pub otp_consumed_timestep: Option<i64>,
}

impl User {
    #[must_use]
    pub fn confirmed(&self) -> bool {
        self.confirmed_at.is_some()
    }

    /// Mastodon's login-usability check (minus the memorial part Plamenu
    /// doesn't have): may this login actually be used?
    #[must_use]
    pub fn functional(&self) -> bool {
        self.confirmed() && self.approved && !self.disabled
    }
}

/// Stored posting visibility preference.
///
/// `Default` is a sentinel: it resolves to `private` for locked accounts and
/// `public` otherwise, matching Mastodon's fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PostingDefaultVisibility {
    #[default]
    Default,
    Public,
    Unlisted,
    Private,
    Direct,
    Local,
}

impl PostingDefaultVisibility {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "public" => Self::Public,
            "unlisted" => Self::Unlisted,
            "private" => Self::Private,
            "direct" => Self::Direct,
            "local" => Self::Local,
            _ => Self::Default,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
            Self::Direct => "direct",
            Self::Local => "local",
        }
    }

    #[must_use]
    pub fn resolve(self, locked: bool) -> &'static str {
        match self {
            Self::Default if locked => "private",
            Self::Default | Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
            Self::Direct => "direct",
            Self::Local => "local",
        }
    }
}

/// Stored media expansion preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadingExpandMedia {
    #[default]
    Default,
    ShowAll,
    HideAll,
}

impl ReadingExpandMedia {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "show_all" => Self::ShowAll,
            "hide_all" => Self::HideAll,
            _ => Self::Default,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ShowAll => "show_all",
            Self::HideAll => "hide_all",
        }
    }
}

/// Stored timeline ordering preference: chronological by post date (clamped
/// to ingest time — what Mastodon's publish-time snowflake ids yield) or in
/// the order this server received the rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineOrder {
    #[default]
    Published,
    Received,
}

impl TimelineOrder {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "received" => Self::Received,
            _ => Self::Published,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Received => "received",
        }
    }
}

/// Stored reply/thread ordering preference. `Tree` is what Mastodon serves
/// from `/context`: ancestors are the direct reply chain, descendants come in
/// depth-first tree order with the root author's uninterrupted self-replies
/// promoted to the front. `Flat` is Pleroma's shape: the whole conversation in
/// arrival order, split at the focal post into ancestors and descendants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThreadOrder {
    #[default]
    Tree,
    Flat,
}

impl ThreadOrder {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "flat" => Self::Flat,
            _ => Self::Tree,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Flat => "flat",
        }
    }
}

/// Stored default quote policy (Mastodon's `setting_default_quote_policy`):
/// who may quote a new post when it carries no explicit
/// `quote_approval_policy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DefaultQuotePolicy {
    #[default]
    Public,
    Followers,
    Nobody,
}

impl DefaultQuotePolicy {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "followers" => Self::Followers,
            "nobody" => Self::Nobody,
            _ => Self::Public,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Followers => "followers",
            Self::Nobody => "nobody",
        }
    }
}

/// Stored default composer format (P4, Pleroma's `content_type`): how a new
/// post's text is rendered when the client sends no explicit `content_type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PostingDefaultFormat {
    #[default]
    Plain,
    Markdown,
    Html,
}

impl PostingDefaultFormat {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "text/markdown" => Self::Markdown,
            "text/html" => Self::Html,
            _ => Self::Plain,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Html => "text/html",
        }
    }
}

/// Per-user preferences stored in typed `users` columns.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's user settings"
)]
pub struct UserSettings {
    pub posting_default_visibility: PostingDefaultVisibility,
    pub posting_default_sensitive: bool,
    pub posting_default_language: String,
    pub posting_default_quote_policy: DefaultQuotePolicy,
    /// Default composer format (P4): the `content_type` a new post gets when
    /// the client sends none.
    pub posting_default_content_type: PostingDefaultFormat,
    pub reading_expand_media: ReadingExpandMedia,
    pub reading_expand_spoilers: bool,
    pub reading_autoplay_gifs: bool,
    /// Allow direct-origin media fallback when the instance cannot cache a
    /// remote file (for example, an oversized video). Enabled by default;
    /// disabling it keeps those requests on the instance and protects the
    /// viewer's IP at the cost of leaving uncacheable media unavailable.
    pub reading_allow_direct_remote_media: bool,
    /// Whether the built-in web client merges repeated boosts of one post into
    /// a single card — Mastodon's `aggregate_reblogs`, and like it a
    /// per-reader setting the operator's master switch can override off. Web
    /// only: the API always serves every boost row.
    pub reading_collapse_boosts: bool,
    /// The language on-demand status translation targets; `None` follows
    /// `posting_default_language` (the pre-preference behavior).
    pub reading_translate_language: Option<String>,
    pub timeline_order: TimelineOrder,
    pub thread_order: ThreadOrder,
    /// Opt-out of search-engine indexing (Mastodon's `noindex` setting):
    /// robots meta on the owner's public web pages, `noindex` on the entity.
    pub noindex: bool,
    /// Mastodon's "Disclose application used to post" setting
    /// (`user.show_application`): whether serialized statuses carry the
    /// authoring app's name for other viewers.
    pub show_application: bool,
    /// The viewer's IANA display zone; `None` = the server default
    /// (UTC). Kept on the settings struct so page renders that already load
    /// settings get the zone without a second query.
    pub time_zone: Option<String>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            posting_default_visibility: PostingDefaultVisibility::Default,
            posting_default_sensitive: false,
            posting_default_language: "en".to_owned(),
            posting_default_quote_policy: DefaultQuotePolicy::Public,
            posting_default_content_type: PostingDefaultFormat::Plain,
            reading_expand_media: ReadingExpandMedia::Default,
            reading_expand_spoilers: false,
            reading_autoplay_gifs: false,
            reading_allow_direct_remote_media: true,
            reading_collapse_boosts: true,
            reading_translate_language: None,
            timeline_order: TimelineOrder::Published,
            thread_order: ThreadOrder::Tree,
            noindex: false,
            show_application: true,
            time_zone: None,
        }
    }
}

impl UserSettings {
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let language = self.posting_default_language.trim();
        self.posting_default_language = if language.is_empty() {
            "en".to_owned()
        } else {
            language.to_owned()
        };
        self.reading_translate_language = self
            .reading_translate_language
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned);
        self
    }

    #[must_use]
    pub fn resolved_visibility(&self, locked: bool) -> &'static str {
        self.posting_default_visibility.resolve(locked)
    }

    /// The target language for on-demand translation: the dedicated
    /// preference, falling back to the default posting language.
    #[must_use]
    pub fn translate_language(&self) -> &str {
        self.reading_translate_language
            .as_deref()
            .unwrap_or(&self.posting_default_language)
    }

    #[must_use]
    pub fn default_language(&self) -> Option<&str> {
        let language = self.posting_default_language.trim();
        (!language.is_empty()).then_some(language)
    }

    #[must_use]
    pub fn preferences_json(&self, locked: bool) -> Value {
        json!({
            "posting:default:visibility": self.resolved_visibility(locked),
            "posting:default:sensitive": self.posting_default_sensitive,
            "posting:default:language": self.posting_default_language,
            "posting:default:quote_policy": self.posting_default_quote_policy.as_str(),
            "reading:expand:media": self.reading_expand_media.as_str(),
            "reading:expand:spoilers": self.reading_expand_spoilers,
            "reading:autoplay:gifs": self.reading_autoplay_gifs,
            // Plamenu extension: Mastodon has no such key (its timelines are
            // always publish-time-ordered via snowflake ids).
            "reading:timeline:order": self.timeline_order.as_str(),
            // Plamenu extension: reply ordering in /context and the web
            // thread view ("tree" = Mastodon shape, "flat" = Pleroma shape).
            "reading:thread:order": self.thread_order.as_str(),
            // Plamenu extension: allow direct remote-media fetch when the
            // instance cannot proxy a file (privacy off).
            "reading:allow_direct_remote_media": self.reading_allow_direct_remote_media,
        })
    }
}

pub async fn create(
    pool: &PgPool,
    account_id: i64,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        -- CLI/admin-created accounts are confirmed on creation;
        -- `approved` defaults true. The role subselect grants the default
        -- "User" role, degrading to NULL if the seeded row was deleted.
        INSERT INTO users (id, account_id, email, password_hash, confirmed_at, role_id)
        VALUES ($1, $2, $3, $4, now(), (SELECT id FROM user_roles WHERE id = $5))
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        id::next(),
        account_id,
        email,
        password_hash,
        crate::role::DEFAULT_ROLE_ID,
    )
    .fetch_one(pool)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    Ok(user)
}

/// A self-service registration. With a confirmation token it starts
/// unconfirmed, carrying the token hash whose preimage was e-mailed to the
/// address; without one (no e-mail given, or no mail relay to send through)
/// there is nothing to confirm and the user starts confirmed.
pub struct NewRegisteredUser<'a> {
    pub account_id: i64,
    pub email: Option<&'a str>,
    pub password_hash: &'a str,
    /// Granted upfront in `open` registrations mode; `approved` mode leaves
    /// it false for the admin queue.
    pub approved: bool,
    pub confirmation_token_hash: Option<&'a str>,
    pub locale: Option<&'a str>,
    pub sign_up_ip: Option<&'a str>,
    pub created_by_application_id: i64,
    /// The `reason` field from approval-mode sign-up forms.
    pub invite_request_text: Option<&'a str>,
    /// The valid invite this sign-up came through; bumps the
    /// invite's `uses` counter atomically with the insert.
    pub invite_id: Option<i64>,
    /// The owner's IANA time zone, validated by the caller; `None`
    /// leaves the server default.
    pub time_zone: Option<&'a str>,
    /// When age verification passed; `Some(now)` when the server has a
    /// `min_age` gate and the sign-up cleared it, `None` when the gate is off.
    pub age_verified_at: Option<OffsetDateTime>,
}

pub async fn create_registered(pool: &PgPool, new: NewRegisteredUser<'_>) -> Result<User, DbError> {
    let mut tx = pool.begin().await?;
    let user = create_registered_conn(&mut tx, new).await?;
    tx.commit().await?;
    Ok(user)
}

/// [`create_registered`] against an already-open connection/transaction, so
/// registration can commit the user row (and its invite-use increment) in the
/// same transaction as the account, keys, and confirmation mail — a late
/// failure then rolls the whole signup back rather than leaving the username
/// and e-mail reserved by a userless account.
pub async fn create_registered_conn(
    conn: &mut sqlx::PgConnection,
    new: NewRegisteredUser<'_>,
) -> Result<User, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        -- The role subselect grants the default "User" role, degrading to
        -- NULL if the seeded row was deleted.
        INSERT INTO users (id, account_id, email, password_hash, approved,
                           confirmation_token_hash, confirmation_sent_at, confirmed_at,
                           locale, sign_up_ip, created_by_application_id,
                           invite_request_text, invite_id, time_zone, age_verified_at,
                           role_id)
        VALUES ($1, $2, $3, $4, $5, $6,
                CASE WHEN $6::text IS NULL THEN NULL ELSE now() END,
                CASE WHEN $6::text IS NULL THEN now() ELSE NULL END,
                $7, $8, $9, $10, $11, $12, $13,
                (SELECT id FROM user_roles WHERE id = $14))
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        id::next(),
        new.account_id,
        new.email,
        new.password_hash,
        new.approved,
        new.confirmation_token_hash,
        new.locale,
        new.sign_up_ip,
        new.created_by_application_id,
        new.invite_request_text,
        new.invite_id,
        new.time_zone,
        new.age_verified_at,
        crate::role::DEFAULT_ROLE_ID,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    if let Some(invite_id) = new.invite_id {
        sqlx::query!(
            "UPDATE invites SET uses = uses + 1 WHERE id = $1",
            invite_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(user)
}

/// Confirms the e-mail address behind a confirmation token (single-use: the
/// token is cleared). `grant_approval` re-checks the registration gate at
/// confirmation time — Mastodon's `grant_approval_on_confirmation?`, so a
/// sign-up from before the server switched to open registrations still gets
/// approved. Returns `None` for an unknown (or already used) token.
pub async fn confirm_by_token_hash(
    pool: &PgPool,
    token_hash: &str,
    grant_approval: bool,
) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET confirmed_at = now(),
            confirmation_token_hash = NULL,
            approved = approved OR $2
        WHERE confirmation_token_hash = $1
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        token_hash,
        grant_approval,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Replaces an unconfirmed user's confirmation token (and optionally the
/// address itself) for a resend — `POST /api/v1/emails/confirmations`.
/// Returns `None` when the user is already confirmed.
///
/// Prefer [`refresh_confirmation_token_with_mail`] for the resend paths that
/// also queue a confirmation mail; this bare variant is for the degenerate
/// address-less resend (nothing to deliver) and for tests.
pub async fn refresh_confirmation_token(
    pool: &PgPool,
    user_id: i64,
    token_hash: &str,
    new_email: Option<&str>,
) -> Result<Option<User>, DbError> {
    write_confirmation_token(pool, user_id, token_hash, new_email).await
}

/// Replaces an unconfirmed user's confirmation token (optionally the address)
/// and enqueues the confirmation mail in one transaction. Returns `None` —
/// enqueueing nothing — when the user is already
/// confirmed. If the mail row cannot commit, the token change rolls back, so
/// the previous confirmation link stays valid and retrying a failed resend
/// never destroys the last usable link.
pub async fn refresh_confirmation_token_with_mail(
    pool: &PgPool,
    user_id: i64,
    token_hash: &str,
    new_email: Option<&str>,
    mail: &crate::email::OutgoingEmail<'_>,
) -> Result<Option<User>, DbError> {
    let mut tx = pool.begin().await?;
    let Some(user) = write_confirmation_token(&mut *tx, user_id, token_hash, new_email).await?
    else {
        // Already confirmed: nothing changed, so drop the transaction without
        // queueing a mail.
        return Ok(None);
    };
    crate::email::enqueue_tx(&mut tx, mail).await?;
    tx.commit().await?;
    Ok(Some(user))
}

/// The shared confirmation-token UPDATE, usable against a pool or an open
/// transaction.
async fn write_confirmation_token<'e, E>(
    executor: E,
    user_id: i64,
    token_hash: &str,
    new_email: Option<&str>,
) -> Result<Option<User>, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET confirmation_token_hash = $2,
            confirmation_sent_at = now(),
            email = COALESCE($3, email)
        WHERE id = $1 AND confirmed_at IS NULL
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        user_id,
        token_hash,
        new_email,
    )
    .fetch_optional(executor)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    Ok(user)
}

/// Creates or replaces the login credentials for an account.
pub async fn set_credentials(
    pool: &PgPool,
    account_id: i64,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        -- Creating a login from scratch confirms it, like `create` does
        -- ("CLI/admin-created accounts are confirmed on creation"). The
        -- `DO UPDATE` arm deliberately leaves `confirmed_at` untouched so an
        -- operator password-reset can't confirm a pending self-registration.
        INSERT INTO users (id, account_id, email, password_hash, confirmed_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (account_id)
        -- An omitted e-mail keeps whatever address the login already has.
        DO UPDATE SET email = COALESCE(EXCLUDED.email, users.email),
                      password_hash = EXCLUDED.password_hash
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        id::next(),
        account_id,
        email,
        password_hash,
    )
    .fetch_one(pool)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    Ok(user)
}

/// Records a successful sign-in: advances the Devise trackable columns
/// (`current`/`last_sign_in_at`, `sign_in_count`), seeds `locale` from the
/// caller's Accept-Language when still unset, and appends a `login_activities`
/// row (the daily-unique source for the `active_users` measure + retention).
/// Mirrors Mastodon's sign-in tracking plus its `activity:logins` record.
pub async fn record_sign_in(
    pool: &PgPool,
    user_id: i64,
    locale: Option<&str>,
) -> Result<(), DbError> {
    record_sign_in_with_ip(pool, user_id, locale, None, SignInContext::default()).await
}

/// The factor that authenticated a sign-in and the client that made it — the
/// detail the authentication-history page shows beyond the bare daily count.
#[derive(Debug, Default, Clone)]
pub struct SignInContext<'a> {
    /// `password`, `otp` or `webauthn` (see the login-activity page); `None`
    /// leaves the column unset (e.g. background test seeding).
    pub method: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

/// Like [`record_sign_in`], additionally tracking the source IP for the admin
/// `user_ips` surface and the `ctx` detail for the authentication-history page.
/// A successful sign-in also clears failed-attempt lockout.
pub async fn record_sign_in_with_ip(
    pool: &PgPool,
    user_id: i64,
    locale: Option<&str>,
    ip: Option<&str>,
    ctx: SignInContext<'_>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"
        UPDATE users
        SET last_sign_in_at = current_sign_in_at,
            current_sign_in_at = now(),
            last_sign_in_ip = CASE
                WHEN $3::text IS NULL THEN last_sign_in_ip
                ELSE current_sign_in_ip
            END,
            current_sign_in_ip = COALESCE($3, current_sign_in_ip),
            sign_in_count = sign_in_count + 1,
            failed_attempts = 0,
            locked_at = NULL,
            locale = COALESCE(locale, $2)
        WHERE id = $1
        "#,
        user_id,
        locale,
        ip,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO login_activities
             (id, user_id, ip, authentication_method, success, user_agent)
         VALUES ($1, $2, $3, $4, true, $5)",
        id::next(),
        user_id,
        ip,
        ctx.method,
        ctx.user_agent,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Returns whether the user is inside the active timed lockout window.
pub async fn login_locked(pool: &PgPool, user_id: i64) -> Result<bool, DbError> {
    let locked = sqlx::query_scalar!(
        r#"
        SELECT locked_at IS NOT NULL
           AND locked_at > now() - ($2::int * interval '1 second') AS "locked!"
        FROM users
        WHERE id = $1
        "#,
        user_id,
        LOGIN_LOCK_WINDOW_SECONDS,
    )
    .fetch_optional(pool)
    .await?;
    Ok(locked.unwrap_or(false))
}

/// Records a bad password attempt. Returns `true` when the attempt has put the
/// account into the active lockout window.
pub async fn record_failed_sign_in(pool: &PgPool, user_id: i64) -> Result<bool, DbError> {
    let locked = sqlx::query_scalar!(
        r#"
        UPDATE users
        SET failed_attempts = CASE
                WHEN locked_at IS NOT NULL
                 AND locked_at <= now() - ($3::int * interval '1 second')
                THEN 1
                ELSE failed_attempts + 1
            END,
            locked_at = CASE
                WHEN locked_at IS NOT NULL
                 AND locked_at > now() - ($3::int * interval '1 second')
                THEN locked_at
                WHEN (
                    CASE
                        WHEN locked_at IS NOT NULL
                         AND locked_at <= now() - ($3::int * interval '1 second')
                        THEN 1
                        ELSE failed_attempts + 1
                    END
                ) >= $2
                THEN now()
                ELSE NULL
            END
        WHERE id = $1
        RETURNING locked_at IS NOT NULL
              AND locked_at > now() - ($3::int * interval '1 second') AS "locked!"
        "#,
        user_id,
        LOGIN_LOCK_MAX_ATTEMPTS,
        LOGIN_LOCK_WINDOW_SECONDS,
    )
    .fetch_optional(pool)
    .await?;
    Ok(locked.unwrap_or(false))
}

/// Stores a freshly generated (encrypted) TOTP secret without requiring it
/// at login yet — the enrollment state while the user confirms a code, like
/// Mastodon's provisional `otp_secret`.
pub async fn set_otp_secret(pool: &PgPool, user_id: i64, encrypted: &str) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users
         SET otp_secret = $2, otp_required_for_login = false, otp_consumed_timestep = NULL
         WHERE id = $1",
        user_id,
        encrypted,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Flips the login requirement on once the user proved their authenticator
/// works. Returns `false` when there is no provisional secret to enable.
pub async fn enable_otp(pool: &PgPool, user_id: i64) -> Result<bool, DbError> {
    let enabled = sqlx::query!(
        "UPDATE users SET otp_required_for_login = true
         WHERE id = $1 AND otp_secret IS NOT NULL",
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(enabled.rows_affected() == 1)
}

/// Clears every trace of 2FA: TOTP secret, requirement, replay state, the
/// recovery codes and — since `WebAuthn` keys are gated behind TOTP being on
/// (Mastodon's model) — every registered security key.
pub async fn disable_otp(pool: &PgPool, user_id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE users
         SET otp_secret = NULL, otp_required_for_login = false,
             otp_consumed_timestep = NULL
         WHERE id = $1",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM otp_backup_codes WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE user_id = $1",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Claims a TOTP timestep so the same code cannot be replayed inside its
/// drift window. Returns `false` when the step was already consumed.
pub async fn consume_otp_timestep(
    pool: &PgPool,
    user_id: i64,
    timestep: i64,
) -> Result<bool, DbError> {
    let claimed = sqlx::query!(
        "UPDATE users SET otp_consumed_timestep = $2
         WHERE id = $1
           AND (otp_consumed_timestep IS NULL OR otp_consumed_timestep < $2)",
        user_id,
        timestep,
    )
    .execute(pool)
    .await?;
    Ok(claimed.rows_affected() == 1)
}

pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        FROM users
        WHERE lower(email) = lower($1)
        "#,
        email,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// The application a self-registered user signed up through
/// (`users.created_by_application_id`) — gate for the confirmation-resend
/// endpoint. `None` for CLI-created users.
pub async fn created_by_application_id(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<i64>, DbError> {
    let app_id = sqlx::query_scalar!(
        "SELECT created_by_application_id FROM users WHERE id = $1",
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(app_id.flatten())
}

pub async fn find_by_account_id(pool: &PgPool, account_id: i64) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        FROM users
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

pub async fn find_by_id(pool: &PgPool, user_id: i64) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        FROM users
        WHERE id = $1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Sets a local user's login gate, matching Mastodon's user disable/enable
/// actions. Returns
/// whether a row changed.
pub async fn set_disabled(pool: &PgPool, account_id: i64, disabled: bool) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE users SET disabled = $2 WHERE account_id = $1",
        account_id,
        disabled,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Approves a pending local user. E-mail confirmation is
/// deliberately separate: an unconfirmed-but-approved user still has
/// to click their link. Returns whether this call made the transition — an
/// already-approved user is left alone, so callers can run the
/// became-functional side effects exactly once.
pub async fn approve(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE users
         SET approved = true
         WHERE account_id = $1 AND NOT approved",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Destroys the login behind an account (self-deletion): cascades
/// every token, grant, marker and push subscription, and frees the e-mail
/// address for re-use — Mastodon deletes self-served accounts with
/// `reserve_email: false`. Returns whether a login existed.
pub async fn delete_by_account_id<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM users WHERE account_id = $1", account_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Mastodon's show-application preference (whether the posting app is shown
/// on statuses) for a render batch.
/// Accounts without a local user row (remote accounts) are absent and therefore
/// treated as false by callers.
pub async fn show_application_by_account_ids(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, bool>, DbError> {
    if account_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT account_id, show_application
        FROM users
        WHERE account_id = ANY($1)
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.show_application))
        .collect())
}

pub async fn settings_by_user_id(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<UserSettings>, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT posting_default_visibility, posting_default_sensitive,
               posting_default_language, posting_default_quote_policy,
               posting_default_content_type, reading_expand_media,
               reading_expand_spoilers, reading_autoplay_gifs,
               reading_allow_direct_remote_media, reading_collapse_boosts,
               reading_translate_language,
               timeline_order,
               thread_order, noindex, show_application, time_zone
        FROM users
        WHERE id = $1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| UserSettings {
        posting_default_visibility: PostingDefaultVisibility::parse(
            &row.posting_default_visibility,
        ),
        posting_default_sensitive: row.posting_default_sensitive,
        posting_default_language: row.posting_default_language,
        posting_default_quote_policy: DefaultQuotePolicy::parse(&row.posting_default_quote_policy),
        posting_default_content_type: PostingDefaultFormat::parse(
            &row.posting_default_content_type,
        ),
        reading_expand_media: ReadingExpandMedia::parse(&row.reading_expand_media),
        reading_expand_spoilers: row.reading_expand_spoilers,
        reading_autoplay_gifs: row.reading_autoplay_gifs,
        reading_allow_direct_remote_media: row.reading_allow_direct_remote_media,
        reading_collapse_boosts: row.reading_collapse_boosts,
        reading_translate_language: row.reading_translate_language,
        timeline_order: TimelineOrder::parse(&row.timeline_order),
        thread_order: ThreadOrder::parse(&row.thread_order),
        noindex: row.noindex,
        show_application: row.show_application,
        time_zone: row.time_zone,
    }))
}

pub async fn settings_by_account_id(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<UserSettings>, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT posting_default_visibility, posting_default_sensitive,
               posting_default_language, posting_default_quote_policy,
               posting_default_content_type, reading_expand_media,
               reading_expand_spoilers, reading_autoplay_gifs,
               reading_allow_direct_remote_media, reading_collapse_boosts,
               reading_translate_language,
               timeline_order,
               thread_order, noindex, show_application, time_zone
        FROM users
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| UserSettings {
        posting_default_visibility: PostingDefaultVisibility::parse(
            &row.posting_default_visibility,
        ),
        posting_default_sensitive: row.posting_default_sensitive,
        posting_default_language: row.posting_default_language,
        posting_default_quote_policy: DefaultQuotePolicy::parse(&row.posting_default_quote_policy),
        posting_default_content_type: PostingDefaultFormat::parse(
            &row.posting_default_content_type,
        ),
        reading_expand_media: ReadingExpandMedia::parse(&row.reading_expand_media),
        reading_expand_spoilers: row.reading_expand_spoilers,
        reading_autoplay_gifs: row.reading_autoplay_gifs,
        reading_allow_direct_remote_media: row.reading_allow_direct_remote_media,
        reading_collapse_boosts: row.reading_collapse_boosts,
        reading_translate_language: row.reading_translate_language,
        timeline_order: TimelineOrder::parse(&row.timeline_order),
        thread_order: ThreadOrder::parse(&row.thread_order),
        noindex: row.noindex,
        show_application: row.show_application,
        time_zone: row.time_zone,
    }))
}

/// The Account-entity attributes only a local account carries (Mastodon's
/// `if: :local?` serializer branch): the owner's `noindex` preference and
/// their role when it is publicly highlighted (`roles` shows highlighted
/// roles only). `None` when no `users` row backs the account.
#[derive(Debug, Clone)]
pub struct AccountEntityOverlay {
    pub noindex: bool,
    pub highlighted_role: Option<HighlightedRole>,
}

/// The subset of a role the public Account entity's `roles` carries.
#[derive(Debug, Clone)]
pub struct HighlightedRole {
    pub id: i64,
    pub name: String,
    pub color: String,
}

pub async fn account_entity_overlay(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<AccountEntityOverlay>, DbError> {
    Ok(account_entity_overlay_batch(pool, &[account_id])
        .await?
        .remove(&account_id))
}

/// [`account_entity_overlay`] for a batch of local accounts in one query — ids
/// with no `users` row (remote accounts) are simply absent from the map, so a
/// page of accounts costs a single round trip instead of one per local row.
pub async fn account_entity_overlay_batch(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<HashMap<i64, AccountEntityOverlay>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT u.account_id AS "account_id!",
               u.noindex AS "noindex!",
               r.id AS "role_id?", r.name AS "role_name?", r.color AS "role_color?"
        FROM users u
        LEFT JOIN user_roles r ON r.id = u.role_id AND r.highlighted
        WHERE u.account_id = ANY($1)
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.account_id,
                AccountEntityOverlay {
                    noindex: row.noindex,
                    highlighted_role: match (row.role_id, row.role_name, row.role_color) {
                        (Some(id), Some(name), Some(color)) => {
                            Some(HighlightedRole { id, name, color })
                        }
                        _ => None,
                    },
                },
            )
        })
        .collect())
}

pub async fn update_settings(
    pool: &PgPool,
    user_id: i64,
    settings: UserSettings,
) -> Result<Option<UserSettings>, DbError> {
    let settings = settings.sanitized();
    let row = sqlx::query!(
        r#"
        UPDATE users
        SET posting_default_visibility = $2,
            posting_default_sensitive = $3,
            posting_default_language = $4,
            posting_default_quote_policy = $5,
            posting_default_content_type = $6,
            reading_expand_media = $7,
            reading_expand_spoilers = $8,
            reading_autoplay_gifs = $9,
            reading_allow_direct_remote_media = $10,
            timeline_order = $11,
            thread_order = $12,
            noindex = $13,
            show_application = $14,
            reading_translate_language = $15,
            reading_collapse_boosts = $16
        WHERE id = $1
        RETURNING posting_default_visibility, posting_default_sensitive,
                  posting_default_language, posting_default_quote_policy,
                  posting_default_content_type, reading_expand_media,
                  reading_expand_spoilers, reading_autoplay_gifs,
                  reading_allow_direct_remote_media, reading_collapse_boosts,
                  reading_translate_language,
                  timeline_order,
                  thread_order, noindex, show_application, time_zone
        "#,
        user_id,
        settings.posting_default_visibility.as_str(),
        settings.posting_default_sensitive,
        settings.posting_default_language,
        settings.posting_default_quote_policy.as_str(),
        settings.posting_default_content_type.as_str(),
        settings.reading_expand_media.as_str(),
        settings.reading_expand_spoilers,
        settings.reading_autoplay_gifs,
        settings.reading_allow_direct_remote_media,
        settings.timeline_order.as_str(),
        settings.thread_order.as_str(),
        settings.noindex,
        settings.show_application,
        settings.reading_translate_language,
        settings.reading_collapse_boosts,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| UserSettings {
        posting_default_visibility: PostingDefaultVisibility::parse(
            &row.posting_default_visibility,
        ),
        posting_default_sensitive: row.posting_default_sensitive,
        posting_default_language: row.posting_default_language,
        posting_default_quote_policy: DefaultQuotePolicy::parse(&row.posting_default_quote_policy),
        posting_default_content_type: PostingDefaultFormat::parse(
            &row.posting_default_content_type,
        ),
        reading_expand_media: ReadingExpandMedia::parse(&row.reading_expand_media),
        reading_expand_spoilers: row.reading_expand_spoilers,
        reading_autoplay_gifs: row.reading_autoplay_gifs,
        reading_allow_direct_remote_media: row.reading_allow_direct_remote_media,
        reading_collapse_boosts: row.reading_collapse_boosts,
        reading_translate_language: row.reading_translate_language,
        timeline_order: TimelineOrder::parse(&row.timeline_order),
        thread_order: ThreadOrder::parse(&row.thread_order),
        noindex: row.noindex,
        show_application: row.show_application,
        time_zone: row.time_zone,
    }))
}

/// Whether the account owner allows direct remote-media fetch as a
/// last-resort fallback (`reading_allow_direct_remote_media`). The media
/// serializers consult this per viewer to decide whether their proxy URLs may
/// fall through to the origin. A remote account (no `users` row) is `false` —
/// only a local viewer with the setting on gets the fallback marker.
pub async fn allows_direct_remote_media(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let row = sqlx::query!(
        "SELECT reading_allow_direct_remote_media FROM users WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some_and(|row| row.reading_allow_direct_remote_media))
}

/// [`allows_direct_remote_media`] across a set of viewers in one query — the
/// subset of `account_ids` whose owner opted into the direct-fetch fallback.
pub async fn allows_direct_remote_media_of(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT account_id FROM users
           WHERE account_id = ANY($1) AND reading_allow_direct_remote_media"#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The set of languages the user posts in — the web composer offers
/// only these in its language selector. `None` means no restriction (the
/// full inventory), for users who never touched the checklist.
pub async fn posting_languages(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<Vec<String>>, DbError> {
    let row = sqlx::query!("SELECT posting_languages FROM users WHERE id = $1", user_id,)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.posting_languages))
}

/// Stores the posting-language set; `None` or an empty set clears the
/// restriction (a composer offering zero languages would be unusable).
/// Returns whether the user existed.
pub async fn update_posting_languages(
    pool: &PgPool,
    user_id: i64,
    languages: Option<&[String]>,
) -> Result<bool, DbError> {
    let languages = languages.filter(|set| !set.is_empty());
    let done = sqlx::query!(
        "UPDATE users SET posting_languages = $2 WHERE id = $1",
        user_id,
        languages,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The reading-side language filter — Mastodon's `chosen_languages`.
/// The home timeline drops posts whose `language` isn't in this set. `None`
/// (the default) means no filter: every language reaches the feed.
pub async fn chosen_languages(pool: &PgPool, user_id: i64) -> Result<Option<Vec<String>>, DbError> {
    let row = sqlx::query!("SELECT chosen_languages FROM users WHERE id = $1", user_id,)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.chosen_languages))
}

/// Stores the reading-side language filter; `None` or an empty set clears it
/// (an empty filter would hide the whole timeline). Returns whether the user
/// existed.
pub async fn update_chosen_languages(
    pool: &PgPool,
    user_id: i64,
    languages: Option<&[String]>,
) -> Result<bool, DbError> {
    let languages = languages.filter(|set| !set.is_empty());
    let done = sqlx::query!(
        "UPDATE users SET chosen_languages = $2 WHERE id = $1",
        user_id,
        languages,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The owner's preferred IANA time zone; `None` means the server
/// default. Validation of the identifier is the caller's job (see the
/// server's `time_zones` inventory).
pub async fn time_zone(pool: &PgPool, user_id: i64) -> Result<Option<String>, DbError> {
    let row = sqlx::query!("SELECT time_zone FROM users WHERE id = $1", user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.time_zone))
}

/// The owner's preferred zone, looked up by account rather than user id — for
/// paths that only carry the account (the scheduled-status quota).
pub async fn time_zone_by_account_id(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<String>, DbError> {
    let row = sqlx::query!(
        "SELECT time_zone FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|row| row.time_zone))
}

/// Stores the owner's time zone; `None` (or an empty string) clears it back to
/// the server default. Returns whether the user existed.
pub async fn update_time_zone(
    pool: &PgPool,
    user_id: i64,
    time_zone: Option<&str>,
) -> Result<bool, DbError> {
    let time_zone = time_zone.map(str::trim).filter(|tz| !tz.is_empty());
    let done = sqlx::query!(
        "UPDATE users SET time_zone = $2 WHERE id = $1",
        user_id,
        time_zone,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn locale(pool: &PgPool, user_id: i64) -> Result<Option<String>, DbError> {
    let row = sqlx::query!("SELECT locale FROM users WHERE id = $1", user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.locale))
}

/// The request-wide preferences every rendered page and its response policy
/// need — interface locale, display time zone, and whether this viewer allows
/// direct remote media — in one round-trip. They live in the same row;
/// splitting the CSP choice into another query would add cost to every signed-
/// in page.
pub async fn web_preferences(
    pool: &PgPool,
    user_id: i64,
) -> Result<(Option<String>, Option<String>, bool), DbError> {
    let row = sqlx::query!(
        "SELECT locale, time_zone, reading_allow_direct_remote_media
         FROM users WHERE id = $1",
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some(row) => (
            row.locale,
            row.time_zone,
            row.reading_allow_direct_remote_media,
        ),
        None => (None, None, false),
    })
}

/// Stores the owner's interface locale; `None` (or an empty string) clears it
/// so the next sign-in re-seeds from `Accept-Language`. Returns whether the
/// user existed.
pub async fn update_locale(
    pool: &PgPool,
    user_id: i64,
    locale: Option<&str>,
) -> Result<bool, DbError> {
    let locale = locale.map(str::trim).filter(|l| !l.is_empty());
    let done = sqlx::query!(
        "UPDATE users SET locale = $2 WHERE id = $1",
        user_id,
        locale,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The receiving account's stored locale, for locale-tagged deliveries like
/// Web Push payloads. `None` when the account has no local user or never had
/// a locale seeded.
pub async fn locale_by_account_id(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<String>, DbError> {
    let row = sqlx::query!("SELECT locale FROM users WHERE account_id = $1", account_id,)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.locale))
}

/// The new-IP sign-in alert opt-in and the address to notify. `None`
/// when the user is unknown; `email` is `None` for an account without one (the
/// alert then can't be sent).
pub struct NewIpAlertPrefs {
    pub enabled: bool,
    pub email: Option<String>,
}

pub async fn new_ip_alert_prefs(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<NewIpAlertPrefs>, DbError> {
    let row = sqlx::query!(
        "SELECT new_ip_sign_in_alert, email FROM users WHERE id = $1",
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| NewIpAlertPrefs {
        enabled: row.new_ip_sign_in_alert,
        email: row.email,
    }))
}

/// Reads the new-IP alert opt-in for rendering the security-settings toggle.
pub async fn new_ip_sign_in_alert(pool: &PgPool, user_id: i64) -> Result<bool, DbError> {
    let enabled = sqlx::query_scalar!(
        "SELECT new_ip_sign_in_alert FROM users WHERE id = $1",
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(enabled.unwrap_or(false))
}

/// Sets the new-IP alert opt-in. Returns whether the user existed.
pub async fn set_new_ip_sign_in_alert(
    pool: &PgPool,
    user_id: i64,
    enabled: bool,
) -> Result<bool, DbError> {
    let done = sqlx::query!(
        "UPDATE users SET new_ip_sign_in_alert = $2 WHERE id = $1",
        user_id,
        enabled,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Stores a fresh reset-password token for the user (replacing any earlier
/// one). The preimage goes into the e-mailed link.
///
/// Prefer [`set_reset_password_token_with_mail`] for `POST /auth/password` and
/// the admin reset, which must queue the reset mail atomically with this
/// update; this bare variant is used by tests.
pub async fn set_reset_password_token(
    pool: &PgPool,
    user_id: i64,
    token_hash: &str,
) -> Result<(), DbError> {
    write_reset_password_token(pool, user_id, token_hash).await
}

/// Stores a fresh reset-password token and enqueues the reset mail in one
/// transaction. If the mail row cannot commit, the token
/// update rolls back and any previous reset link stays valid — so retrying a
/// failed request never invalidates the last usable link before the
/// replacement is durably queued.
pub async fn set_reset_password_token_with_mail(
    pool: &PgPool,
    user_id: i64,
    token_hash: &str,
    mail: &crate::email::OutgoingEmail<'_>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    write_reset_password_token(&mut *tx, user_id, token_hash).await?;
    crate::email::enqueue_tx(&mut tx, mail).await?;
    tx.commit().await?;
    Ok(())
}

/// The shared reset-token UPDATE, usable against a pool or an open transaction.
async fn write_reset_password_token<'e, E>(
    executor: E,
    user_id: i64,
    token_hash: &str,
) -> Result<(), DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE users
        SET reset_password_token_hash = $2,
            reset_password_sent_at = now()
        WHERE id = $1
        "#,
        user_id,
        token_hash,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// The user behind a live (existing and unexpired) reset-password token —
/// validates the e-mailed link before showing the new-password form. The
/// 6-hour window is Devise's `reset_password_within`, which Mastodon keeps.
pub async fn find_by_reset_password_token_hash(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        FROM users
        WHERE reset_password_token_hash = $1
          AND reset_password_sent_at > now() - interval '6 hours'
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Sets the password behind a live reset token (single-use: the token is
/// cleared). `None` for an unknown, already-used or expired token.
pub async fn reset_password_by_token_hash(
    pool: &PgPool,
    token_hash: &str,
    password_hash: &str,
) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET password_hash = $2,
            reset_password_token_hash = NULL,
            reset_password_sent_at = NULL
        WHERE reset_password_token_hash = $1
          AND reset_password_sent_at > now() - interval '6 hours'
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        token_hash,
        password_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Changes a signed-in user's password *and* revokes every live token in one
/// transaction. A bare update-then-revoke leaves a window
/// where the password has changed but the sign-out-everywhere revocation can
/// still fail — stranding a copied token the change was meant to kill. Committing
/// both together closes that window: either the new password and the revocation
/// both take, or neither does. `None` when the user id does not exist. The caller
/// rotates the acting browser onto a fresh token *after* this commits.
pub async fn change_password_and_revoke(
    pool: &PgPool,
    user_id: i64,
    password_hash: &str,
) -> Result<Option<User>, DbError> {
    let mut tx = pool.begin().await?;
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET password_hash = $2
        WHERE id = $1
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        user_id,
        password_hash,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if user.is_some() {
        crate::oauth::revoke_all_for_user_conn(&mut *tx, user_id).await?;
    }
    tx.commit().await?;
    Ok(user)
}

/// Sets the password behind a live reset token *and* signs the account out
/// everywhere, in one transaction. The token-guarded password
/// UPDATE, the single-use token consumption, and the sign-out-everywhere
/// revocation commit or roll back together, so a self-service reset can never
/// change the password and consume the link while leaving a copied token live.
/// `None` for an unknown, already-used or expired token (like
/// [`reset_password_by_token_hash`], which this replaces on the self-service
/// path).
pub async fn reset_password_by_token_and_revoke(
    pool: &PgPool,
    token_hash: &str,
    password_hash: &str,
) -> Result<Option<User>, DbError> {
    let mut tx = pool.begin().await?;
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET password_hash = $2,
            reset_password_token_hash = NULL,
            reset_password_sent_at = NULL
        WHERE reset_password_token_hash = $1
          AND reset_password_sent_at > now() - interval '6 hours'
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        token_hash,
        password_hash,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(user) = &user {
        crate::oauth::revoke_all_for_user_conn(&mut *tx, user.id).await?;
    }
    tx.commit().await?;
    Ok(user)
}

/// Sets an account's credentials, revokes every live token, and (when `audit`
/// is present) appends the moderator's audit line — all in one transaction (QC
/// audit #37). The admin "set generated password" and CLI `account passwd`
/// paths promise "the password changed and every session was signed out"; doing
/// the credential write and the revocation as separate statements can leave a
/// stolen token live after the password already changed, and — for the admin
/// path — record (or fail to record) the action out of step with the change.
/// The generated one-time password is shown only after this commits. Behaves
/// like [`set_credentials`] otherwise (upsert on `account_id`, `EmailTaken` on a
/// duplicate address).
pub async fn set_credentials_and_revoke(
    pool: &PgPool,
    account_id: i64,
    email: Option<&str>,
    password_hash: &str,
    audit: Option<crate::admin_action_log::NewActionLog<'_>>,
) -> Result<User, DbError> {
    let mut tx = pool.begin().await?;
    let user = sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (id, account_id, email, password_hash, confirmed_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (account_id)
        DO UPDATE SET email = COALESCE(EXCLUDED.email, users.email),
                      password_hash = EXCLUDED.password_hash
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        id::next(),
        account_id,
        email,
        password_hash,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    crate::oauth::revoke_all_for_user_conn(&mut *tx, user.id).await?;
    if let Some(audit) = audit {
        crate::admin_action_log::record_tx(&mut tx, audit).await?;
    }
    tx.commit().await?;
    Ok(user)
}

/// The admin "reset password" transition (Mastodon's `User#reset_password!`) as
/// one transaction: scramble the current password, revoke every
/// live token, store the fresh reset token, and durably enqueue the reset mail.
/// A bare four-step sequence can lock the target out — the old password gone and
/// its tokens dead — while the mail that delivers the way back in never commits.
/// Here the whole transition is atomic: either the user is signed out with a
/// durably-queued reset link, or nothing changed. The mail is rendered by the
/// caller before the transaction so no work happens across the commit.
pub async fn admin_reset_password(
    pool: &PgPool,
    user_id: i64,
    scrambled_hash: &str,
    reset_token_hash: &str,
    mail: &crate::email::OutgoingEmail<'_>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE users SET password_hash = $2 WHERE id = $1",
        user_id,
        scrambled_hash,
    )
    .execute(&mut *tx)
    .await?;
    crate::oauth::revoke_all_for_user_conn(&mut *tx, user_id).await?;
    write_reset_password_token(&mut *tx, user_id, reset_token_hash).await?;
    crate::email::enqueue_tx(&mut tx, mail).await?;
    tx.commit().await?;
    Ok(())
}

/// Sets or clears the e-mail address (`None` removes it, dropping password
/// reset and e-mail login until a new one is set).
pub async fn update_email(
    pool: &PgPool,
    user_id: i64,
    email: Option<&str>,
) -> Result<Option<User>, DbError> {
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET email = $2
        WHERE id = $1
        RETURNING id, account_id, email, password_hash, disabled, approved, confirmed_at, created_at,
                  otp_secret, otp_required_for_login, otp_consumed_timestep
        "#,
        user_id,
        email,
    )
    .fetch_optional(pool)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::EmailTaken,
        _ => DbError::Sqlx(err),
    })?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local_account(pool: &PgPool) -> i64 {
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

    #[sqlx::test]
    async fn create_and_find_user(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let created = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();
        let by_email = find_by_email(&pool, "ALICE@EXAMPLE.COM")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_email.id, created.id);
        assert_eq!(by_email.account_id, account_id);
        let by_id = find_by_id(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(by_id.email.as_deref(), Some("alice@example.com"));

        // Duplicate email (case-insensitively) is rejected.
        assert!(
            create(&pool, account_id, Some("Alice@example.com"), "h")
                .await
                .is_err()
        );
        assert!(
            find_by_email(&pool, "nobody@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn set_credentials_creates_then_replaces(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let created = set_credentials(&pool, account_id, Some("alice@example.com"), "$argon2id$1")
            .await
            .unwrap();
        assert_eq!(created.account_id, account_id);
        // The INSERT arm mints a confirmed login (like `create`), so a
        // freshly password-set account isn't stuck at the OAuth confirmation
        // gate.
        assert!(created.confirmed());

        // Upserting the same account replaces email and hash, keeping one row.
        let replaced = set_credentials(&pool, account_id, Some("alice@new.example"), "$argon2id$2")
            .await
            .unwrap();
        assert_eq!(replaced.account_id, account_id);
        assert_eq!(replaced.password_hash, "$argon2id$2");
        // The UPDATE arm leaves confirmation state alone; an already-confirmed
        // login stays confirmed.
        assert!(replaced.confirmed());
        assert!(
            find_by_email(&pool, "alice@example.com")
                .await
                .unwrap()
                .is_none()
        );
        let by_email = find_by_email(&pool, "alice@new.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_email.account_id, account_id);

        // An omitted e-mail keeps the stored address (password-only change).
        let kept = set_credentials(&pool, account_id, None, "$argon2id$3")
            .await
            .unwrap();
        assert_eq!(kept.email.as_deref(), Some("alice@new.example"));
        assert_eq!(kept.password_hash, "$argon2id$3");
    }

    /// An operator `account passwd` reset (the UPDATE arm) must not confirm a
    /// pending self-registration — otherwise a password reset would smuggle an
    /// unverified e-mail past the confirmation gate.
    #[sqlx::test]
    async fn set_credentials_update_arm_keeps_pending_registration_unconfirmed(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let app = crate::oauth::create_app(
            &pool,
            crate::oauth::NewApp {
                name: "test",
                website: None,
                client_id: "cid",
                client_secret_hash: "hash",
                redirect_uris: &[],
                scopes: "read",
            },
        )
        .await
        .unwrap();
        // A sign-up carrying a confirmation token starts unconfirmed.
        let pending = create_registered(
            &pool,
            NewRegisteredUser {
                account_id,
                email: Some("alice@example.com"),
                password_hash: "$argon2id$old",
                approved: true,
                confirmation_token_hash: Some("tokenhash"),
                locale: None,
                sign_up_ip: None,
                created_by_application_id: app.id,
                invite_request_text: None,
                invite_id: None,
                time_zone: None,
                age_verified_at: None,
            },
        )
        .await
        .unwrap();
        assert!(!pending.confirmed());

        // An operator password reset hits the UPDATE arm; confirmation state
        // is untouched, so the registration is still pending.
        let reset = set_credentials(&pool, account_id, None, "$argon2id$new")
            .await
            .unwrap();
        assert_eq!(reset.password_hash, "$argon2id$new");
        assert!(!reset.confirmed());
    }

    #[sqlx::test]
    async fn email_is_optional_and_clearable(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let created = create(&pool, account_id, None, "$argon2id$x")
            .await
            .unwrap();
        assert_eq!(created.email, None);
        assert!(created.confirmed());

        // Two e-mail-less users don't collide on the unique index.
        let other_account = account::create_local(
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
        create(&pool, other_account, None, "$argon2id$y")
            .await
            .unwrap();

        // An address can be added later, then removed again.
        let with_email = update_email(&pool, created.id, Some("alice@example.com"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with_email.email.as_deref(), Some("alice@example.com"));
        let cleared = update_email(&pool, created.id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared.email, None);
        assert!(
            find_by_email(&pool, "alice@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn registration_without_confirmation_token_starts_confirmed(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let app = crate::oauth::create_app(
            &pool,
            crate::oauth::NewApp {
                name: "test",
                website: None,
                client_id: "cid",
                client_secret_hash: "hash",
                redirect_uris: &[],
                scopes: "read",
            },
        )
        .await
        .unwrap();
        let user = create_registered(
            &pool,
            NewRegisteredUser {
                account_id,
                email: None,
                password_hash: "$argon2id$x",
                approved: true,
                confirmation_token_hash: None,
                locale: None,
                sign_up_ip: None,
                created_by_application_id: app.id,
                invite_request_text: None,
                invite_id: None,
                time_zone: None,
                age_verified_at: None,
            },
        )
        .await
        .unwrap();
        assert!(user.confirmed());
        assert!(user.functional());
        assert_eq!(user.email, None);
    }

    #[sqlx::test]
    async fn reset_password_token_is_single_use_and_expires(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();

        set_reset_password_token(&pool, user.id, "hash1")
            .await
            .unwrap();
        // A resend replaces the earlier token.
        set_reset_password_token(&pool, user.id, "hash2")
            .await
            .unwrap();
        assert!(
            find_by_reset_password_token_hash(&pool, "hash1")
                .await
                .unwrap()
                .is_none()
        );
        let found = find_by_reset_password_token_hash(&pool, "hash2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, user.id);

        let reset = reset_password_by_token_hash(&pool, "hash2", "$argon2id$new")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset.password_hash, "$argon2id$new");
        // Single use: the token is gone.
        assert!(
            reset_password_by_token_hash(&pool, "hash2", "$argon2id$again")
                .await
                .unwrap()
                .is_none()
        );

        // An expired token (older than Devise's 6-hour window) is dead.
        set_reset_password_token(&pool, user.id, "hash3")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET reset_password_sent_at = now() - interval '7 hours' WHERE id = $1",
            user.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            find_by_reset_password_token_hash(&pool, "hash3")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            reset_password_by_token_hash(&pool, "hash3", "$argon2id$late")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The reset token and its mail must commit or roll
    /// back as one unit, so a failed enqueue never invalidates the previous
    /// link before the replacement is durably queued.
    #[sqlx::test]
    async fn reset_token_and_mail_commit_or_roll_back_together(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();

        // A successful request commits the token and the mail together.
        let good = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Reset password",
            body: "https://example.test/auth/password/edit?reset_password_token=t1",
        };
        set_reset_password_token_with_mail(&pool, user.id, "hash1", &good)
            .await
            .unwrap();
        assert!(
            find_by_reset_password_token_hash(&pool, "hash1")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);

        // A late enqueue failure — a NUL byte is rejected by the text column,
        // standing in for any failure inserting the mail row — must roll back
        // the token update, so no new mail is queued and the previous link
        // stays valid.
        let poison = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Reset password",
            body: "link\0with-nul",
        };
        assert!(
            set_reset_password_token_with_mail(&pool, user.id, "hash2", &poison)
                .await
                .is_err()
        );
        // hash1 is still the live token; hash2 never took; nothing new queued.
        assert!(
            find_by_reset_password_token_hash(&pool, "hash1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            find_by_reset_password_token_hash(&pool, "hash2")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);
    }

    /// The confirmation-token rotation and its mail must
    /// commit or roll back together, and an already-confirmed resend must
    /// rotate nothing and queue nothing.
    #[sqlx::test]
    async fn confirmation_token_and_mail_commit_or_roll_back_together(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let app = crate::oauth::create_app(
            &pool,
            crate::oauth::NewApp {
                name: "test",
                website: None,
                client_id: "cid",
                client_secret_hash: "hash",
                redirect_uris: &[],
                scopes: "read",
            },
        )
        .await
        .unwrap();
        // A sign-up carrying a confirmation token starts unconfirmed.
        let user = create_registered(
            &pool,
            NewRegisteredUser {
                account_id,
                email: Some("alice@example.com"),
                password_hash: "$argon2id$old",
                approved: true,
                confirmation_token_hash: Some("c1"),
                locale: None,
                sign_up_ip: None,
                created_by_application_id: app.id,
                invite_request_text: None,
                invite_id: None,
                time_zone: None,
                age_verified_at: None,
            },
        )
        .await
        .unwrap();
        assert!(!user.confirmed());

        // A successful resend rotates the token to c2 and queues one mail.
        let good = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Confirm",
            body: "https://example.test/auth/confirmation?confirmation_token=c2",
        };
        assert!(
            refresh_confirmation_token_with_mail(&pool, user.id, "c2", None, &good)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);

        // A late enqueue failure rolls back the rotation: c3 never takes, the
        // previous link (c2) survives, and no extra mail is queued.
        let poison = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Confirm",
            body: "link\0with-nul",
        };
        assert!(
            refresh_confirmation_token_with_mail(&pool, user.id, "c3", None, &poison)
                .await
                .is_err()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);
        assert!(
            confirm_by_token_hash(&pool, "c3", false)
                .await
                .unwrap()
                .is_none()
        );
        let confirmed = confirm_by_token_hash(&pool, "c2", false)
            .await
            .unwrap()
            .expect("the previous confirmation link is still live");
        assert!(confirmed.confirmed());

        // Once confirmed, a further resend rotates nothing and queues no mail.
        assert!(
            refresh_confirmation_token_with_mail(&pool, user.id, "c4", None, &good)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);
    }

    /// An app to mint test tokens against.
    async fn test_app(pool: &PgPool) -> crate::oauth::App {
        crate::oauth::create_app(
            pool,
            crate::oauth::NewApp {
                name: "test",
                website: None,
                client_id: "cid",
                client_secret_hash: "hash",
                redirect_uris: &[],
                scopes: "read write",
            },
        )
        .await
        .unwrap()
    }

    /// How many un-revoked tokens the user still has.
    async fn live_tokens(pool: &PgPool, user_id: i64) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "c!" FROM oauth_tokens
               WHERE user_id = $1 AND revoked_at IS NULL"#,
            user_id,
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A signed-in password change and its sign-out-everywhere
    /// revocation must commit together, so there is no window where the password
    /// changed but a stolen token survives a failed revocation.
    #[sqlx::test]
    async fn change_password_and_revoke_signs_out_everywhere(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();
        let app = test_app(&pool).await;
        crate::oauth::create_token(&pool, "sess1", app.id, Some(user.id), "read")
            .await
            .unwrap();
        crate::oauth::create_token(&pool, "sess2", app.id, Some(user.id), "read")
            .await
            .unwrap();
        assert_eq!(live_tokens(&pool, user.id).await, 2);

        let changed = change_password_and_revoke(&pool, user.id, "$argon2id$new")
            .await
            .unwrap()
            .expect("the user exists");
        assert_eq!(changed.password_hash, "$argon2id$new");
        // Both tokens are revoked in the same transaction as the change.
        assert_eq!(live_tokens(&pool, user.id).await, 0);

        // A non-existent user changes nothing and reports it.
        assert!(
            change_password_and_revoke(&pool, user.id + 9_999, "$argon2id$x")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The self-service reset sets the password behind the live
    /// token *and* signs the account out everywhere as one unit; an invalid token
    /// changes and revokes nothing.
    #[sqlx::test]
    async fn reset_password_by_token_and_revoke_is_atomic(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();
        let app = test_app(&pool).await;
        crate::oauth::create_token(&pool, "sess1", app.id, Some(user.id), "read")
            .await
            .unwrap();
        set_reset_password_token(&pool, user.id, "tok")
            .await
            .unwrap();

        // An invalid token resets nothing and leaves the token live.
        assert!(
            reset_password_by_token_and_revoke(&pool, "wrong", "$argon2id$new")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(live_tokens(&pool, user.id).await, 1);

        // The live token resets the password, consumes the link, and signs out.
        let reset = reset_password_by_token_and_revoke(&pool, "tok", "$argon2id$new")
            .await
            .unwrap()
            .expect("the token is live");
        assert_eq!(reset.password_hash, "$argon2id$new");
        assert_eq!(live_tokens(&pool, user.id).await, 0);
        // Single-use: the token is gone.
        assert!(
            reset_password_by_token_and_revoke(&pool, "tok", "$argon2id$again")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The admin set-password / CLI `passwd` transition writes the
    /// credential and revokes every token in one transaction, and (when given
    /// one) records the moderator's audit line in the same unit.
    #[sqlx::test]
    async fn set_credentials_and_revoke_is_atomic_with_optional_audit(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();
        let app = test_app(&pool).await;
        crate::oauth::create_token(&pool, "sess1", app.id, Some(user.id), "read")
            .await
            .unwrap();

        let audit = crate::admin_action_log::NewActionLog {
            account_id,
            action: "set_password",
            target_type: "User",
            target_id: account_id,
            human_identifier: "@alice",
            permalink: None,
        };
        let updated =
            set_credentials_and_revoke(&pool, account_id, None, "$argon2id$new", Some(audit))
                .await
                .unwrap();
        assert_eq!(updated.password_hash, "$argon2id$new");
        assert_eq!(live_tokens(&pool, user.id).await, 0);
        let logged = crate::admin_action_log::list(
            &pool,
            &crate::admin_action_log::LogFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].action, "set_password");
    }

    /// The admin reset commits scramble + revoke + fresh reset
    /// token + durable mail as one unit. A late mail failure rolls the whole
    /// transition back, so the target is never locked out with no way back in.
    #[sqlx::test]
    async fn admin_reset_password_commits_all_four_or_rolls_back(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(
            &pool,
            account_id,
            Some("alice@example.com"),
            "$argon2id$old",
        )
        .await
        .unwrap();
        let app = test_app(&pool).await;
        crate::oauth::create_token(&pool, "sess1", app.id, Some(user.id), "read")
            .await
            .unwrap();

        // A late enqueue failure (a NUL byte the text column rejects, standing in
        // for any mail-insert failure) rolls the whole transition back.
        let poison = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Reset",
            body: "link\0nul",
        };
        assert!(
            admin_reset_password(&pool, user.id, "$argon2id$scrambled", "rtok", &poison)
                .await
                .is_err()
        );
        let after = find_by_id(&pool, user.id).await.unwrap().unwrap();
        assert_eq!(after.password_hash, "$argon2id$old", "password unchanged");
        assert_eq!(live_tokens(&pool, user.id).await, 1, "token still live");
        assert!(
            find_by_reset_password_token_hash(&pool, "rtok")
                .await
                .unwrap()
                .is_none(),
            "no reset token committed"
        );
        assert_eq!(
            crate::email::pending(&pool).await.unwrap(),
            0,
            "no mail queued"
        );

        // The good transition commits all four together.
        let good = crate::email::OutgoingEmail {
            recipient: "alice@example.com",
            subject: "Reset",
            body: "https://example.test/auth/password/edit?reset_password_token=rtok",
        };
        admin_reset_password(&pool, user.id, "$argon2id$scrambled", "rtok", &good)
            .await
            .unwrap();
        let after = find_by_id(&pool, user.id).await.unwrap().unwrap();
        assert_eq!(after.password_hash, "$argon2id$scrambled");
        assert_eq!(live_tokens(&pool, user.id).await, 0);
        assert!(
            find_by_reset_password_token_hash(&pool, "rtok")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(crate::email::pending(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn failed_sign_ins_lock_then_success_resets(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();

        for _ in 0..(LOGIN_LOCK_MAX_ATTEMPTS - 1) {
            assert!(!record_failed_sign_in(&pool, user.id).await.unwrap());
        }
        assert!(record_failed_sign_in(&pool, user.id).await.unwrap());
        assert!(login_locked(&pool, user.id).await.unwrap());

        sqlx::query!(
            "UPDATE users SET locked_at = now() - interval '2 hours' WHERE id = $1",
            user.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(!login_locked(&pool, user.id).await.unwrap());
        assert!(!record_failed_sign_in(&pool, user.id).await.unwrap());

        record_sign_in_with_ip(
            &pool,
            user.id,
            Some("ka"),
            Some("198.51.100.7"),
            SignInContext::default(),
        )
        .await
        .unwrap();
        let row = sqlx::query!(
            r#"
            SELECT failed_attempts, locked_at, current_sign_in_ip, sign_in_count, locale
            FROM users
            WHERE id = $1
            "#,
            user.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.failed_attempts, 0);
        assert!(row.locked_at.is_none());
        assert_eq!(row.current_sign_in_ip.as_deref(), Some("198.51.100.7"));
        assert_eq!(row.sign_in_count, 1);
        assert_eq!(row.locale.as_deref(), Some("ka"));
        let ip = sqlx::query_scalar!(
            "SELECT ip FROM login_activities WHERE user_id = $1",
            user.id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ip.as_deref(), Some("198.51.100.7"));
    }

    #[sqlx::test]
    async fn user_settings_roundtrip_defaults_missing_fields(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();
        let defaults = settings_by_user_id(&pool, user.id).await.unwrap().unwrap();
        assert_eq!(defaults, UserSettings::default());
        assert!(defaults.reading_allow_direct_remote_media);
        assert_eq!(defaults.resolved_visibility(false), "public");
        assert_eq!(defaults.default_language(), Some("en"));
        assert_eq!(defaults.timeline_order, TimelineOrder::Published);
        assert_eq!(defaults.thread_order, ThreadOrder::Tree);
        assert_eq!(
            defaults.posting_default_quote_policy,
            DefaultQuotePolicy::Public
        );
        assert!(defaults.show_application);
        // Boost collapse is on for a new reader (Mastodon's
        // `aggregate_reblogs` default), subject to the operator's switch.
        assert!(defaults.reading_collapse_boosts);

        let updated = UserSettings {
            posting_default_visibility: PostingDefaultVisibility::Unlisted,
            posting_default_sensitive: true,
            posting_default_language: " fr ".to_owned(),
            posting_default_quote_policy: DefaultQuotePolicy::Followers,
            posting_default_content_type: PostingDefaultFormat::Markdown,
            reading_expand_media: ReadingExpandMedia::ShowAll,
            reading_expand_spoilers: true,
            reading_autoplay_gifs: true,
            reading_allow_direct_remote_media: false,
            reading_collapse_boosts: false,
            reading_translate_language: Some(" de ".to_owned()),
            timeline_order: TimelineOrder::Received,
            thread_order: ThreadOrder::Flat,
            noindex: true,
            show_application: false,
            time_zone: None,
        };
        update_settings(&pool, user.id, updated)
            .await
            .unwrap()
            .unwrap();

        let stored = settings_by_account_id(&pool, account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.resolved_visibility(true), "unlisted");
        assert_eq!(stored.default_language(), Some("fr"));
        // Sanitized like the posting language; resolves as the target.
        assert_eq!(stored.reading_translate_language.as_deref(), Some("de"));
        assert_eq!(stored.translate_language(), "de");
        assert!(stored.posting_default_sensitive);
        assert_eq!(stored.reading_expand_media, ReadingExpandMedia::ShowAll);
        assert_eq!(stored.timeline_order, TimelineOrder::Received);
        assert_eq!(stored.thread_order, ThreadOrder::Flat);
        assert_eq!(
            stored.posting_default_quote_policy,
            DefaultQuotePolicy::Followers
        );
        assert!(!stored.reading_allow_direct_remote_media);
        assert!(!stored.show_application);
        assert!(!stored.reading_collapse_boosts);

        let updated = UserSettings {
            posting_default_visibility: PostingDefaultVisibility::Local,
            ..stored
        };
        update_settings(&pool, user.id, updated)
            .await
            .unwrap()
            .unwrap();
        let stored = settings_by_account_id(&pool, account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.resolved_visibility(false), "local");
    }

    #[sqlx::test]
    async fn posting_languages_roundtrip_and_empty_clears(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();

        // Fresh users carry no restriction.
        assert_eq!(posting_languages(&pool, user.id).await.unwrap(), None);

        let set = ["de".to_owned(), "en".to_owned()];
        assert!(
            update_posting_languages(&pool, user.id, Some(&set))
                .await
                .unwrap()
        );
        assert_eq!(
            posting_languages(&pool, user.id).await.unwrap().as_deref(),
            Some(&set[..])
        );

        // An empty set is normalized back to "no restriction".
        assert!(
            update_posting_languages(&pool, user.id, Some(&[]))
                .await
                .unwrap()
        );
        assert_eq!(posting_languages(&pool, user.id).await.unwrap(), None);

        // A missing user reports itself instead of pretending to save.
        assert!(
            !update_posting_languages(&pool, -1, Some(&set))
                .await
                .unwrap()
        );
        assert_eq!(posting_languages(&pool, -1).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn chosen_languages_roundtrip_and_empty_clears(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();

        // Fresh users read no reading-side filter.
        assert_eq!(chosen_languages(&pool, user.id).await.unwrap(), None);

        let set = ["de".to_owned(), "en".to_owned()];
        assert!(
            update_chosen_languages(&pool, user.id, Some(&set))
                .await
                .unwrap()
        );
        assert_eq!(
            chosen_languages(&pool, user.id).await.unwrap().as_deref(),
            Some(&set[..])
        );

        // An empty set is normalized back to "no filter".
        assert!(
            update_chosen_languages(&pool, user.id, Some(&[]))
                .await
                .unwrap()
        );
        assert_eq!(chosen_languages(&pool, user.id).await.unwrap(), None);

        // A missing user reports itself instead of pretending to save.
        assert!(
            !update_chosen_languages(&pool, -1, Some(&set))
                .await
                .unwrap()
        );
        assert_eq!(chosen_languages(&pool, -1).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn time_zone_roundtrip_and_blank_clears(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let user = create(&pool, account_id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap();

        assert_eq!(time_zone(&pool, user.id).await.unwrap(), None);

        assert!(
            update_time_zone(&pool, user.id, Some("Europe/London"))
                .await
                .unwrap()
        );
        assert_eq!(
            time_zone(&pool, user.id).await.unwrap().as_deref(),
            Some("Europe/London")
        );

        // A blank string clears back to the server default.
        assert!(update_time_zone(&pool, user.id, Some(" ")).await.unwrap());
        assert_eq!(time_zone(&pool, user.id).await.unwrap(), None);
    }
}
